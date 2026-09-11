//! Validate-1 C2 (Codex PIPE-01): `OverflowPolicy::Reject` must surface
//! `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` to the caller when
//! the gap buffer fills during a reconnect, not silently drop bytes.
//!
//! Before the C2 fix, `ManagedTransport::send_managed` discarded the
//! `Err(GapBufferError::Full)` result with `let _ = gap.enqueue(...);`,
//! violating the documented contract of `OverflowPolicy::Reject` ("refuse
//! to enqueue; return an error to the caller"). After the fix, the error
//! propagates as `TransportError::Backpressure` — the shells map it to
//! `ShellErrorKind::Backpressure` and `tst-c` to `TST_E_BUFFER_FULL`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tst_pipeline::reconnect::OverflowPolicy;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, ReconnectPolicy, Transport, TransportError,
};

/// Mock `Transport` whose every `send_bytes` returns `Broken` so the
/// `ManagedTransport` decorator is forced into the reconnect path, where
/// new bytes get queued into the gap buffer.
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
        true
    }

    fn close(&mut self) {}
}

/// With `OverflowPolicy::Reject` + capacity 1, the second send must
/// surface `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` rather than
/// silently dropping the new bytes.
#[test]
fn reject_policy_surfaces_backpressure_when_gap_full() {
    let factory = || -> Result<AlwaysBroken, TransportError> {
        Err(TransportError::Broken {
            msg: "factory always fails".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    };
    // max_attempts: Some(1) + zero backoff keeps each send fast: one
    // factory call, give up, return Broken. With our fix, the second
    // send refuses BEFORE reconnect, so it returns Backpressure.
    let policy = ReconnectPolicy {
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 1,
        overflow_policy: OverflowPolicy::Reject,
        ..Default::default()
    };

    let inner = AlwaysBroken {
        sends: Arc::new(AtomicU32::new(0)),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy);

    // First send: transport returns Broken, bytes enqueue successfully
    // (buffer was empty), reconnect attempt fails, ManagedTransport
    // returns Broken("reconnect gave up..."). The byte stays queued.
    let first = managed.send_bytes(b"first").unwrap_err();
    assert!(
        matches!(first, TransportError::Broken { .. }),
        "first send: expected Broken after give-up, got {first:?}"
    );

    // Second send: transport (rebuilt as None after first failed
    // reconnect cycle) is dead; we fall through to the enqueue path.
    // Buffer is full from the first byte. With Reject policy, the new
    // bytes MUST surface as Backpressure rather than be silently dropped.
    let second = managed.send_bytes(b"second").unwrap_err();
    assert!(
        matches!(second, TransportError::Backpressure { ref msg, .. } if msg.contains("gap buffer full")),
        "second send: expected Backpressure(\"gap buffer full\"), got {second:?}"
    );
}

/// Counter-test: `OverflowPolicy::DropOldest` must NOT surface
/// `Backpressure` — the contract there is "evict oldest, accept new."
/// This guards against an over-eager fix that surfaces overflow on every
/// policy.
#[test]
fn drop_oldest_policy_does_not_surface_backpressure_on_overflow() {
    let factory = || -> Result<AlwaysBroken, TransportError> {
        Err(TransportError::Broken {
            msg: "factory always fails".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    };
    let policy = ReconnectPolicy {
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 1,
        overflow_policy: OverflowPolicy::DropOldest,
        ..Default::default()
    };

    let inner = AlwaysBroken {
        sends: Arc::new(AtomicU32::new(0)),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy);

    // First send: Broken → enqueue → reconnect fails → Broken give-up.
    let first = managed.send_bytes(b"first").unwrap_err();
    assert!(matches!(first, TransportError::Broken { .. }));

    // Second send: buffer is full from "first", DropOldest evicts it,
    // enqueues "second", reconnect fails, returns Broken (not
    // Backpressure). The eviction is silent per the policy contract.
    let second = managed.send_bytes(b"second").unwrap_err();
    assert!(
        matches!(second, TransportError::Broken { .. }),
        "DropOldest must NOT surface Backpressure on overflow, got {second:?}"
    );
}
