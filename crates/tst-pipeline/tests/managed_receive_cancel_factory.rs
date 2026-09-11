//! A cancel must reach a `ManagedRecvTransport` whose reconnect is parked
//! INSIDE the factory — the listener-mode re-accept case (ROADMAP Apple
//! rider 2). The factory publishes the handle that can wake it through a
//! `FactoryCancel` slot; the managed transport's own cancel fires that
//! slot, and the factory then reports `ExplicitClose`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tst_core::transport::{BrokenCause, RecvTransport, TransportCancel, TransportError};
use tst_pipeline::{BackoffStrategy, FactoryCancel, ManagedRecvTransport, ReconnectPolicy};

/// Inner transport that is dead on arrival: the first recv reports
/// `Broken`, which sends the managed transport into its reconnect loop.
struct DeadInner;

impl RecvTransport for DeadInner {
    fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        Err(TransportError::Broken {
            msg: "dead on arrival".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        false
    }
}

/// Stand-in for a listener's cancel handle: `cancel()` releases whoever
/// is parked on the paired receiver, exactly as `Listener::cancel_handle`
/// releases a parked `accept()`.
struct ParkedAccept {
    wake: Mutex<Option<mpsc::Sender<()>>>,
}

impl TransportCancel for ParkedAccept {
    fn cancel(&self) {
        if let Some(tx) = self.wake.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

#[test]
fn cancel_wakes_a_factory_parked_on_its_installed_handle() {
    let factory_cancel = Arc::new(FactoryCancel::new());
    let woken_by_cancel = Arc::new(AtomicU32::new(0));

    let fc = Arc::clone(&factory_cancel);
    let woken = Arc::clone(&woken_by_cancel);
    let factory: Box<dyn FnMut() -> Result<DeadInner, TransportError> + Send> =
        Box::new(move || {
            let (tx, rx) = mpsc::channel::<()>();
            let handle: Arc<dyn TransportCancel + Send + Sync> = Arc::new(ParkedAccept {
                wake: Mutex::new(Some(tx)),
            });
            fc.install(handle);
            // Park like a listener in accept(): only the installed handle's
            // cancel() can release us. A 5 s cap keeps a regression from
            // hanging the test binary.
            let released = rx.recv_timeout(Duration::from_secs(5)).is_ok();
            fc.clear();
            if released {
                woken.fetch_add(1, Ordering::SeqCst);
            }
            // What a cancellable listen helper returns once it has been
            // woken by a cancel rather than by a peer.
            Err(TransportError::ExplicitClose)
        });

    let policy = ReconnectPolicy {
        max_attempts: Some(5),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    };
    let mut managed =
        ManagedRecvTransport::new_with_factory_cancel(DeadInner, factory, policy, factory_cancel);
    let cancel = managed.cancel_handle().expect("managed cancel handle");

    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
    });

    let start = Instant::now();
    let mut buf = [0u8; 1316];
    let result = managed.recv_bytes(&mut buf);
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "expected ExplicitClose after a cross-thread cancel, got {result:?}"
    );
    assert_eq!(
        woken_by_cancel.load(Ordering::SeqCst),
        1,
        "the factory was not released through its installed cancel handle"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "cancel took {elapsed:?} to reach the parked factory"
    );
    assert!(
        !managed.is_alive(),
        "managed transport must latch closed after cancel"
    );
}

/// Both ends of the mid-factory race in one type, because
/// `ManagedRecvTransport<R>` rebuilds its inner from the same `R`:
/// `dead: true` breaks on the first read and sends the wrapper into its
/// reconnect loop; `dead: false` is the healthy connection the factory
/// hands back *after* the cancel already landed. It would happily serve
/// bytes — only the wrapper can decide not to read from it. Its
/// `cancel_handle()` latches `cancelled`, which is what the test checks
/// the wrapper fired.
struct RaceInner {
    cancelled: Arc<AtomicBool>,
    dead: bool,
}

impl RecvTransport for RaceInner {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.dead {
            return Err(TransportError::Broken {
                msg: "dead on arrival".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        buf[0] = 1;
        Ok(1)
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        !self.dead && !self.cancelled.load(Ordering::SeqCst)
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(FlagCancel(Arc::clone(&self.cancelled))))
    }
}

struct FlagCancel(Arc<AtomicBool>);

impl TransportCancel for FlagCancel {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// CORR-02: a cancel that lands *while the factory runs* must not be lost.
/// The factory here succeeds after the cancel, so the wrapper holds a
/// perfectly healthy fresh connection that the caller has already asked it
/// to abandon: it must fire that connection's wake handle and report the
/// caller-initiated close instead of delivering its bytes.
#[test]
fn factory_success_after_cancel_is_not_delivered() {
    let fresh_flag = Arc::new(AtomicBool::new(false));
    // The managed transport's own cancel handle, handed to the factory
    // closure through a cell because it only exists after construction.
    let handle_cell: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>> =
        Arc::new(Mutex::new(None));

    let cell = Arc::clone(&handle_cell);
    let flag = Arc::clone(&fresh_flag);
    let factory: Box<dyn FnMut() -> Result<RaceInner, TransportError> + Send> =
        Box::new(move || {
            // Cancel lands while the factory is "accepting": after this
            // returns, the wrapper must NOT read from the fresh connection.
            cell.lock()
                .unwrap()
                .as_ref()
                .expect("cancel handle installed before the first recv")
                .cancel();
            Ok(RaceInner {
                cancelled: Arc::clone(&flag),
                dead: false,
            })
        });

    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    };
    // The initial inner carries its OWN flag: `fresh_flag` must only be
    // reachable through the connection the factory built, or the assertion
    // below would pass on the pre-fix code.
    let initial = RaceInner {
        cancelled: Arc::new(AtomicBool::new(false)),
        dead: true,
    };
    let mut managed = ManagedRecvTransport::new(initial, factory, policy);
    *handle_cell.lock().unwrap() = managed.cancel_handle();

    let mut buf = [0u8; 16];
    let result = managed.recv_bytes(&mut buf);

    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "bytes from a connection built after the cancel were delivered: {result:?}"
    );
    assert!(
        fresh_flag.load(Ordering::SeqCst),
        "the fresh inner's cancel handle must have fired"
    );
    assert!(
        !managed.is_alive(),
        "managed transport must latch closed after cancel"
    );
}

#[test]
fn install_after_cancel_fires_the_handle_immediately() {
    // The race the slot must close: the cancel lands between the factory's
    // bind and its install. Installing into an already-cancelled slot must
    // fire the handle at once, never leave the factory parked.
    let factory_cancel = FactoryCancel::new();
    factory_cancel.cancel();
    assert!(factory_cancel.is_cancelled());

    let (tx, rx) = mpsc::channel::<()>();
    let handle: Arc<dyn TransportCancel + Send + Sync> = Arc::new(ParkedAccept {
        wake: Mutex::new(Some(tx)),
    });
    factory_cancel.install(handle);
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_ok(),
        "install() into a cancelled slot did not fire the handle"
    );
}
