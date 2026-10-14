//! Verifies `tracing` instrumentation on the sender-side reconnect loop
//! (`ManagedTransport::reconnect_and_drain`).
//!
//! Uses `tracing-test` to assert that the documented log emissions
//! (info on each reconnect attempt, warn on terminal give-up) actually
//! fire during a real reconnect-and-give-up flow.

mod common;

use crate::common::{ScriptedSend, broken};

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;
use tracing_test::traced_test;
use tst_pipeline::{BackoffStrategy, ManagedTransport, ReconnectPolicy, Transport, TransportError};

#[traced_test]
#[test]
fn sender_reconnect_emits_info_on_attempt_and_warn_on_give_up() {
    // Factory always fails so the reconnect budget is exhausted and the
    // give-up branch fires.
    let factory =
        || -> Result<ScriptedSend, TransportError> { Err(broken("test factory always fails")) };

    // Cap attempts low + zero backoff so the test is fast.
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    };

    let inner = ScriptedSend::always(broken("always broken (test)"))
        .with_counter(Arc::new(AtomicU32::new(0)));
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
