//! Behavior suite for `ReconnectMode::Background`.
//!
//! All synchronization is deadline-polling (`wait_until`) — never bare
//! sleeps-as-sync. Timing assertions carry >=5x margins for slow CI.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tst_core::transport::{BrokenCause, Transport, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, OverflowPolicy, ReconnectMode, ReconnectPolicy,
};

/// What the next inner send should do. Empty script => Ok.
#[derive(Clone, Copy, Debug)]
enum SendOutcome {
    Ok,
    Backpressure,
    Broken,
}

/// Script-driven mock transport. Successful sends land in a log SHARED
/// with the test rig (survives factory rebuilds, so FIFO can be asserted
/// across reconnect cycles).
struct ScriptedTransport {
    script: Arc<Mutex<VecDeque<SendOutcome>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    max_payload: usize,
    alive: bool,
}

impl Transport for ScriptedTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.max_payload,
            });
        }
        if !self.alive {
            return Err(TransportError::Broken {
                msg: "dead".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        match self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SendOutcome::Ok)
        {
            SendOutcome::Ok => {
                self.sent.lock().unwrap().push(msg.to_vec());
                Ok(())
            }
            SendOutcome::Backpressure => Err(TransportError::Backpressure {
                msg: "bp".into(),
                errno_code: None,
            }),
            SendOutcome::Broken => {
                self.alive = false;
                Err(TransportError::Broken {
                    msg: "scripted break".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
        }
    }
    fn max_payload(&self) -> usize {
        self.max_payload
    }
    fn is_alive(&self) -> bool {
        self.alive
    }
    fn close(&mut self) {
        self.alive = false;
    }
}

/// Shared state backing every transport the rig (or its factories) builds.
struct Rig {
    script: Arc<Mutex<VecDeque<SendOutcome>>>,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    factory_calls: Arc<AtomicU32>,
}

impl Rig {
    fn new() -> Self {
        Self {
            script: Arc::new(Mutex::new(VecDeque::new())),
            sent: Arc::new(Mutex::new(Vec::new())),
            factory_calls: Arc::new(AtomicU32::new(0)),
        }
    }
    fn transport(&self) -> ScriptedTransport {
        self.transport_with_payload(1316)
    }
    fn transport_with_payload(&self, max_payload: usize) -> ScriptedTransport {
        ScriptedTransport {
            script: Arc::clone(&self.script),
            sent: Arc::clone(&self.sent),
            max_payload,
            alive: true,
        }
    }
    fn factory(
        &self,
        fail_first: u32,
    ) -> impl Fn() -> Result<ScriptedTransport, TransportError> + Send + Sync + 'static {
        self.factory_with_payload(fail_first, 1316)
    }
    fn factory_with_payload(
        &self,
        fail_first: u32,
        max_payload: usize,
    ) -> impl Fn() -> Result<ScriptedTransport, TransportError> + Send + Sync + 'static {
        let calls = Arc::clone(&self.factory_calls);
        let script = Arc::clone(&self.script);
        let sent = Arc::clone(&self.sent);
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < fail_first {
                Err(TransportError::Broken {
                    msg: "factory down".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            } else {
                Ok(ScriptedTransport {
                    script: Arc::clone(&script),
                    sent: Arc::clone(&sent),
                    max_payload,
                    alive: true,
                })
            }
        }
    }
    fn push_outcome(&self, o: SendOutcome) {
        self.script.lock().unwrap().push_back(o);
    }
    fn sent_snapshot(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }
}

fn bg_policy(max_attempts: Option<u32>, backoff: BackoffStrategy) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff,
        gap_buffer_capacity: 64,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Background,
    }
}

/// Poll until `cond` or panic after `deadline`.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < deadline,
            "condition not met within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn background_send_never_blocks_during_outage() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken); // first send breaks the inner
    // Factory never succeeds; backoff parks the worker 5s per attempt.
    // Every send during the outage must return immediately regardless.
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_secs(5)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    for i in 0..20u8 {
        let t0 = Instant::now();
        managed
            .send_bytes(&[i])
            .expect("background mode accepts into the gap buffer");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "send_bytes blocked on reconnect backoff: {:?}",
            t0.elapsed()
        );
    }
    // Drop detaches; the worker wakes from its 5s wait on the signal and exits.
}

