//! Verifies `tracing` instrumentation on the receiver-side reconnect loop
//! (`ManagedRecvTransport::recv_bytes`).
//!
//! Mirror of `reconnect_tracing.rs` for the receive side. Uses
//! `tracing-test` to assert that the documented log emissions (info on
//! each reconnect attempt, warn on terminal give-up) actually fire
//! during a real reconnect-and-give-up flow on the receive side.

use crate::common::{ScriptedRecv, broken};

use std::time::Duration;
use tracing_test::traced_test;
use tst_pipeline::{
    BackoffStrategy, ManagedRecvTransport, ReconnectMode, ReconnectPolicy, RecvTransport,
    TransportError,
};

#[traced_test]
#[test]
fn receiver_reconnect_emits_info_on_attempt_and_warn_on_give_up() {
    // Factory always fails so the reconnect budget is exhausted and the
    // give-up branch fires.
    let factory = Box::new(|| -> Result<ScriptedRecv, TransportError> {
        Err(broken("test factory always fails"))
    });

    // Cap attempts low + zero backoff so the test is fast.
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    };

    let inner = ScriptedRecv::failing(broken("always broken (test)")).with_alive(true);
    let mut managed = ManagedRecvTransport::new(inner, factory, policy);

    // Drive the reconnect path: a single recv triggers Broken → drop inner →
    // factory failures within the policy budget → give-up returns Closed.
    let mut buf = [0u8; 8];
    let err = managed.recv_bytes(&mut buf).unwrap_err();
    assert_eq!(
        err,
        TransportError::Closed,
        "expected Closed after give-up, got {err:?}"
    );

    // INFO  "reconnect attempt"   (one per attempt)
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

/// `ReconnectMode::Background` is send-side only (no gap buffer, no
/// worker thread on the receive side). Construction with `mode:
/// Background` must succeed and log a one-shot warning, and the actual
/// reconnect behavior must be indistinguishable from `Blocking`: the
/// same synchronous give-up-after-budget flow the test above exercises.
#[traced_test]
#[test]
fn receiver_background_mode_warns_and_behaves_as_blocking() {
    let factory = Box::new(|| -> Result<ScriptedRecv, TransportError> {
        Err(broken("test factory always fails"))
    });

    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        mode: ReconnectMode::Background,
        ..Default::default()
    };

    let inner = ScriptedRecv::failing(broken("always broken (test)")).with_alive(true);
    let mut managed = ManagedRecvTransport::new(inner, factory, policy);

    // The construction-time warning fires immediately, regardless of
    // whether recv_bytes is ever called.
    assert!(
        logs_contain("ReconnectMode::Background is send-side only"),
        "expected the send-side-only warning at construction"
    );

    // Behavior is unchanged from Blocking: recv_bytes reconnects
    // synchronously on the caller's thread until the budget exhausts,
    // then returns Closed. There is no enqueue-and-return-Ok gap-buffer
    // path — the receive side has no gap buffer at all.
    let mut buf = [0u8; 8];
    let err = managed.recv_bytes(&mut buf).unwrap_err();
    assert_eq!(
        err,
        TransportError::Closed,
        "Background must behave exactly like Blocking on the recv side"
    );
    assert!(
        logs_contain("reconnect gave up"),
        "give-up flow must be identical to the Blocking test above"
    );
}
