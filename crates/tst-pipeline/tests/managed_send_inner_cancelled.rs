//! An inner transport reports `ExplicitClose` only when someone cancelled
//! it: a cancel handle taken from it before it was wrapped, or a process-exit
//! path that closes every open socket directly. Neither goes through
//! `ManagedTransport`'s own handle, so the wrapper's latch is not set when
//! the inner says so. That is not an outage. The wrapper must treat it as
//! its own cancel: report `ExplicitClose`, stop, and never call the factory
//! again.
//!
//! Message ownership is unchanged by it. `Blocking`: the failed send's
//! message goes back to the caller and nothing stays queued. `Background`:
//! what was accepted before stays queued and undelivered (`gap_len` keeps
//! counting it, the drop counters do not), exactly as after a cancel through
//! the wrapper's own handle.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tst_core::transport::{BrokenCause, Transport, TransportCancel, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedStatsHandle, ManagedTransport, OverflowPolicy, ReconnectMode,
    ReconnectPolicy,
};

const PROMPT: Duration = Duration::from_secs(10);

/// Reports `ExplicitClose` once its flag is set, `Broken` while `dead`, and
/// records the message otherwise.
struct Inner {
    cancelled: Arc<AtomicBool>,
    dead: bool,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Transport for Inner {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(TransportError::ExplicitClose);
        }
        if self.dead {
            return Err(broken("dead on arrival"));
        }
        self.delivered.lock().unwrap().push(msg.to_vec());
        Ok(())
    }

    fn max_payload(&self) -> usize {
        1316
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
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn broken(msg: &str) -> TransportError {
    TransportError::Broken {
        msg: msg.into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    }
}

fn flag(set: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(set))
}

fn policy(mode: ReconnectMode) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(1)),
        gap_buffer_capacity: 8,
        overflow_policy: OverflowPolicy::DropOldest,
        mode,
    }
}

/// What the factory hands back, by call.
#[derive(Clone, Copy)]
enum Rebuild {
    /// Refuses with `Broken`: an ordinary failed dial.
    Refuses,
    /// A connection that was cancelled before the wrapper could use it.
    CancelledInner,
    /// The dial itself was cancelled.
    CancelledDial,
}

struct Rig {
    managed: ManagedTransport<Inner>,
    stats: ManagedStatsHandle,
    factory_calls: Arc<AtomicU32>,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn rig(
    initial_cancelled: Arc<AtomicBool>,
    dead: bool,
    rebuild: Rebuild,
    mode: ReconnectMode,
) -> Rig {
    let delivered: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::new(AtomicU32::new(0));
    let factory = {
        let calls = Arc::clone(&factory_calls);
        let delivered = Arc::clone(&delivered);
        move || -> Result<Inner, TransportError> {
            calls.fetch_add(1, Ordering::SeqCst);
            match rebuild {
                Rebuild::Refuses => Err(broken("factory down")),
                Rebuild::CancelledInner => Ok(Inner {
                    cancelled: flag(true),
                    dead: false,
                    delivered: Arc::clone(&delivered),
                }),
                Rebuild::CancelledDial => Err(TransportError::ExplicitClose),
            }
        }
    };
    let initial = Inner {
        cancelled: initial_cancelled,
        dead,
        delivered: Arc::clone(&delivered),
    };
    let managed = ManagedTransport::new(initial, factory, policy(mode));
    let stats = managed.stats_handle();
    Rig {
        managed,
        stats,
        factory_calls,
        delivered,
    }
}

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < PROMPT {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    f()
}

/// The terminal state every case below must end in: no further factory
/// call, the close reported, the wrapper not alive, nothing delivered.
fn assert_terminal(rig: &mut Rig, factory_calls: u32, gap_len: u64) {
    assert!(
        !rig.managed.is_alive(),
        "alive after its inner was cancelled"
    );
    assert_eq!(
        rig.managed.send_bytes(b"after"),
        Err(TransportError::ExplicitClose),
        "every later send reports the close"
    );
    assert_eq!(
        rig.factory_calls.load(Ordering::SeqCst),
        factory_calls,
        "the factory was called after the inner reported the cancel"
    );
    let s = rig.stats.stats().expect("gap lock not poisoned");
    assert_eq!(s.gap_len, gap_len);
    assert_eq!(s.gap_messages_dropped, 0, "undelivered, not dropped");
    assert!(!s.reconnecting);
    assert!(rig.delivered.lock().unwrap().is_empty());
}

#[test]
fn blocking_direct_send_into_a_cancelled_inner_is_terminal() {
    let cancelled = flag(false);
    let mut rig = rig(
        Arc::clone(&cancelled),
        false,
        Rebuild::Refuses,
        ReconnectMode::Blocking,
    );
    // The handle a caller took from the inner before wrapping it.
    FlagCancel(cancelled).cancel();

    assert_eq!(
        rig.managed.send_bytes(b"x"),
        Err(TransportError::ExplicitClose)
    );
    // Handed back: nothing queued, no reconnect.
    assert_terminal(&mut rig, 0, 0);
}

#[test]
fn blocking_drain_into_a_cancelled_inner_is_terminal() {
    let mut rig = rig(
        flag(false),
        true,
        Rebuild::CancelledInner,
        ReconnectMode::Blocking,
    );

    assert_eq!(
        rig.managed.send_bytes(b"x"),
        Err(TransportError::ExplicitClose)
    );
    // One dial, for the break; none for the cancel. Handed back.
    assert_terminal(&mut rig, 1, 0);
}

#[test]
fn blocking_cancelled_dial_is_terminal() {
    let mut rig = rig(
        flag(false),
        true,
        Rebuild::CancelledDial,
        ReconnectMode::Blocking,
    );

    assert_eq!(
        rig.managed.send_bytes(b"x"),
        Err(TransportError::ExplicitClose)
    );
    assert_terminal(&mut rig, 1, 0);
}

#[test]
fn background_direct_send_into_a_cancelled_inner_is_terminal() {
    let cancelled = flag(false);
    let mut rig = rig(
        Arc::clone(&cancelled),
        false,
        Rebuild::Refuses,
        ReconnectMode::Background,
    );
    FlagCancel(cancelled).cancel();

    // Not accepted: no worker would ever deliver it.
    assert_eq!(
        rig.managed.send_bytes(b"x"),
        Err(TransportError::ExplicitClose)
    );
    assert_terminal(&mut rig, 0, 0);
}

#[test]
fn background_worker_drain_into_a_cancelled_inner_is_terminal() {
    let mut rig = rig(
        flag(false),
        true,
        Rebuild::CancelledInner,
        ReconnectMode::Background,
    );

    rig.managed
        .send_bytes(b"x")
        .expect("the break is accepted into the gap buffer");
    let stats = rig.stats.clone();
    assert!(
        wait_for(|| !stats.stats().expect("no poison").reconnecting),
        "the worker kept going after its inner reported the cancel"
    );
    // The accepted message stays queued, undelivered.
    assert_terminal(&mut rig, 1, 1);
}

#[test]
fn background_cancelled_dial_is_terminal() {
    let mut rig = rig(
        flag(false),
        true,
        Rebuild::CancelledDial,
        ReconnectMode::Background,
    );

    rig.managed
        .send_bytes(b"x")
        .expect("the break is accepted into the gap buffer");
    let stats = rig.stats.clone();
    assert!(
        wait_for(|| !stats.stats().expect("no poison").reconnecting),
        "the worker kept dialling after the dial reported the cancel"
    );
    assert_terminal(&mut rig, 1, 1);
}
