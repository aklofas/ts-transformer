//! Silent-peer detection on a managed sender (`with_silent_peer_timeout`),
//! driven by a scripted inner whose peer reading the test controls.
//!
//! The windows are 20 to 100 ms. A test that waits for a trip waits on a
//! counter with a deadline: load can only delay a trip. A test that asserts
//! that NO trip happens sends one message after its pause and checks that
//! very send, so a preempted thread cannot make it fail.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_test::traced_test;
use tst_pipeline::reconnect::OverflowPolicy;
use tst_pipeline::{
    BackoffStrategy, ManagedTransport, ReconnectMode, ReconnectPolicy, Transport, TransportCancel,
    TransportError,
};
use tst_test_helpers::timing::scaled;

use crate::common::{broken, wait_for};

/// What a scripted inner answers to `peer_activity`.
#[derive(Clone)]
enum Reading {
    /// Always this value: a peer that never answers.
    Frozen(u64),
    /// The number of sends so far: a peer that answers every send.
    MovesEverySend,
    /// Whatever the test stores.
    Controlled(Arc<AtomicU64>),
    /// `None`: the transport cannot tell.
    CannotTell,
}

/// What a test sees of one scripted inner.
#[derive(Default)]
struct Seen {
    sends: AtomicU32,
    closes: AtomicU32,
    abandons: AtomicU32,
    reads: AtomicU32,
    /// Firings of the inner's own cancel handle (see `with_cancel`).
    cancels: AtomicU32,
    log: Mutex<Vec<Vec<u8>>>,
}

impl Seen {
    fn log(&self) -> Vec<Vec<u8>> {
        self.log.lock().unwrap().clone()
    }
}

struct Scripted {
    reading: Reading,
    seen: Arc<Seen>,
    /// Sends after which every send fails with `Broken`.
    break_after: Option<u32>,
    /// A pause inside every send (kept well under half the window).
    pause: Duration,
    alive: bool,
    /// Whether the inner hands out a cancel handle.
    cancellable: bool,
}

/// A scripted inner's cancel handle: it only counts.
struct CountingCancel(Arc<Seen>);
impl TransportCancel for CountingCancel {
    fn cancel(&self) {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
    }
    fn is_cancelled(&self) -> bool {
        self.0.cancels.load(Ordering::SeqCst) > 0
    }
}

impl Scripted {
    fn new(reading: Reading) -> (Self, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        (
            Self {
                reading,
                seen: Arc::clone(&seen),
                break_after: None,
                pause: Duration::ZERO,
                alive: true,
                cancellable: false,
            },
            seen,
        )
    }
    fn with_cancel(mut self) -> Self {
        self.cancellable = true;
        self
    }
}

impl Transport for Scripted {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if !self.alive {
            return Err(TransportError::Closed);
        }
        if self
            .break_after
            .is_some_and(|n| self.seen.sends.load(Ordering::SeqCst) >= n)
        {
            return Err(broken("scripted break"));
        }
        if !self.pause.is_zero() {
            std::thread::sleep(self.pause);
        }
        self.seen.log.lock().unwrap().push(msg.to_vec());
        self.seen.sends.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        self.alive
    }
    fn close(&mut self) {
        self.seen.closes.fetch_add(1, Ordering::SeqCst);
        self.alive = false;
    }
    fn abandon(&mut self) {
        self.seen.abandons.fetch_add(1, Ordering::SeqCst);
        self.alive = false;
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        self.cancellable
            .then(|| Arc::new(CountingCancel(Arc::clone(&self.seen))) as _)
    }
    fn peer_activity(&self) -> Option<u64> {
        self.seen.reads.fetch_add(1, Ordering::SeqCst);
        match &self.reading {
            Reading::Frozen(v) => Some(*v),
            Reading::MovesEverySend => Some(u64::from(self.seen.sends.load(Ordering::SeqCst))),
            Reading::Controlled(v) => Some(v.load(Ordering::SeqCst)),
            Reading::CannotTell => None,
        }
    }
}

/// A factory that counts its calls and hands out the inners of `script` in
/// order, then inners that answer every send. Every inner it made is seen
/// through `made`.
#[derive(Clone, Default)]
struct Factory {
    calls: Arc<AtomicU32>,
    script: Arc<Mutex<VecDeque<Scripted>>>,
    made: Arc<Mutex<Vec<Arc<Seen>>>>,
}