#[test]
fn background_reconnect_drains_fifo_then_resumes_direct() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Ok); //     msg 0: direct send
    rig.push_outcome(SendOutcome::Broken); // msg 1: breaks the inner (msg 1 must be queued, not lost)
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(20)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(2), policy);

    managed.send_bytes(&[0]).unwrap();
    managed.send_bytes(&[1]).unwrap(); // break -> enqueued, worker spawned
    managed.send_bytes(&[2]).unwrap(); // gate -> gap
    managed.send_bytes(&[3]).unwrap(); // gate -> gap
    // Factory fails twice; 3rd call succeeds; worker drains 1,2,3 in order.
    wait_until(Duration::from_secs(10), || rig.sent_snapshot().len() == 4);
    assert_eq!(
        rig.sent_snapshot(),
        vec![vec![0], vec![1], vec![2], vec![3]],
        "exact FIFO across the outage — no loss, no reorder, no double-send"
    );
    // After drain the worker exits; the next send may go direct or ride a
    // benign gate window — either way it lands, in order.
    managed.send_bytes(&[4]).unwrap();
    wait_until(Duration::from_secs(10), || rig.sent_snapshot().len() == 5);
    assert_eq!(rig.sent_snapshot()[4], vec![4]);
}

#[test]
fn background_dropoldest_evicts_and_counts_deterministically() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    // 200ms backoff >> the microseconds the 6 sends below take, so the
    // eviction sequence is deterministic (no concurrent drain).
    let mut policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(200)));
    policy.gap_buffer_capacity = 2;
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(3), policy);
    let stats = managed.stats_handle();

    for i in 0..6u8 {
        managed.send_bytes(&[i]).unwrap();
    }
    // Queue evolution (cap 2): [0] [0,1] [1,2] [2,3] [3,4] [4,5] => 4 evicted.
    wait_until(Duration::from_secs(15), || {
        stats.stats().unwrap().reconnect_successes == 1
    });
    wait_until(Duration::from_secs(15), || {
        !stats.stats().unwrap().reconnecting
    });
    let s = stats.stats().unwrap();
    assert_eq!(s.gap_messages_dropped, 4);
    assert_eq!(s.reconnect_attempts, 4, "3 factory failures + 1 success");
    assert_eq!(
        rig.sent_snapshot(),
        vec![vec![4], vec![5]],
        "only the surviving tail is delivered, in order"
    );
    assert_eq!(s.gap_len, 0);
}

/// Fix round 1 (review finding I1): a worker that unwinds — e.g. a
/// user-supplied `factory()` that panics on `unwrap()` during DNS/socket
/// setup — must not leave `bg_active` stuck true. Pre-fix that would wedge
/// every future send into the enqueue-and-return-Ok branch forever, with
/// no replacement worker ever able to spawn: unbounded silent loss
/// reported as healthy. The panic below is expected to print to captured
/// test output.
#[test]
fn background_worker_panic_recovers_via_abnormal_give_up() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken); // msg 0's send breaks the inner -> worker spawns
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(10)));

    // Custom factory (not rig.factory(), which never panics): first call
    // panics — simulating e.g. an `unwrap()` on DNS/socket setup — every
    // later call succeeds with a normal working transport.
    let calls = Arc::clone(&rig.factory_calls);
    let script = Arc::clone(&rig.script);
    let sent = Arc::clone(&rig.sent);
    let factory = move || -> Result<ScriptedTransport, TransportError> {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            panic!("test factory panic");
        }
        Ok(ScriptedTransport {
            script: Arc::clone(&script),
            sent: Arc::clone(&sent),
            max_payload: 1316,
            alive: true,
        })
    };

    let mut managed = ManagedTransport::new(rig.transport(), factory, policy);
    let stats = managed.stats_handle();

    managed.send_bytes(&[0]).unwrap(); // breaks the inner; enqueued; worker spawns and panics
    wait_until(Duration::from_secs(10), || {
        !stats.stats().unwrap().reconnecting
    });

    // Report-once: the next send surfaces the abnormal give-up, and its
    // own bytes are NOT queued (same contract as a normal give-up).
    match managed.send_bytes(&[1]).unwrap_err() {
        TransportError::Broken { msg, .. } => {
            assert!(
                msg.contains("aborted"),
                "expected an abnormal give-up message, got: {msg}"
            );
        }
        other => panic!("expected Broken, got {other:?}"),
    }

    // The give-up is consumed — this send starts a fresh cycle instead of
    // repeating the abnormal error, and the factory's 2nd call succeeds.
    managed.send_bytes(&[2]).unwrap();
    wait_until(Duration::from_secs(10), || rig.sent_snapshot().len() == 2);
    assert_eq!(
        rig.sent_snapshot(),
        vec![vec![0], vec![2]],
        "msg 0 (queued through the crash) then msg 2, in order; msg 1 was never queued"
    );
    assert!(stats.stats().unwrap().reconnect_successes >= 1);
}

