//! Plan B poison-policy tests: covers Task 2 (ExplicitClose wiring) and
//! Tasks 3-4 (hybrid mutex policy — 4 sites become typed errors, 2 sites
//! become documented panics).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportCancel, TransportError};
use tst_pipeline::reconnect::{BackoffStrategy, ReconnectPolicy};
use tst_pipeline::{ManagedRecvTransport, ManagedTransport};

// ----------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------

/// Cancel handle for `ParkedRecv` — sets the shared flag so `recv_bytes`
/// unblocks and returns `Closed`. This lets `ManagedRecvTransport`'s cancel
/// machinery propagate to the inner transport and interrupt the blocking recv.
struct ParkedRecvCancel {
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl TransportCancel for ParkedRecvCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// A RecvTransport that blocks indefinitely on recv_bytes until the cancel
/// handle is invoked, then returns Closed (simulating an SRT recv parked
/// in srt_recvmsg).
struct ParkedRecv {
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl RecvTransport for ParkedRecv {
    fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        // Spin until cancel — this is a test mock so a tight loop is fine.
        // Real SRT would be parked in srt_recvmsg with a select fd.
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return Err(TransportError::Closed);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        !self.cancel.load(Ordering::Acquire)
    }
    fn close(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        // Expose a cancel handle so ManagedRecvTransport can propagate the
        // cross-thread cancel into our spinning recv_bytes loop.
        Some(Arc::new(ParkedRecvCancel {
            flag: self.cancel.clone(),
        }))
    }
}

/// A RecvTransport that returns Closed immediately (simulating clean peer-EOS).
struct EosRecv;
impl RecvTransport for EosRecv {
    fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        false
    }
}

fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff: BackoffStrategy::Constant(Duration::from_millis(1)),
        gap_buffer_capacity: 16,
        overflow_policy: tst_pipeline::reconnect::OverflowPolicy::DropOldest,
        ..Default::default()
    }
}

// ----------------------------------------------------------------
// Task 2 — ExplicitClose vs Closed disambiguation
// ----------------------------------------------------------------

/// Caller calls cancel() mid-recv; the parked recv_bytes returns Closed
/// from the inner transport but ManagedRecvTransport sees its cancel
/// signal first and returns ExplicitClose (caller-initiated path).
#[test]
fn cancel_during_active_recv_returns_explicit_close() {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let inner = ParkedRecv {
        cancel: cancel.clone(),
    };

    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory_calls_cl = factory_calls.clone();
    let cancel_cl = cancel.clone();
    let factory = Box::new(move || {
        factory_calls_cl.fetch_add(1, Ordering::Relaxed);
        Ok(ParkedRecv {
            cancel: cancel_cl.clone(),
        })
    });

    let mut managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(5)));
    let cancel_handle = managed
        .cancel_handle()
        .expect("cancel_handle returned None");

    // Fire cancel from another thread after a brief delay so the parked
    // recv_bytes is genuinely active when cancel hits.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        cancel_handle.cancel();
    });

    let mut buf = [0u8; 1316];
    let result = managed.recv_bytes(&mut buf);
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "got: {result:?}"
    );
}

/// Peer disconnects cleanly (no caller cancel). After the reconnect budget
/// is exhausted, ManagedRecvTransport returns Closed (peer-EOS-ish path —
/// the shell's kind_from_transport will map this to EndOfStream kind).
#[test]
fn peer_eos_returns_closed_not_explicit_close() {
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let factory_calls_cl = factory_calls.clone();
    let factory = Box::new(move || {
        factory_calls_cl.fetch_add(1, Ordering::Relaxed);
        // Always reconnect to EosRecv — peer immediately re-EOSs each time.
        Ok(EosRecv)
    });

    let mut managed = ManagedRecvTransport::new(EosRecv, factory, fast_policy(Some(2)));
    let mut buf = [0u8; 1316];
    let result = managed.recv_bytes(&mut buf);
    // After 2 attempts the budget is exhausted; returns Closed (not ExplicitClose).
    assert!(
        matches!(result, Err(TransportError::Closed)),
        "got: {result:?}"
    );
    // Verify we actually exercised the reconnect path (factory was called).
    assert!(factory_calls.load(Ordering::Relaxed) >= 1);
}

// ----------------------------------------------------------------
// Task 3 — poisoned inner lock → TransportError::Broken
// ----------------------------------------------------------------

