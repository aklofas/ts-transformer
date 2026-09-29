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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tst_pipeline::reconnect::OverflowPolicy;
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedTransport, ReconnectMode, ReconnectPolicy, Transport,
    TransportError,
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

/// With `OverflowPolicy::Reject` and no room in the buffer, a send must
/// surface `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` rather than
/// silently dropping the new bytes.
///
/// Capacity 0: in `Blocking` mode a send that fails takes its message back
/// out of the buffer, so a failed send cannot be what fills it.
#[test]
fn reject_policy_surfaces_backpressure_when_gap_full() {
    let factory = || -> Result<AlwaysBroken, TransportError> {
        Err(TransportError::Broken {
            msg: "factory always fails".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    };
    // The buffer refuses BEFORE any reconnect is attempted.
    let policy = ReconnectPolicy {
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 0,
        overflow_policy: OverflowPolicy::Reject,
        ..Default::default()
    };

    let inner = AlwaysBroken {
        sends: Arc::new(AtomicU32::new(0)),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy);

    // The transport returns Broken and the bytes have nowhere to queue.
    // With Reject policy they MUST surface as Backpressure rather than be
    // silently dropped.
    let err = managed.send_bytes(b"first").unwrap_err();
    assert!(
        matches!(err, TransportError::Backpressure { ref msg, .. } if msg.contains("gap buffer full")),
        "expected Backpressure(\"gap buffer full\"), got {err:?}"
    );
}

/// Inner for the overflow counter-test: the initial one breaks on every
/// send, the one the factory builds records what it is given.
struct Link {
    broken: bool,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Transport for Link {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if self.broken {
            return Err(TransportError::Broken {
                msg: "broken (test)".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        self.delivered.lock().unwrap().push(msg.to_vec());
        Ok(())
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        !self.broken
    }

    fn close(&mut self) {}
}

/// Holds the reconnect factory until the test has filled and overflowed the
/// buffer. `entered` latches once the worker is inside the factory.
#[derive(Default)]
struct Barrier {
    state: Mutex<(bool, bool)>, // (entered, released)
    cv: Condvar,
}

impl Barrier {
    /// Park until released. Gives up after `RESCUE` so a failing test never
    /// leaves the worker parked for good.
    fn hold(&self) {
        let mut st = self.state.lock().unwrap();
        st.0 = true;
        self.cv.notify_all();
        let deadline = Instant::now() + RESCUE;
        while !st.1 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            st = self.cv.wait_timeout(st, left).unwrap().0;
        }
    }

    fn wait_entered(&self) -> bool {
        let st = self.state.lock().unwrap();
        let (st, _) = self.cv.wait_timeout_while(st, RESCUE, |s| !s.0).unwrap();
        st.0
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
}

/// Releases the barrier when the test ends, however it ends.
struct ReleaseOnDrop(Arc<Barrier>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

const RESCUE: Duration = Duration::from_secs(10);

/// Counter-test: `OverflowPolicy::DropOldest` must NOT surface
/// `Backpressure` — the contract there is "evict oldest, accept new."
/// This guards against an over-eager fix that surfaces overflow on every
/// policy.
///
/// `Background` mode, because only there does a message outlive the call
/// that queued it: the worker is held inside the factory, so the outage
/// lasts for as long as the test needs to fill the buffer and then send one
/// message more than it holds.
#[test]
fn drop_oldest_policy_does_not_surface_backpressure_on_overflow() {
    let delivered: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::default());
    let _release = ReleaseOnDrop(Arc::clone(&barrier));
    let factory = {
        let barrier = Arc::clone(&barrier);
        let delivered = Arc::clone(&delivered);
        move || -> Result<Link, TransportError> {
            barrier.hold();
            Ok(Link {
                broken: false,
                delivered: Arc::clone(&delivered),
            })
        }
    };
    let policy = ReconnectPolicy {
        max_attempts: None,
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 2,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Background,
    };
    let inner = Link {
        broken: true,
        delivered: Arc::clone(&delivered),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy);
    let stats = managed.stats_handle();

    // Breaks the inner: queued, and the worker goes to the factory.
    managed.send_bytes(b"first").expect("accepted");
    assert!(
        barrier.wait_entered(),
        "the worker never reached the factory"
    );
    managed.send_bytes(b"second").expect("accepted");
    let full = stats.stats().expect("gap lock not poisoned");
    assert_eq!(
        (full.gap_len, full.gap_messages_dropped),
        (2, 0),
        "precondition: the buffer is full and nothing was evicted yet"
    );

    // The overflow: one more than the buffer holds.
    let third = managed.send_bytes(b"third");
    assert!(
        third.is_ok(),
        "DropOldest must NOT surface Backpressure on overflow, got {third:?}"
    );
    let after = stats.stats().expect("gap lock not poisoned");
    assert_eq!(after.gap_len, 2, "still within capacity");
    assert_eq!(
        (after.gap_messages_dropped, after.gap_bytes_dropped),
        (1, b"first".len() as u64),
        "the oldest message was evicted, and counted"
    );

    barrier.release();
    let deadline = Instant::now() + RESCUE;
    while delivered.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *delivered.lock().unwrap(),
        vec![b"second".to_vec(), b"third".to_vec()],
        "the evicted message is never delivered; the survivors are, in order"
    );
}