/// Fix round 2 (review finding B): `ActiveClearGuard`'s `Drop` must not
/// unconditionally re-clear `bg_active` after the Empty-exit already
/// cleared it in place under the gap lock. If a worker's `Drop` runs
/// *after* a fresh cycle has already spawned a replacement (bg_active =
/// true again), an unconditional re-clear would clobber that fresh
/// cycle's ownership: the newer worker ends up "unowned", a later
/// `send_bytes` sees `!bg_active` with a non-empty gap and spawns YET
/// ANOTHER worker on top of it, and that `spawn_worker`'s `prev.join()`
/// blocks on the still-live worker — under `max_attempts: None` and a
/// persisting outage, forever.
///
/// This drives many drain-empty-then-break-again flaps back to back —
/// exactly the window the clobber lives in — with every `send_bytes` call
/// deadline-asserted. The assert runs after `send_bytes` returns, so it
/// cannot convert a genuine join-block wedge into a clean test failure —
/// a real wedge hangs this binary under plain `cargo test`; nextest's
/// per-test timeout is the actual bound in CI. Note: the clobber needs
/// specific cross-thread timing to land (see the fix report for why a
/// deterministic red run isn't attempted here — this is a soak against
/// the race, not a targeted repro).
#[test]
fn flap_cycles_never_wedge_or_hang() {
    let rig = Rig::new();
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(1)));
    // fail_first: 0 -> every reconnect attempt succeeds on the first try,
    // so each cycle is a fast break -> queue -> reconnect -> drain -> empty
    // flap.
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(0), policy);
    let stats = managed.stats_handle();

    // u32 index encoding: each message's payload is its 4-byte LE global
    // send index, so FIFO/subsequence checking across 30 cycles is
    // unambiguous regardless of total message count.
    let mut next_idx: u32 = 0;
    let mut all_sent: Vec<u32> = Vec::new();
    for _ in 0..30 {
        rig.push_outcome(SendOutcome::Broken); // breaks the inner on this cycle's first send
        for _ in 0..5 {
            let idx = next_idx;
            next_idx += 1;
            let t0 = Instant::now();
            managed
                .send_bytes(&idx.to_le_bytes())
                .expect("background mode always accepts, direct or into the gap");
            assert!(
                t0.elapsed() < Duration::from_secs(1),
                "send_bytes blocked for {:?} on message {idx} — possible join() wedge (Finding B)",
                t0.elapsed()
            );
            all_sent.push(idx);
        }
        wait_until(Duration::from_secs(10), || {
            let s = stats.stats().unwrap();
            !s.reconnecting && s.gap_len == 0
        });
    }

    let delivered: Vec<u32> = rig
        .sent_snapshot()
        .into_iter()
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("4-byte u32 index")))
        .collect();

    // Strictly increasing (FIFO — no reorder, no double-send) and a
    // subsequence of everything sent (DropOldest could in principle evict
    // some, though capacity 64 >> the 5-message bursts here).
    let mut send_iter = all_sent.into_iter().peekable();
    let mut prev: Option<u32> = None;
    for idx in &delivered {
        if let Some(p) = prev {
            assert!(*idx > p, "delivered log out of order: {p} then {idx}");
        }
        while send_iter.peek().is_some_and(|s| s != idx) {
            send_iter.next();
        }
        assert!(
            send_iter.next().is_some(),
            "delivered index {idx} is not a subsequence of what was sent"
        );
        prev = Some(*idx);
    }

    assert!(
        stats.stats().unwrap().reconnect_successes >= 30,
        "expected at least one successful reconnect per flap cycle"
    );

    // M10 (routed fix): the 30 cycles above all reconnect on the first
    // attempt (factory(0), 1ms backoff), so the guard-clobber window
    // closes fast every time — not discriminating for a PERSISTING
    // outage, where the worker sits through several failed reconnect
    // attempts before draining. `Rig::factory`'s fail-first count is
    // fixed at construction and shared across the whole ManagedTransport
    // lifetime, not re-armed per cycle — so with `factory(3)` only the
    // FIRST of the 3 cycles below is a genuinely persisting outage (3
    // failed attempts at the slower 300ms cadence before the reconnect
    // succeeds); the counter has already passed 3 by the time cycles 2
    // and 3 break, so those reconnect immediately, same as the fast
    // cycles above. They're kept anyway for extra drain-empty -> break-
    // again handoff coverage at the slower cadence. All 3 cycles are
    // checked together, independently, for FIFO/subsequence below.
    let rig2 = Rig::new();
    let policy2 = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(300)));
    let mut managed2 = ManagedTransport::new(rig2.transport(), rig2.factory(3), policy2);
    let stats2 = managed2.stats_handle();

    let mut next_idx2: u32 = 0;
    let mut all_sent2: Vec<u32> = Vec::new();
    for _ in 0..3 {
        rig2.push_outcome(SendOutcome::Broken); // breaks the inner on this cycle's first send
        for _ in 0..5 {
            let idx = next_idx2;
            next_idx2 += 1;
            let t0 = Instant::now();
            managed2
                .send_bytes(&idx.to_le_bytes())
                .expect("background mode always accepts, direct or into the gap");
            assert!(
                t0.elapsed() < Duration::from_secs(1),
                "send_bytes blocked for {:?} on message {idx} (persisting-outage cycle) — possible join() wedge (Finding B)",
                t0.elapsed()
            );
            all_sent2.push(idx);
        }
        wait_until(Duration::from_secs(10), || {
            let s = stats2.stats().unwrap();
            !s.reconnecting && s.gap_len == 0
        });
    }

    let delivered2: Vec<u32> = rig2
        .sent_snapshot()
        .into_iter()
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("4-byte u32 index")))
        .collect();

    let mut send_iter2 = all_sent2.into_iter().peekable();
    let mut prev2: Option<u32> = None;
    for idx in &delivered2 {
        if let Some(p) = prev2 {
            assert!(*idx > p, "delivered log out of order: {p} then {idx}");
        }
        while send_iter2.peek().is_some_and(|s| s != idx) {
            send_iter2.next();
        }
        assert!(
            send_iter2.next().is_some(),
            "delivered index {idx} is not a subsequence of what was sent"
        );
        prev2 = Some(*idx);
    }
    assert!(
        stats2.stats().unwrap().reconnect_successes >= 3,
        "expected at least one successful reconnect per persisting-outage cycle"
    );
}

