//! `ManagedTransport`'s cancel must reach the inner transport without ever
//! taking the I/O mutex.
//!
//! Every send path in the wrapper holds `inner` across the inner transport's
//! `send_bytes`. A cancel that has to read the inner out of that same mutex
//! to find its cancel handle therefore queues behind the very call it was
//! asked to interrupt — the wake never arrives and the caller waits out the
//! transport's own timeout instead. These tests cover the three places a
//! send can park: a direct send, an inline gap drain after a reconnect, and
//! a background worker's drain.
//!
//! Shape rules (so a regression fails loudly instead of wedging a CI runner):
//! every wait is deadline-bounded, the gate the mock parks on is released in
//! cleanup on every path, and the cancel runs on its own thread so "cancel
//! never returned" surfaces as `joined == false` rather than a hang.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tst_core::transport::{Transport, TransportCancel, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, OverflowPolicy, ReconnectMode, ReconnectPolicy,
};

/// How long a parked send is given to actually park.
const PARK_DEADLINE: Duration = Duration::from_secs(5);
/// How long the cancel gets to return. Post-fix it never waits on the send
/// mutex and returns in microseconds; pre-fix it never returns at all.
const CANCEL_DEADLINE: Duration = Duration::from_secs(2);

/// One-way gate a parked send waits on. `open()` releases every waiter and
/// stays open, so opening twice (once from the cancel handle, once from the
/// test's cleanup) is safe.
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            open: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn open(&self) {
        let mut open = self.open.lock().expect("gate mutex is never poisoned");
        *open = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().expect("gate mutex is never poisoned");
        while !*open {
            open = self.cv.wait(open).expect("gate mutex is never poisoned");
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Parks inside `send_bytes` until the gate opens — stands in for a
    /// transport blocked in `srt_sendmsg`.
    Park,
    /// Refuses every send, which is what drives the wrapper onto its
    /// reconnect path.
    Break,
}

struct Mock {
    behavior: Behavior,
    gate: Arc<Gate>,
    entered: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl Transport for Mock {
    fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
        match self.behavior {
            Behavior::Break => Err(TransportError::Broken {
                msg: "inner is down".into(),
                errno_code: None,
            }),
            Behavior::Park => {
                self.entered.store(true, Ordering::SeqCst);
                self.gate.wait();
                Ok(())
            }
        }
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        self.behavior == Behavior::Park && !self.cancelled.load(Ordering::SeqCst)
    }

    fn close(&mut self) {}

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(GateCancel {
            cancelled: Arc::clone(&self.cancelled),
            gate: Arc::clone(&self.gate),
        }))
    }
}

/// What a real cancel handle does: flag the transport AND unblock whatever
/// call is parked on it (libsrt's `srt_close` returns a parked `srt_sendmsg`
/// with an error). Modelling the wake here is what makes these tests prove
/// the cancel *reached something that can free the parked thread*, not just
/// that it returned.
struct GateCancel {
    cancelled: Arc<AtomicBool>,
    gate: Arc<Gate>,
}

impl TransportCancel for GateCancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.gate.open();
    }
}

/// A `Break` inner with private flags. Every test starts the wrapper on one
/// of these where a reconnect is wanted: its cancel handle writes flags no
/// assertion reads, so a slot still holding this stale handle when the cancel
/// fires cannot satisfy the "the live inner was cancelled" assertions.
fn broken_inner() -> Mock {
    Mock {
        behavior: Behavior::Break,
        gate: Arc::new(Gate::new()),
        entered: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

/// Factory that never succeeds — used where the test must never reconnect.
fn dead_factory() -> impl Fn() -> Result<Mock, TransportError> + Send + Sync + 'static {
    || {
        Err(TransportError::Broken {
            msg: "factory down".into(),
            errno_code: None,
        })
    }
}

/// Factory handing back a parking transport, counting its calls.
fn park_factory(
    gate: Arc<Gate>,
    entered: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    calls: Arc<AtomicU32>,
) -> impl Fn() -> Result<Mock, TransportError> + Send + Sync + 'static {
    move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Mock {
            behavior: Behavior::Park,
            gate: Arc::clone(&gate),
            entered: Arc::clone(&entered),
            cancelled: Arc::clone(&cancelled),
        })
    }
}

fn policy(mode: ReconnectMode) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: None,
        // Zero backoff: these tests are about the cancel path, not the
        // cadence, and a real wait would only slow the reconnect they need.
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 64,
        overflow_policy: OverflowPolicy::DropOldest,
        mode,
    }
}

