//! `OverflowPolicy::Reject` must surface
//! `TransportError::Backpressure { msg: "gap buffer full", errno_code: None }` to the caller when
//! the gap buffer fills during a reconnect, not silently drop bytes.
//!
//! A `ManagedTransport::send_managed` that discards the
//! `Err(GapBufferError::Full)` result violates the documented contract of
//! `OverflowPolicy::Reject` ("refuse to enqueue; return an error to the
//! caller"). The error propagates as `TransportError::Backpressure` — the
//! shells map it to `ShellErrorKind::Backpressure` and `tst-c` to
//! `TST_E_BUFFER_FULL`.

use crate::common::{ScriptedSend, SendStep, broken, wait_for};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tst_core::transport::TransportCancel;
use tst_pipeline::reconnect::OverflowPolicy;
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, ReconnectMode, ReconnectPolicy, Transport, TransportError,
};

/// A capacity-0 wrapper whose initial inner breaks on its first send. The
/// factory counts its calls, refuses the first `failing_calls` of them and
/// then hands back inners that follow `fresh_script` (empty: every send is
/// delivered to `wire`).
struct ZeroRig {
    wire: Arc<Mutex<Vec<Vec<u8>>>>,
    factory_calls: Arc<AtomicU32>,
    fresh_script: Arc<Mutex<VecDeque<SendStep>>>,
}

fn capacity_zero(
    mode: ReconnectMode,
    overflow: OverflowPolicy,
    max_attempts: u32,
    failing_calls: u32,
) -> (ManagedTransport<ScriptedSend>, ZeroRig) {
    let wire = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::new(AtomicU32::new(0));
    let fresh_script: Arc<Mutex<VecDeque<SendStep>>> = Arc::default();
    let initial = ScriptedSend::scripted(
        Arc::new(Mutex::new(VecDeque::from([SendStep::Broken]))),
        Arc::clone(&wire),
    );
    let factory = {
        let wire = Arc::clone(&wire);
        let calls = Arc::clone(&factory_calls);
        let fresh_script = Arc::clone(&fresh_script);
        move || -> Result<ScriptedSend, TransportError> {
            if calls.fetch_add(1, Ordering::SeqCst) < failing_calls {
                return Err(broken("factory down (test)"));
            }
            Ok(ScriptedSend::scripted(
                Arc::clone(&fresh_script),
                Arc::clone(&wire),
            ))
        }
    };
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(max_attempts))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(1)))
        .with_gap_buffer_capacity(0)
        .with_overflow_policy(overflow)
        .with_mode(mode);
    (
        ManagedTransport::new(initial, factory, policy).expect("the policy validates"),
        ZeroRig {
            wire,
            factory_calls,
            fresh_script,
        },
    )
}

impl ZeroRig {
    fn calls(&self) -> u32 {
        self.factory_calls.load(Ordering::SeqCst)
    }

    fn wire(&self) -> Vec<Vec<u8>> {
        self.wire.lock().unwrap().clone()
    }
}

fn is_gap_full(r: &Result<(), TransportError>) -> bool {
    matches!(r, Err(TransportError::Backpressure { msg, .. }) if msg.contains("gap buffer full"))
}

/// With `OverflowPolicy::Reject` and no room in the buffer, a `Background`
/// send must surface `TransportError::Backpressure { msg: "gap buffer full",
/// errno_code: None }` rather than silently dropping the new bytes. The
/// refusal is about admission only: the broken connection is still
/// re-dialled.
#[test]
fn reject_policy_surfaces_backpressure_when_gap_full() {
    // One attempt, and the factory refuses it.
    let (mut managed, rig) = capacity_zero(
        ReconnectMode::Background,
        OverflowPolicy::Reject,
        1,
        u32::MAX,
    );
    let stats = managed.stats_handle();

    let first = managed.send_bytes(b"first");
    assert!(
        is_gap_full(&first),
        "expected Backpressure(\"gap buffer full\"), got {first:?}"
    );
    assert!(
        wait_for(RESCUE, || !stats.stats().unwrap().reconnecting),
        "the worker never finished its budget"
    );
    assert_eq!(rig.calls(), 1, "the refused send still started a reconnect");
    // The worker's give-up is reported once, as it is at any capacity.
    let second = managed.send_bytes(b"second");
    assert!(
        matches!(second, Err(TransportError::Broken { .. })),
        "got {second:?}"
    );
    assert!(rig.wire().is_empty(), "nothing was delivered");
}