#[test]
fn background_give_up_reports_broken_once_then_restarts_budget() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = bg_policy(
        Some(2),
        BackoffStrategy::Constant(Duration::from_millis(10)),
    );
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    let stats = managed.stats_handle();

    managed.send_bytes(&[0]).unwrap(); // break -> worker starts, 2 attempts, gives up
    wait_until(Duration::from_secs(10), || {
        !stats.stats().unwrap().reconnecting
    });
    assert!(!managed.is_alive(), "gave up + inner gone => not alive");
    assert_eq!(
        stats.stats().unwrap().gap_len,
        1,
        "backlog retained across give-up"
    );

    // First send after give-up: the one-shot Broken report. Its bytes are
    // NOT queued — the caller saw the error and owns the resend.
    let err = managed.send_bytes(&[1]).unwrap_err();
    match err {
        tst_core::transport::TransportError::Broken { msg, .. } => {
            assert!(msg.contains("gave up after 2 attempts"), "got: {msg}");
        }
        other => panic!("expected Broken give-up report, got {other:?}"),
    }
    assert_eq!(
        stats.stats().unwrap().gap_len,
        1,
        "reported call's bytes not queued"
    );

    // Next send starts a fresh cycle with a fresh budget.
    managed.send_bytes(&[2]).unwrap();
    let s = stats.stats().unwrap();
    assert!(
        s.reconnecting || s.gap_len >= 2,
        "fresh worker cycle started with the backlog: {s:?}"
    );
}

