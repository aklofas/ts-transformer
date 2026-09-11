//! Send-side mirror of `managed_receive_cancel_factory.rs`: a cancel that
//! lands while `ManagedTransport`'s reconnect factory is running must not be
//! lost. The fresh inner the factory hands back is installed into an
//! already-cancelled `CancelSlot` (which fires its wake handle on arrival),
//! and the wrapper must then report the caller-initiated close instead of
//! draining the gap buffer through a connection the caller already asked it
//! to abandon.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tst_core::transport::{BrokenCause, Transport, TransportCancel, TransportError};
use tst_pipeline::{BackoffStrategy, ManagedTransport, ReconnectPolicy};

/// What the mock reports as `max_payload`: one SRT TS bundle (7 × 188-byte
/// packets), the ceiling the real transports advertise — large enough that
/// the wrapper's size pre-check never rejects the test's single packet.
const MAX_PAYLOAD: usize = 7 * 188;

/// Both ends of the mid-factory race in one type, because
/// `ManagedTransport<T>` rebuilds its inner from the same `T`: `dead: true`
/// breaks on the first send and sends the wrapper into its reconnect loop;
/// `dead: false` is the healthy connection the factory hands back *after*
/// the cancel already landed. It would happily accept bytes — only the
/// wrapper can decide not to write to it. Its `cancel_handle()` latches
/// `cancelled` (what the test checks the wrapper fired) and `delivered`
/// counts every send that reached it (what the test checks stayed at zero).
struct RaceInner {
    cancelled: Arc<AtomicBool>,
    delivered: Arc<AtomicU32>,
    dead: bool,
}

impl Transport for RaceInner {
    fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
        if self.dead {
            return Err(TransportError::Broken {
                msg: "dead on arrival".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        // Deliberately ignores `cancelled`: a real socket would fail here
        // after its cancel, which would let the wrapper's drain surface a
        // wire-looking `Broken` for a caller-initiated close. Accepting the
        // bytes instead makes a wrapper that drains after the cancel visible
        // as "delivered", not just mis-reported.
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn max_payload(&self) -> usize {
        MAX_PAYLOAD
    }

    fn is_alive(&self) -> bool {
        !self.dead && !self.cancelled.load(Ordering::SeqCst)
    }

    fn close(&mut self) {}

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

/// A cancel that lands *while the factory runs* must not be lost. The
/// factory here succeeds after the cancel, so the wrapper holds a perfectly
/// healthy fresh connection that the caller has already asked it to abandon:
/// it must fire that connection's wake handle (install-after-cancel) and
/// report the caller-initiated close, never drain the gap buffer into it.
#[test]
fn factory_success_after_cancel_is_not_drained_into() {
    let fresh_flag = Arc::new(AtomicBool::new(false));
    let fresh_delivered = Arc::new(AtomicU32::new(0));
    // The managed transport's own cancel handle, handed to the factory
    // closure through a cell because it only exists after construction.
    let handle_cell: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>> =
        Arc::new(Mutex::new(None));

    let cell = Arc::clone(&handle_cell);
    let flag = Arc::clone(&fresh_flag);
    let delivered = Arc::clone(&fresh_delivered);
    let factory = move || {
        // Cancel lands while the factory is "dialling": after this returns,
        // the wrapper must NOT write to the fresh connection.
        cell.lock()
            .unwrap()
            .as_ref()
            .expect("cancel handle installed before the first send")
            .cancel();
        Ok(RaceInner {
            cancelled: Arc::clone(&flag),
            delivered: Arc::clone(&delivered),
            dead: false,
        })
    };

    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    };
    // The initial inner carries its OWN flag and counter: `fresh_*` must
    // only be reachable through the connection the factory built, or the
    // assertions below would pass on the pre-fix code.
    let initial = RaceInner {
        cancelled: Arc::new(AtomicBool::new(false)),
        delivered: Arc::new(AtomicU32::new(0)),
        dead: true,
    };
    let mut managed = ManagedTransport::new(initial, factory, policy);
    *handle_cell.lock().unwrap() = managed.cancel_handle();

    let result = managed.send_bytes(&[0x47; 188]);

    assert!(
        matches!(result, Err(TransportError::Closed)),
        "a cancel that landed mid-factory must surface as the caller-initiated close, got {result:?}"
    );
    assert!(
        fresh_flag.load(Ordering::SeqCst),
        "the fresh inner's cancel handle must have fired (install-after-cancel)"
    );
    assert_eq!(
        fresh_delivered.load(Ordering::SeqCst),
        0,
        "the gap buffer was drained into a connection the caller had already cancelled"
    );
    assert!(
        !managed.is_alive(),
        "managed transport must latch closed after cancel"
    );
}
