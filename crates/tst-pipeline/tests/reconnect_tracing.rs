//! Verifies `tracing` instrumentation on the sender-side reconnect loop
//! (`ManagedTransport::reconnect_and_drain`).
//!
//! Uses `tracing-test` to assert that the documented log emissions
//! (info on each reconnect attempt, warn on terminal give-up) actually
//! fire during a real reconnect-and-give-up flow.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tracing_test::traced_test;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, ReconnectPolicy, Transport, TransportError,
};

/// Mock `Transport` whose every `send_bytes` returns `Broken` so the
/// `ManagedTransport` decorator is forced into the reconnect path.
struct AlwaysBroken {
    sends: Arc<AtomicU32>,
}

impl Transport for AlwaysBroken {
    fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Err(TransportError::Broken {
            msg: "always broken (test)".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        // Returning `true` keeps `drain_gap_if_alive` willing to attempt
        // the inner; the broken `send_bytes` then triggers reconnect.
        true
    }

    fn close(&mut self) {}
}

#[traced_test]
#[test]
fn sender_reconnect_emits_info_on_attempt_and_warn_on_give_up() {
    // Factory always fails so the reconnect budget is exhausted and the
    // give-up branch fires.
    let factory = || -> Result<AlwaysBroken, TransportError> {
        Err(TransportError::Broken {
            msg: "test factory always fails".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    };

    // Cap attempts low + zero backoff so the test is fast.
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    };

    let inner = AlwaysBroken {
        sends: Arc::new(AtomicU32::new(0)),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy);

    // Drive the reconnect path: a single send triggers Broken → enqueue →
    // reconnect_and_drain → 3 factory failures → give-up.
    let err = managed.send_bytes(b"trigger").unwrap_err();
    assert!(
        matches!(err, TransportError::Broken { .. }),
        "expected Broken after give-up, got {err:?}"
    );

    // INFO  "reconnect attempt"   (one per attempt, 3 total)
    // WARN  "reconnect gave up"   (one terminal)
    assert!(
        logs_contain("reconnect attempt"),
        "expected at least one INFO 'reconnect attempt' event"
    );
    assert!(
        logs_contain("reconnect gave up"),
        "expected a WARN 'reconnect gave up' event after exhausting the budget"
    );
}