#[test]
fn background_none_budget_never_gives_up() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(5)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    let stats = managed.stats_handle();
    managed.send_bytes(&[0]).unwrap();
    wait_until(Duration::from_secs(10), || {
        stats.stats().unwrap().reconnect_attempts >= 10
    });
    let s = stats.stats().unwrap();
    assert!(s.reconnecting, "still retrying, never gave up: {s:?}");
    assert!(
        managed.send_bytes(&[1]).is_ok(),
        "no give-up report with None budget"
    );
}

#[test]
fn close_joins_worker_promptly_mid_backoff() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    // 30s backoff: a sleep-bounded join would hang the test far past the
    // assert; the interruptible wait makes close return in milliseconds.
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_secs(30)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    managed.send_bytes(&[0]).unwrap(); // worker parks in the 30s wait
    let t0 = Instant::now();
    managed.close();
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "close must interrupt the backoff wait and join, took {:?}",
        t0.elapsed()
    );
    assert!(matches!(
        managed.send_bytes(&[1]).unwrap_err(),
        tst_core::transport::TransportError::Closed
    ));
}

#[test]
fn drop_detaches_without_blocking_and_worker_quiesces() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(5)));
    let calls = Arc::clone(&rig.factory_calls);
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    managed.send_bytes(&[0]).unwrap();
    wait_until(Duration::from_secs(10), || {
        calls.load(Ordering::SeqCst) >= 2
    });
    let t0 = Instant::now();
    drop(managed);
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "Drop must signal-and-detach, never join: {:?}",
        t0.elapsed()
    );
    // Quiescence: at a 5ms retry cadence a live worker adds ~40 calls per
    // 200ms window; a signaled worker exits and the counter goes flat.
    std::thread::sleep(Duration::from_millis(100)); // allow the exit to land
    let a = calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    let b = calls.load(Ordering::SeqCst);
    assert_eq!(a, b, "worker kept retrying after Drop ({a} -> {b})");
}

#[test]
fn cancel_handle_stops_background_worker() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(5)));
    let calls = Arc::clone(&rig.factory_calls);
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    let cancel = managed
        .cancel_handle()
        .expect("managed always has a handle");
    managed.send_bytes(&[0]).unwrap();
    wait_until(Duration::from_secs(10), || {
        calls.load(Ordering::SeqCst) >= 2
    });
    cancel.cancel();
    std::thread::sleep(Duration::from_millis(100));
    let a = calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    let b = calls.load(Ordering::SeqCst);
    assert_eq!(a, b, "worker kept retrying after cancel ({a} -> {b})");
    assert!(matches!(
        managed.send_bytes(&[1]).unwrap_err(),
        tst_core::transport::TransportError::Closed
    ));
}

#[test]
fn drain_backpressure_retries_same_message_without_loss() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken); //       msg 0 -> gap
    rig.push_outcome(SendOutcome::Backpressure); // drain: msg 0 stalls twice...
    rig.push_outcome(SendOutcome::Backpressure);
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(10)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(0), policy);
    managed.send_bytes(&[0]).unwrap();
    managed.send_bytes(&[1]).unwrap();
    // ...then the script runs dry (=> Ok): the worker's 20ms retry loop
    // must deliver both, in order, exactly once.
    wait_until(Duration::from_secs(10), || {
        rig.sent_snapshot() == vec![vec![0], vec![1]]
    });
}