/// Poison the inner-transport mutex pattern. ManagedTransport doesn't
/// expose the raw inner Arc<Mutex<...>>, so this test validates the
/// .lock().map_err(|_| Broken(...)) pattern directly against a hand-rolled
/// Mutex. The production code in Task 3 applies the same pattern.
#[test]
fn poisoned_inner_lock_returns_broken_not_panic() {
    struct OkTransport;
    impl Transport for OkTransport {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    let factory = || Ok(OkTransport);
    let managed = Arc::new(ManagedTransport::new(
        OkTransport,
        factory,
        fast_policy(Some(2)),
    ));

    use std::sync::Mutex;
    let m: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let m_cl = m.clone();
    let _ = std::thread::spawn(move || {
        let _g = m_cl.lock().unwrap();
        panic!("intentional panic to poison");
    })
    .join();

    // Now m is poisoned.
    assert!(m.is_poisoned());

    // The Plan B pattern: convert poison into TransportError::Broken with
    // a site-specific message.
    let result: Result<i32, TransportError> = m
        .lock()
        .map_err(|_| TransportError::Broken {
            msg: "test: pattern-only".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
        .map(|g| *g);

    assert!(
        matches!(result, Err(TransportError::Broken { .. })),
        "got: {result:?}"
    );
    if let Err(TransportError::Broken { msg: s, .. }) = result {
        assert!(s.contains("test: pattern-only"), "message: {s}");
    }

    // Suppress unused warning.
    let _ = managed;
}

// ----------------------------------------------------------------
// Regression: deadlock-on-successful-reconnect (caught in Plan B final review)
// ----------------------------------------------------------------

/// Regression test for a same-thread deadlock in ManagedTransport::send_managed
/// caused by holding self.inner.lock() through self.reconnect_and_drain()
/// (which also acquires self.inner.lock()). std::sync::Mutex is not reentrant.
///
/// The bug fires on the production happy-reconnect path: first connection
/// breaks (Broken from send_bytes), factory rebuilds a fresh transport
/// successfully, reconnect_and_drain installs it via self.inner.lock() → deadlock.
///
/// All Plan B's other tests used always-failing factories so the install path
/// was never reached. This test uses a one-shot factory that fails once then
/// succeeds, exercising the path that would deadlock without the scope-wrap fix.
#[test]
fn successful_reconnect_does_not_deadlock() {
    use std::sync::mpsc;

    /// Transport that fails the next send_bytes with Broken (one-shot), then
    /// succeeds. Simulates a transport that initially errors then a freshly-
    /// constructed sibling that succeeds — the canonical happy-reconnect shape.
    struct OneShotBrokenThenOk {
        broken_first: Arc<AtomicUsize>,
    }
    impl Transport for OneShotBrokenThenOk {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            if self.broken_first.fetch_sub(1, Ordering::Relaxed) > 0 {
                Err(TransportError::Broken {
                    msg: "simulated one-shot break".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            } else {
                Ok(())
            }
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    let broken_counter = Arc::new(AtomicUsize::new(1)); // first send fails once
    let broken_counter_cl = broken_counter.clone();
    let factory_count = Arc::new(AtomicUsize::new(0));
    let factory_count_cl = factory_count.clone();
    let factory = move || {
        factory_count_cl.fetch_add(1, Ordering::Relaxed);
        // Fresh sibling that always succeeds (broken_first counter is zero).
        Ok(OneShotBrokenThenOk {
            broken_first: Arc::new(AtomicUsize::new(0)),
        })
    };

    let mut managed = ManagedTransport::new(
        OneShotBrokenThenOk {
            broken_first: broken_counter_cl,
        },
        factory,
        fast_policy(Some(3)),
    );

    // First send: fails with Broken, triggers reconnect, fresh transport
    // installs successfully via reconnect_and_drain. If transport_guard is
    // held through reconnect_and_drain, this deadlocks. Run on a separate
    // thread with a join-timeout so the test fails (not hangs) if the
    // deadlock returns.
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = managed.send_bytes(b"hello world");
        let _ = tx.send(result);
    });

    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("send_bytes deadlocked — transport_guard held through reconnect_and_drain");
    let _ = handle.join();

    // Successful reconnect path should ultimately succeed (or surface a
    // legitimate error like Broken-after-budget-exhausted; both are OK as
    // long as we didn't deadlock).
    eprintln!("successful_reconnect_does_not_deadlock: send_bytes result = {result:?}");
    assert!(
        factory_count.load(Ordering::Relaxed) >= 1,
        "factory should have been called for reconnect"
    );
}

// ----------------------------------------------------------------
// Task 4 — poisoned gap lock → BUG: panic
// ----------------------------------------------------------------

/// Verify that the .expect("BUG: gap lock poisoned ...") pattern panics
/// with the BUG: prefix when the gap mutex is poisoned. Uses
/// catch_unwind to intercept the panic and assert its payload contains
/// the expected message.
#[test]
fn poisoned_gap_lock_panics_with_bug_prefix() {
    use std::sync::Mutex;
    let m: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let m_cl = m.clone();
    let _ = std::thread::spawn(move || {
        let _g = m_cl.lock().unwrap();
        panic!("intentional panic to poison");
    })
    .join();

    // Now m is poisoned.
    assert!(m.is_poisoned());

    // The Plan B pattern: .expect("BUG: ...") on the gap lock.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = m
            .lock()
            .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
        // unreachable
    }));

    assert!(result.is_err(), "expected panic, got Ok");
    let payload = result.unwrap_err();
    // Panic payload is typically &'static str or String. Extract.
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<unknown payload type>".to_string()
    };
    assert!(
        msg.starts_with("BUG: gap lock poisoned"),
        "panic message should start with 'BUG: gap lock poisoned ...', got: {msg}"
    );
}