/// Poll `f` until it holds or the deadline passes. Returns whether it held.
fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while !f() {
        if t0.elapsed() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Fire `cancel` on its own thread; report whether it returned in time.
///
/// The thread is deliberately not joined while it may still be running: when
/// the cancel is stuck behind the send mutex it never returns, and joining it
/// there would hang the test instead of failing it. The caller releases the
/// gate straight after, which lets the stuck thread finish and exit on its own.
///
/// Once the thread HAS finished, it is joined after all — that join cannot
/// block — so a panicking `cancel()` is re-raised here instead of being
/// reported as a prompt, successful cancel (`is_finished()` alone is true for
/// a panicked thread too).
fn cancel_completes(cancel: Arc<dyn TransportCancel + Send + Sync>, deadline: Duration) -> bool {
    let handle = std::thread::spawn(move || cancel.cancel());
    if !wait_for(deadline, || handle.is_finished()) {
        return false;
    }
    if let Err(panic) = handle.join() {
        std::panic::resume_unwind(panic);
    }
    true
}

/// The direct path: `send_managed` parks inside the inner send while holding
/// `inner`.
#[test]
fn cancel_completes_while_a_direct_send_is_parked() {
    let gate = Arc::new(Gate::new());
    let entered = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let inner = Mock {
        behavior: Behavior::Park,
        gate: Arc::clone(&gate),
        entered: Arc::clone(&entered),
        cancelled: Arc::clone(&cancelled),
    };
    let managed = ManagedTransport::new(inner, dead_factory(), policy(ReconnectMode::Blocking));
    let cancel = managed
        .cancel_handle()
        .expect("managed always hands out a cancel handle");

    let sender = std::thread::spawn(move || {
        let mut managed = managed;
        let _ = managed.send_bytes(b"x");
    });
    assert!(
        wait_for(PARK_DEADLINE, || entered.load(Ordering::SeqCst)),
        "the inner send never parked — the test never reached the state it is about"
    );

    let joined = cancel_completes(cancel, CANCEL_DEADLINE);
    gate.open(); // cleanup: release the parked thread on every path
    let _ = sender.join();

    assert!(joined, "managed cancel blocked behind the parked send");
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the inner's cancel handle never fired"
    );
}

/// The inline reconnect path: the caller's own `send_bytes` rebuilds the
/// inner and then parks inside `drain_gap_if_alive`, holding `inner` (and
/// `gap`) across the queued message's send.
#[test]
fn cancel_completes_while_an_inline_gap_drain_is_parked() {
    let gate = Arc::new(Gate::new());
    let entered = Arc::new(AtomicBool::new(false));
    let park_cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicU32::new(0));
    let managed = ManagedTransport::new(
        broken_inner(),
        park_factory(
            Arc::clone(&gate),
            Arc::clone(&entered),
            Arc::clone(&park_cancelled),
            Arc::clone(&calls),
        ),
        policy(ReconnectMode::Blocking),
    );
    let cancel = managed
        .cancel_handle()
        .expect("managed always hands out a cancel handle");

    // The construction-time inner refuses the send, so these bytes go into
    // the gap buffer and the inline reconnect installs the parking
    // transport — which then parks draining exactly those bytes.
    let sender = std::thread::spawn(move || {
        let mut managed = managed;
        let _ = managed.send_bytes(b"x");
    });
    assert!(
        wait_for(PARK_DEADLINE, || entered.load(Ordering::SeqCst)),
        "the reconnected inner never parked in drain"
    );

    let joined = cancel_completes(cancel, CANCEL_DEADLINE);
    gate.open();
    let _ = sender.join();

    assert!(joined, "managed cancel blocked behind the parked gap drain");
    assert!(
        park_cancelled.load(Ordering::SeqCst),
        "the cancel fired a stale handle, not the transport the reconnect installed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one reconnect: the drain that parked is the one under test"
    );
}

/// The background path: the per-outage worker owns the reconnect and parks
/// inside its drain, holding `inner` and `gap`. The caller's thread is free,
/// which is exactly what makes a cancel from it plausible here.
#[test]
fn cancel_completes_while_the_background_worker_drain_is_parked() {
    let gate = Arc::new(Gate::new());
    let entered = Arc::new(AtomicBool::new(false));
    let park_cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicU32::new(0));
    let mut managed = ManagedTransport::new(
        broken_inner(),
        park_factory(
            Arc::clone(&gate),
            Arc::clone(&entered),
            Arc::clone(&park_cancelled),
            Arc::clone(&calls),
        ),
        policy(ReconnectMode::Background),
    );
    let cancel = managed
        .cancel_handle()
        .expect("managed always hands out a cancel handle");

    managed
        .send_bytes(b"x")
        .expect("background mode accepts into the gap buffer");
    assert!(
        wait_for(PARK_DEADLINE, || entered.load(Ordering::SeqCst)),
        "the background worker never parked in drain"
    );

    let joined = cancel_completes(cancel, CANCEL_DEADLINE);
    gate.open();
    managed.close(); // joins the worker, so nothing outlives the test

    assert!(
        joined,
        "managed cancel blocked behind the background worker's parked drain"
    );
    assert!(
        park_cancelled.load(Ordering::SeqCst),
        "the cancel fired a stale handle, not the transport the worker installed"
    );
}