impl Factory {
    fn with(script: impl IntoIterator<Item = Scripted>) -> Self {
        let f = Self::default();
        f.script.lock().unwrap().extend(script);
        f
    }
    fn make(&self) -> Result<Scripted, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let inner = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Scripted::new(Reading::MovesEverySend).0);
        self.made.lock().unwrap().push(Arc::clone(&inner.seen));
        Ok(inner)
    }
    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
    fn made(&self, i: usize) -> Arc<Seen> {
        Arc::clone(&self.made.lock().unwrap()[i])
    }
}

fn policy(mode: ReconnectMode) -> ReconnectPolicy {
    ReconnectPolicy::default()
        .with_mode(mode)
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO))
}

fn armed(
    initial: Scripted,
    factory: &Factory,
    policy: ReconnectPolicy,
    window: Duration,
) -> ManagedTransport<Scripted> {
    let f = factory.clone();
    ManagedTransport::new(initial, move || f.make(), policy)
        .expect("the policy validates")
        .with_silent_peer_timeout(window)
}

/// Message `n` as bytes.
fn msg(n: u32) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

/// Sends numbered messages from `*next`, a millisecond apart, until `done`
/// holds or the deadline passes. Returns whether `done` held.
fn send_until(
    m: &mut ManagedTransport<Scripted>,
    next: &mut u32,
    mut done: impl FnMut(&mut ManagedTransport<Scripted>) -> bool,
) -> bool {
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while Instant::now() < deadline {
        if done(m) {
            return true;
        }
        let _ = m.send_bytes(&msg(*next));
        *next += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    done(m)
}

const W: Duration = Duration::from_millis(50);

fn a_silent_peer_is_replaced(mode: ReconnectMode) {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let mut m = armed(initial, &factory, policy(mode), W);
    let stats = m.stats_handle();
    let mut next = 0;
    assert!(
        send_until(&mut m, &mut next, |_| factory.calls() == 1),
        "the silent connection was never replaced"
    );
    assert_eq!(old.abandons.load(Ordering::SeqCst), 1);
    assert_eq!(old.closes.load(Ordering::SeqCst), 0);
    assert!(wait_for(scaled(Duration::from_secs(5)), || {
        stats
            .stats()
            .is_some_and(|s| s.reconnect_successes == 1 && !s.reconnecting)
    }));
    let later = next;
    for _ in 0..5 {
        m.send_bytes(&msg(next))
            .expect("the new connection takes it");
        next += 1;
    }
    let new = factory.made(0).log();
    assert!(new.contains(&msg(later)) && new.contains(&msg(next - 1)));
    assert_eq!(factory.calls(), 1);
}

#[test]
fn an_armed_sender_whose_peer_stops_answering_reconnects() {
    a_silent_peer_is_replaced(ReconnectMode::Blocking);
}

#[test]
fn an_armed_sender_whose_peer_stops_answering_reconnects_in_background_mode() {
    a_silent_peer_is_replaced(ReconnectMode::Background);
}

fn the_tripping_message_arrives_once(mode: ReconnectMode) {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let mut m = armed(initial, &factory, policy(mode), W);
    let stats = m.stats_handle();
    let mut next = 0;
    assert!(send_until(&mut m, &mut next, |_| factory.calls() == 1));
    assert!(wait_for(scaled(Duration::from_secs(5)), || {
        stats
            .stats()
            .is_some_and(|s| s.reconnect_successes == 1 && !s.reconnecting && s.gap_len == 0)
    }));
    for _ in 0..3 {
        m.send_bytes(&msg(next)).unwrap();
        next += 1;
    }
    let old = old.log();
    let new = factory.made(0).log();
    // The last message the dead connection accepted is the one whose send
    // found the silence; the new connection starts with it.
    let tripping = old
        .last()
        .expect("the old connection took messages")
        .clone();
    assert_eq!(new.first(), Some(&tripping), "new: {new:?}");
    let mut all: Vec<_> = old.iter().chain(new.iter()).collect();
    let total = all.len();
    all.sort();
    all.dedup();
    assert_eq!(total - all.len(), 1, "only the tripping message is in both");
    assert_eq!(
        new.iter().filter(|b| **b == tripping).count(),
        1,
        "delivered once on the new connection"
    );
    // Nothing lost after the trip: the new connection holds every later one.
    let first_new = u32::from_be_bytes(tripping[..4].try_into().unwrap());
    let expected: Vec<_> = (first_new..next).map(msg).collect();
    assert_eq!(new, expected);
}

#[test]
fn the_message_that_found_the_silence_arrives_once_on_the_new_connection() {
    the_tripping_message_arrives_once(ReconnectMode::Blocking);
}

#[test]
fn the_message_that_found_the_silence_arrives_once_on_the_new_connection_in_background_mode() {
    the_tripping_message_arrives_once(ReconnectMode::Background);
}

#[test]
fn the_abandoned_connection_is_abandoned_not_closed() {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let mut m = armed(initial, &factory, policy(ReconnectMode::Blocking), W);
    let mut next = 0;
    assert!(send_until(&mut m, &mut next, |_| factory.calls() == 1));
    assert_eq!(old.abandons.load(Ordering::SeqCst), 1, "abandoned");
    assert_eq!(old.closes.load(Ordering::SeqCst), 0, "not closed");
    // The wrapper's own close closes the new connection, it does not
    // abandon it.
    m.close();
    let new = factory.made(0);
    assert_eq!(new.closes.load(Ordering::SeqCst), 1);
    assert_eq!(new.abandons.load(Ordering::SeqCst), 0);
}

fn never_reads(m: &mut ManagedTransport<Scripted>, seen: &Seen) {
    for n in 0..200 {
        m.send_bytes(&msg(n)).unwrap();
    }
    assert_eq!(seen.sends.load(Ordering::SeqCst), 200);
    assert_eq!(seen.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn an_unarmed_sender_never_reads_the_peer() {
    let (initial, seen) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let f = factory.clone();
    let mut m = ManagedTransport::new(initial, move || f.make(), policy(ReconnectMode::Blocking))
        .expect("the policy validates");
    never_reads(&mut m, &seen);
}

#[test]
fn a_zero_window_sender_never_reads_the_peer() {
    let (initial, seen) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let mut m = armed(
        initial,
        &factory,
        policy(ReconnectMode::Blocking),
        Duration::ZERO,
    );
    never_reads(&mut m, &seen);
}

#[test]
fn a_transport_that_cannot_tell_never_trips() {
    let window = Duration::from_millis(20);
    let (initial, seen) = Scripted::new(Reading::CannotTell);
    let factory = Factory::default();
    let mut m = armed(initial, &factory, policy(ReconnectMode::Blocking), window);
    let start = Instant::now();
    let mut n = 0;
    while start.elapsed() < window * 10 {
        m.send_bytes(&msg(n)).unwrap();
        n += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(seen.reads.load(Ordering::SeqCst) > 1, "it was read");
    assert_eq!(factory.calls(), 0);
}

#[test]
fn a_boxed_transport_trips_and_is_abandoned_like_a_bare_one() {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    let factory = Factory::default();
    let f = factory.clone();
    let mut m: ManagedTransport<Box<dyn Transport>> = ManagedTransport::new(
        Box::new(initial) as Box<dyn Transport>,
        move || f.make().map(|t| Box::new(t) as Box<dyn Transport>),
        policy(ReconnectMode::Blocking),
    )
    .expect("the policy validates")
    .with_silent_peer_timeout(W);
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    let mut n = 0u32;
    while factory.calls() == 0 && Instant::now() < deadline {
        let _ = m.send_bytes(&msg(n));
        n += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        factory.calls(),
        1,
        "the boxed silent connection was replaced"
    );
    assert_eq!(old.abandons.load(Ordering::SeqCst), 1);
    assert_eq!(old.closes.load(Ordering::SeqCst), 0);
}

#[test]
fn a_trip_at_gap_capacity_zero_reconnects_in_both_modes() {
    for mode in [ReconnectMode::Blocking, ReconnectMode::Background] {
        let (initial, old) = Scripted::new(Reading::Frozen(1));
        let factory = Factory::default();
        let p = policy(mode)
            .with_gap_buffer_capacity(0)
            .with_overflow_policy(OverflowPolicy::Reject);
        let mut m = armed(initial, &factory, p, W);
        let stats = m.stats_handle();
        let deadline = Instant::now() + scaled(Duration::from_secs(5));
        let mut n = 0u32;
        // Until the send that trips: the first after which the factory was
        // called (Blocking), or the first that is refused (Background).
        let tripping = loop {
            assert!(Instant::now() < deadline, "{mode:?}: never tripped");
            let r = m.send_bytes(&msg(n));
            n += 1;
            match mode {
                ReconnectMode::Blocking if factory.calls() == 1 => {
                    assert!(r.is_ok(), "{mode:?}: {r:?}");
                    break n - 1;
                }
                ReconnectMode::Background if r.is_err() => {
                    assert!(
                        matches!(r, Err(TransportError::Backpressure { .. })),
                        "{mode:?}: {r:?}"
                    );
                    break n - 1;
                }
                _ => assert!(r.is_ok(), "{mode:?}: {r:?}"),
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(
            wait_for(scaled(Duration::from_secs(5)), || {
                stats
                    .stats()
                    .is_some_and(|s| s.reconnect_successes == 1 && !s.reconnecting)
            }),
            "{mode:?}: no reconnect"
        );
        assert_eq!(old.log().last(), Some(&msg(tripping)));
        let new = factory.made(0).log();
        match mode {
            ReconnectMode::Blocking => {
                assert_eq!(new, vec![msg(tripping)], "{mode:?}");
                // The send after the reconnect is a sample like any other.
                assert_eq!(factory.made(0).reads.load(Ordering::SeqCst), 1);
            }
            _ => assert!(
                new.is_empty(),
                "{mode:?}: the refused message is the caller's"
            ),
        }
        m.send_bytes(&msg(n)).expect("the new connection takes it");
        assert_eq!(factory.made(0).log().last(), Some(&msg(n)));
    }
}

/// The window no other test uses, so its log lines are this test's.
const WARN_WINDOW: Duration = Duration::from_millis(47);

#[traced_test]
#[test]
fn one_warning_per_trip() {
    let (initial, _) = Scripted::new(Reading::Frozen(1));
    // The first replacement is silent too; the second answers.
    let factory = Factory::with([Scripted::new(Reading::Frozen(1)).0]);
    let mut m = armed(
        initial,
        &factory,
        policy(ReconnectMode::Blocking),
        WARN_WINDOW,
    );
    let mut next = 0;
    assert!(send_until(&mut m, &mut next, |_| factory.calls() == 2));
    // Some more sends on the answering connection: no further line.
    for _ in 0..50 {
        m.send_bytes(&msg(next)).unwrap();
        next += 1;
    }
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    let lines: Vec<String> = String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| {
            l.contains("peer silent: abandoning the connection") && l.contains("window_ms=47 ")
        })
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for l in &lines {
        assert!(l.contains("WARN") && l.contains("unanswered_ms="), "{l}");
    }
}

#[test]
fn an_idle_pause_longer_than_the_window_does_not_trip() {
    let reading = Arc::new(AtomicU64::new(0));
    let (initial, seen) = Scripted::new(Reading::Controlled(Arc::clone(&reading)));
    let factory = Factory::default();
    let mut m = armed(initial, &factory, policy(ReconnectMode::Blocking), W);
    let mut n = 0;
    // The peer answers: the reading moves, and a send reads the new value.
    for _ in 0..3 {
        reading.fetch_add(1, Ordering::SeqCst);
        let before = seen.reads.load(Ordering::SeqCst);
        assert!(send_until(&mut m, &mut n, |_| seen
            .reads
            .load(Ordering::SeqCst)
            > before));
    }
    assert_eq!(factory.calls(), 0);
    // An idle pause of three windows (input, not a synchronisation), then
    // one send with the reading as it was.
    std::thread::sleep(W * 3);
    let before = seen.reads.load(Ordering::SeqCst);
    m.send_bytes(&msg(n)).unwrap();
    assert_eq!(
        seen.reads.load(Ordering::SeqCst),
        before + 1,
        "the send after the pause was read"
    );
    assert_eq!(factory.calls(), 0, "and did not trip");
    reading.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn a_cancel_after_a_trip_does_not_fire_the_abandoned_connection() {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    let initial = initial.with_cancel();
    // No replacement ever comes, so nothing else is published in the slot.
    let mut m = ManagedTransport::new(
        initial,
        || -> Result<Scripted, TransportError> { Err(broken("factory down (test)")) },
        policy(ReconnectMode::Blocking).with_max_attempts(Some(1)),
    )
    .expect("the policy validates")
    .with_silent_peer_timeout(W);
    let cancel = m.cancel_handle().expect("a managed sender has one");
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    let mut n = 0u32;
    while old.abandons.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "never tripped");
        let _ = m.send_bytes(&msg(n));
        n += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    cancel.cancel();
    assert_eq!(
        old.cancels.load(Ordering::SeqCst),
        0,
        "the abandoned connection's wake handle stayed published"
    );
}

#[test]
fn a_trip_inside_the_background_drain_keeps_the_message_and_reconnects() {
    let (initial, old) = Scripted::new(Reading::Frozen(1));
    // The first replacement is silent too, and slow enough that draining
    // the backlog into it takes many windows; each send stays far under
    // half the window, so the drain's own sends never restart the count.
    let (mut slow, _) = Scripted::new(Reading::Frozen(1));
    slow.pause = W / 10;
    let factory = Factory::with([slow]);
    let p = policy(ReconnectMode::Background)
        .with_gap_buffer_capacity(512)
        // The backlog is queued while the worker waits out its first step.
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(200)));
    let mut m = armed(initial, &factory, p, W);
    let stats = m.stats_handle();
    let mut next = 0;
    assert!(send_until(&mut m, &mut next, |_| old
        .abandons
        .load(Ordering::SeqCst)
        == 1));
    for _ in 0..400 {
        m.send_bytes(&msg(next)).expect("queued");
        next += 1;
    }
    assert!(
        wait_for(scaled(Duration::from_secs(5)), || factory.calls() == 2),
        "the drain never gave up on the silent replacement"
    );
    assert!(wait_for(scaled(Duration::from_secs(5)), || {
        stats
            .stats()
            .is_some_and(|s| s.reconnect_successes == 2 && !s.reconnecting && s.gap_len == 0)
    }));
    let second = factory.made(0);
    assert_eq!(second.abandons.load(Ordering::SeqCst), 1);
    assert_eq!(second.closes.load(Ordering::SeqCst), 0);
    let second = second.log();
    let third = factory.made(1).log();
    // The message at the front when the drain tripped was accepted by the
    // silent replacement, kept at the front, and is the third connection's
    // first, once.
    let front = second.last().expect("the drain sent into it").clone();
    assert_eq!(third.first(), Some(&front));
    assert_eq!(third.iter().filter(|b| **b == front).count(), 1);
    let from = u32::from_be_bytes(front[..4].try_into().unwrap());
    assert_eq!(third, (from..next).map(msg).collect::<Vec<_>>());
    assert_eq!(stats.stats().unwrap().gap_messages_dropped, 0);
}

#[test]
fn a_trip_behind_a_full_reject_backlog_restarts_the_worker() {
    let window = Duration::from_millis(30);
    let (mut initial, _) = Scripted::new(Reading::Frozen(1));
    initial.break_after = Some(0);
    // The replacement is silent and drains slower than the producer
    // offers, so its backlog stays full until it trips.
    let (mut slow, _) = Scripted::new(Reading::Frozen(1));
    slow.pause = window / 10;
    let factory = Factory::with([slow]);
    let p = policy(ReconnectMode::Background)
        .with_gap_buffer_capacity(4)
        .with_overflow_policy(OverflowPolicy::Reject);
    let mut m = armed(initial, &factory, p, window);
    let stats = m.stats_handle();
    let mut refused = 0u32;
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    let mut n = 0u32;
    while factory.calls() < 2 {
        assert!(
            Instant::now() < deadline,
            "the silent replacement was never replaced"
        );
        if matches!(
            m.send_bytes(&msg(n)),
            Err(TransportError::Backpressure { .. })
        ) {
            refused += 1;
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(refused > 0, "the backlog was full");
    assert_eq!(factory.made(0).abandons.load(Ordering::SeqCst), 1);
    assert!(wait_for(scaled(Duration::from_secs(5)), || {
        stats
            .stats()
            .is_some_and(|s| s.reconnect_successes == 2 && !s.reconnecting && s.gap_len == 0)
    }));
    m.send_bytes(&msg(n)).expect("delivered");
    assert_eq!(factory.made(1).log().last(), Some(&msg(n)));
}