/// `Blocking`, nothing can be buffered: the call that meets the break
/// reconnects inline and puts its own message on the fresh connection, so
/// `Ok(())` still means delivered.
#[test]
fn capacity_zero_reject_blocking_reconnects_and_delivers() {
    let (mut managed, rig) = capacity_zero(ReconnectMode::Blocking, OverflowPolicy::Reject, 3, 0);
    let stats = managed.stats_handle();

    managed
        .send_bytes(b"first")
        .expect("delivered after the reconnect");
    assert_eq!(rig.calls(), 1, "one reconnect");
    assert_eq!(rig.wire(), vec![b"first".to_vec()]);
    assert!(managed.is_alive());

    managed.send_bytes(b"second").expect("a live inner: direct");
    assert_eq!(rig.calls(), 1, "no further reconnect");
    assert_eq!(rig.wire(), vec![b"first".to_vec(), b"second".to_vec()]);
    let s = stats.stats().unwrap();
    assert_eq!(
        (s.gap_len, s.gap_messages_dropped, s.reconnect_successes),
        (0, 0, 1)
    );
}

/// `Blocking`, the factory refuses a whole budget: the usual give-up. The
/// caller keeps its message, and the next call runs a fresh budget.
#[test]
fn capacity_zero_reject_blocking_give_up_reports_broken_and_queues_nothing() {
    let (mut managed, rig) = capacity_zero(ReconnectMode::Blocking, OverflowPolicy::Reject, 2, 2);
    let stats = managed.stats_handle();

    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::Broken { ref msg, .. }) if msg.contains("gave up")),
        "got {first:?}"
    );
    assert_eq!(rig.calls(), 2, "the whole budget was used");
    assert_eq!(stats.stats().unwrap().gap_len, 0);
    assert!(rig.wire().is_empty());

    managed
        .send_bytes(b"second")
        .expect("a fresh budget; the factory is back");
    assert_eq!(rig.calls(), 3);
    assert_eq!(rig.wire(), vec![b"second".to_vec()]);
}

/// `Blocking`: what the fresh connection answers to the call's own message
/// is what the call returns. `Backpressure` hands the message back; the
/// identical retry is delivered once, without another reconnect.
#[test]
fn capacity_zero_reject_blocking_backpressure_on_the_fresh_inner_is_handed_back() {
    let (mut managed, rig) = capacity_zero(ReconnectMode::Blocking, OverflowPolicy::Reject, 3, 0);
    rig.fresh_script
        .lock()
        .unwrap()
        .push_back(SendStep::Backpressure);

    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::Backpressure { .. })) && !is_gap_full(&first),
        "the fresh inner's own refusal, got {first:?}"
    );
    assert!(rig.wire().is_empty());

    managed.send_bytes(b"first").expect("the retry");
    assert_eq!(rig.calls(), 1);
    assert_eq!(rig.wire(), vec![b"first".to_vec()]);
}

/// `Blocking`: a fresh connection that breaks on the call's message ends the
/// call with `Broken` (one call, one reconnect); the next call reconnects.
#[test]
fn capacity_zero_reject_blocking_a_fresh_inner_that_breaks_reports_broken() {
    let (mut managed, rig) = capacity_zero(ReconnectMode::Blocking, OverflowPolicy::Reject, 3, 0);
    rig.fresh_script.lock().unwrap().push_back(SendStep::Broken);

    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::Broken { .. })),
        "got {first:?}"
    );
    assert_eq!(rig.calls(), 1, "one reconnect per call");
    assert!(rig.wire().is_empty());

    managed
        .send_bytes(b"first")
        .expect("the next call reconnects");
    assert_eq!(rig.calls(), 2);
    assert_eq!(rig.wire(), vec![b"first".to_vec()]);
}

/// A send transport that does whatever its closure says.
struct Hooked(Box<dyn FnMut() -> Result<(), TransportError> + Send>);

impl Transport for Hooked {
    fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
        (self.0)()
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn close(&mut self) {}
}

/// `Blocking`, capacity 0, `Reject`: the initial inner is dead and every
/// inner the factory builds answers its sends with `fresh_send`.
fn capacity_zero_blocking_over(
    fresh_send: impl Fn() -> Result<(), TransportError> + Send + Sync + 'static,
) -> ManagedTransport<Hooked> {
    let fresh_send = Arc::new(fresh_send);
    let factory = move || -> Result<Hooked, TransportError> {
        let fresh_send = Arc::clone(&fresh_send);
        Ok(Hooked(Box::new(move || fresh_send())))
    };
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(1)))
        .with_gap_buffer_capacity(0)
        .with_overflow_policy(OverflowPolicy::Reject);
    let initial = Hooked(Box::new(|| Err(broken("dead (test)"))));
    ManagedTransport::new(initial, factory, policy).expect("the policy validates")
}

/// `Blocking`: a fresh connection that says `Closed` is a dead connection,
/// not the wrapper's own close.
#[test]
fn capacity_zero_reject_blocking_a_fresh_inner_that_answers_closed_reports_broken() {
    let mut managed = capacity_zero_blocking_over(|| Err(TransportError::Closed));
    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::Broken { .. })),
        "got {first:?}"
    );
    assert!(managed.is_alive(), "the wrapper is not closed");
}