#[test]
fn drain_break_resumes_reconnect_with_front_message_retained() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken); // initial break: msg 0 -> gap
    rig.push_outcome(SendOutcome::Ok); //     reconnect #1 drains msg 0...
    rig.push_outcome(SendOutcome::Broken); // ...and breaks on msg 1 mid-drain
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(10)));
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(0), policy);
    managed.send_bytes(&[0]).unwrap();
    managed.send_bytes(&[1]).unwrap();
    managed.send_bytes(&[2]).unwrap();
    // Worker: reconnect, drain 0, break on 1 (1 stays at front), reconnect
    // again, drain 1 then 2. Exact FIFO, no loss, no double-send.
    wait_until(Duration::from_secs(10), || {
        rig.sent_snapshot() == vec![vec![0], vec![1], vec![2]]
    });
    assert!(managed.stats_handle().stats().unwrap().reconnect_successes >= 2);
}

#[test]
fn background_reject_full_returns_backpressure_immediately() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = ReconnectPolicy {
        max_attempts: None,
        backoff: BackoffStrategy::Constant(Duration::from_secs(5)), // park the worker far away
        gap_buffer_capacity: 2,
        overflow_policy: OverflowPolicy::Reject,
        mode: ReconnectMode::Background,
    };
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(u32::MAX), policy);
    managed.send_bytes(&[0]).unwrap(); // break -> queued (gap: [0])
    managed.send_bytes(&[1]).unwrap(); // gate -> gap: [0,1] == capacity
    let t0 = Instant::now();
    let err = managed.send_bytes(&[2]).unwrap_err();
    assert!(
        matches!(
            err,
            tst_core::transport::TransportError::Backpressure { .. }
        ),
        "Reject-full surfaces Backpressure, got {err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "Reject path must not block either"
    );
}

#[test]
fn oversized_after_ceiling_shrink_drops_and_counts_instead_of_wedging() {
    let rig = Rig::new();
    rig.push_outcome(SendOutcome::Broken);
    let policy = bg_policy(None, BackoffStrategy::Constant(Duration::from_millis(10)));
    // Initial inner accepts 1316 bytes; every REBUILT inner only 4. The
    // 100-byte queued message would wedge the drain forever if the worker
    // propagated TooLarge like the blocking drain does — instead it must
    // drop-and-count and keep draining.
    let mut managed = ManagedTransport::new(
        rig.transport_with_payload(1316),
        rig.factory_with_payload(0, 4),
        policy,
    );
    managed.send_bytes(&[9; 100]).unwrap(); // breaks -> 100-byte msg queued
    managed.send_bytes(&[1, 2, 3]).unwrap(); // small follower
    wait_until(Duration::from_secs(10), || {
        rig.sent_snapshot() == vec![vec![1, 2, 3]]
    });
    let s = managed.stats_handle().stats().unwrap();
    assert_eq!(s.gap_messages_dropped, 1);
    assert_eq!(s.gap_bytes_dropped, 100);
}

#[test]
fn stress_pump_vs_flapping_sink_delivery_is_fifo_subsequence() {
    let rig = Rig::new();
    let policy = ReconnectPolicy {
        max_attempts: None,
        backoff: BackoffStrategy::Constant(Duration::from_millis(1)),
        gap_buffer_capacity: 8,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Background,
    };
    let mut managed = ManagedTransport::new(rig.transport(), rig.factory(0), policy);
    // 2000 sends, a scripted break every 40th — breaks land on direct
    // sends AND drain sends (whichever pops the script next), exercising
    // both fault paths under pump/worker interleaving.
    for i in 0..2000u32 {
        if i % 40 == 0 {
            rig.push_outcome(SendOutcome::Broken);
        }
        managed.send_bytes(&i.to_be_bytes()).unwrap();
        if i % 7 == 0 {
            std::thread::sleep(Duration::from_micros(50)); // let the worker interleave
        }
    }
    let stats = managed.stats_handle();
    wait_until(Duration::from_secs(30), || {
        let s = stats.stats().unwrap();
        !s.reconnecting && s.gap_len == 0
    });
    // FIFO + no-double-send: delivered indices strictly increase.
    let sent = rig.sent_snapshot();
    assert!(!sent.is_empty());
    let mut prev: i64 = -1;
    for m in &sent {
        let v = u32::from_be_bytes(m[..4].try_into().unwrap()) as i64;
        assert!(
            v > prev,
            "out-of-order or duplicate delivery: {v} after {prev}"
        );
        prev = v;
    }
}