/// `Blocking`: a cancel that lands while the call's message is being sent on
/// the fresh connection makes that send fail like a dead link; the call
/// reports the cancel.
#[test]
fn capacity_zero_reject_blocking_cancel_during_the_send_after_the_reconnect_reports_the_cancel() {
    let cancel: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>> = Arc::default();
    let mut managed = capacity_zero_blocking_over({
        let cancel = Arc::clone(&cancel);
        move || {
            cancel
                .lock()
                .unwrap()
                .as_ref()
                .expect("published before the send")
                .cancel();
            Err(broken("socket closed under the write (test)"))
        }
    });
    *cancel.lock().unwrap() = managed.cancel_handle();

    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::ExplicitClose)),
        "got {first:?}"
    );
}

/// `Blocking`: a fresh connection that was itself cancelled ends the
/// wrapper, as a cancelled inner does on every other path.
#[test]
fn capacity_zero_reject_blocking_a_cancelled_fresh_inner_ends_the_wrapper() {
    let mut managed = capacity_zero_blocking_over(|| Err(TransportError::ExplicitClose));
    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::ExplicitClose)),
        "got {first:?}"
    );
    assert!(!managed.is_alive(), "latched");
    assert!(matches!(
        managed.send_bytes(b"second"),
        Err(TransportError::ExplicitClose)
    ));
}

/// `Blocking`: a cancel that fires while the call waits in its reconnect is
/// what the call reports, and the fresh connection carries nothing.
#[test]
fn capacity_zero_reject_blocking_cancel_during_the_reconnect_reports_the_cancel() {
    let wire: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let cancel: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>> = Arc::default();
    let factory = {
        let wire = Arc::clone(&wire);
        let cancel = Arc::clone(&cancel);
        move || -> Result<ScriptedSend, TransportError> {
            // The cancel lands while the send is inside its re-dial.
            cancel
                .lock()
                .unwrap()
                .as_ref()
                .expect("published before the send")
                .cancel();
            Ok(ScriptedSend::scripted(Arc::default(), Arc::clone(&wire)))
        }
    };
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(1)))
        .with_gap_buffer_capacity(0)
        .with_overflow_policy(OverflowPolicy::Reject);
    let initial = ScriptedSend::scripted(
        Arc::new(Mutex::new(VecDeque::from([SendStep::Broken]))),
        Arc::clone(&wire),
    );
    let mut managed =
        ManagedTransport::new(initial, factory, policy).expect("the policy validates");
    *cancel.lock().unwrap() = managed.cancel_handle();

    let first = managed.send_bytes(b"first");
    assert!(
        matches!(first, Err(TransportError::ExplicitClose)),
        "got {first:?}"
    );
    assert!(wire.lock().unwrap().is_empty(), "nothing was sent");
    assert!(!managed.is_alive());
}

/// `Background`, nothing can be buffered: the send that meets the break is
/// refused, a worker re-dials all the same, and once it is done sends go
/// straight to the fresh connection.
#[test]
fn capacity_zero_reject_background_reconnects_and_a_later_send_arrives() {
    let (mut managed, rig) = capacity_zero(ReconnectMode::Background, OverflowPolicy::Reject, 3, 0);
    let stats = managed.stats_handle();

    let first = managed.send_bytes(b"first");
    assert!(is_gap_full(&first), "got {first:?}");
    assert!(
        wait_for(RESCUE, || !stats.stats().unwrap().reconnecting),
        "the worker never left"
    );
    assert_eq!(rig.calls(), 1, "the worker re-dialled once");
    assert!(managed.is_alive());

    // `Backpressure`: the caller offers the same bytes again.
    managed
        .send_bytes(b"first")
        .expect("direct, on the fresh inner");
    managed.send_bytes(b"second").expect("direct");
    assert!(
        wait_for(RESCUE, || rig.wire().len() == 2),
        "the later sends never arrived"
    );
    assert_eq!(rig.wire(), vec![b"first".to_vec(), b"second".to_vec()]);
    let s = stats.stats().unwrap();
    assert_eq!((s.gap_len, s.gap_messages_dropped), (0, 0));
    assert_eq!(rig.calls(), 1);
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
            return Err(broken("broken (test)"));
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
    let policy = ReconnectPolicy::default()
        .with_max_attempts(None)
        // Unlimited attempts need a non-zero wait to pass validation.
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(1)))
        .with_gap_buffer_capacity(2)
        .with_overflow_policy(OverflowPolicy::DropOldest)
        .with_mode(ReconnectMode::Background);
    let inner = Link {
        broken: true,
        delivered: Arc::clone(&delivered),
    };
    let mut managed = ManagedTransport::new(inner, factory, policy).expect("the policy validates");
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
