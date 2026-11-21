//! The coordination state the managed wrappers share: the interruptible
//! backoff wait ([`Shutdown`]), the closed/cancelled latch pair ([`Latch`]),
//! the send side's worker flags and counters ([`ManagedShared`]) and the one
//! install sequence that follows a successful factory call
//! ([`install_fresh_inner`]).
//!
//! Protocols only: nothing here spawns a worker, calls a factory or decides
//! when to retry. The retry loops stay with their drivers (the inline loop in
//! `reconnect::mod`, the worker in `reconnect::background`, the receive loop
//! in `managed_receive`).
//!
//! The receive wrapper (`ManagedRecvTransport`) uses [`Shutdown`] and only
//! the cancelled half of [`Latch`] (`set_cancelled` / `is_cancelled`): its
//! `closed` and `explicit_close` flags are plain fields of its own, because
//! `recv_bytes` is `&mut self` and no worker shares them.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tst_core::cancel::CancelSlot;
use tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES;
use tst_core::transport::{Transport, TransportError};

use tracing::warn;

use super::silent_peer::{SilentPeer, Verdict};

/// Interruptible sleep. `wait_timeout(dur)` parks up to `dur`, returning
/// early (`true`) if `signal()` fired. A poisoned mutex reads as signaled
/// — conservative shutdown. Used by BOTH reconnect modes so that
/// `close()` / `cancel()` / `Drop` interrupt a backoff wait immediately
/// instead of waiting out a full `thread::sleep`.
pub(crate) struct Shutdown {
    flagged: Mutex<bool>,
    cv: Condvar,
}

impl Shutdown {
    pub(crate) fn new() -> Self {
        Self {
            flagged: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn signal(&self) {
        if let Ok(mut f) = self.flagged.lock() {
            *f = true;
        }
        // On poison: waiters treat poison as signaled, so notify is enough.
        self.cv.notify_all();
    }

    /// Returns true if shutdown was signaled (or the lock is poisoned);
    /// false if the full duration elapsed without a signal.
    pub(crate) fn wait_timeout(&self, dur: Duration) -> bool {
        // checked_add: Duration::MAX must not panic (same class as the
        // v0.5.0 checked-timeout-arithmetic fix). None => wait unbounded.
        let deadline = Instant::now().checked_add(dur);
        let Ok(mut flagged) = self.flagged.lock() else {
            return true;
        };
        while !*flagged {
            match deadline {
                Some(d) => {
                    let Some(remaining) = d.checked_duration_since(Instant::now()) else {
                        return false;
                    };
                    match self.cv.wait_timeout(flagged, remaining) {
                        Ok((guard, _)) => flagged = guard,
                        Err(_) => return true,
                    }
                }
                None => match self.cv.wait(flagged) {
                    Ok(guard) => flagged = guard,
                    Err(_) => return true,
                },
            }
        }
        true
    }
}

/// The two flags every managed wrapper shares between its caller, its
/// cancel handle and (Background mode) its worker thread.
///
/// - `closed`: latched true by `cancel()`, `close()` and `Drop` (the one
///   terminal transition). Every send path and both reconnect loops exit
///   at their next check.
/// - `cancelled`: latched ONLY by a cancel, never by `close()` or `Drop`,
///   so a latched-closed wrapper can say which close it was (`ExplicitClose`
///   for a cancel, `Closed` for its own close).
pub(crate) struct Latch {
    closed: AtomicBool,
    cancelled: AtomicBool,
}

impl Latch {
    pub(crate) fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Record a cancel without the rest of the terminal transition (the
    /// receive twin has no `closed` atomic of its own).
    pub(crate) fn set_cancelled(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// The one terminal transition shared by `ManagedTransport::close`,
    /// its `Drop`, and the cancel handle:
    ///
    /// 1. latch `closed` — every send path and both reconnect loops exit at
    ///    their next check;
    /// 2. wake a backoff wait (either mode) — a no-op when nothing waits;
    /// 3. fire the live inner's wake handle out of the `active` slot — the
    ///    ONLY thing that returns a worker parked inside an inner
    ///    `send_bytes` (the shutdown signal never reaches a transport call).
    ///    The handle comes from the slot, never from the `inner` mutex:
    ///    every send path holds `inner` across the inner `send_bytes`, so a
    ///    cancel that read the inner out of that mutex would queue behind
    ///    the very call it was asked to interrupt (locking invariant 5).
    ///    The slot latches, so an inner installed after this point (a
    ///    reconnect was mid-flight) is cancelled on arrival instead of
    ///    being parked on.
    ///
    /// Takes no lock of its own and fires the target outside the slot's
    /// lock, so it is safe from `Drop` provided the target honours the
    /// `TransportCancel` contract (an interrupt, not a wait).
    pub(crate) fn terminal(&self, shutdown: &Shutdown, active: &CancelSlot) {
        self.closed.store(true, Ordering::Release);
        shutdown.signal();
        active.cancel();
    }

    /// A caller's cancel: latch the cancel FIRST, then the shared terminal
    /// transition, so a send woken by step 3 already reads the reason as
    /// the cancel.
    pub(crate) fn cancel(&self, shutdown: &Shutdown, active: &CancelSlot) {
        self.set_cancelled();
        self.terminal(shutdown, active);
    }

    /// The terminal transition for an inner transport (or the factory) that
    /// reported `ExplicitClose`, shared by the inline paths and the
    /// background worker. Called with neither lock held (locking
    /// invariant 5).
    ///
    /// If the wrapper is already latched, its own `close()` / `Drop` /
    /// `cancel()` fired the inner's wake handle and the inner is reporting
    /// exactly that: the reason is recorded and stays as it is (`Closed`
    /// for an own close). Otherwise the inner was cancelled from outside
    /// the wrapper, which is recorded as a cancel, in `cancel`'s order.
    pub(crate) fn inner_cancel(&self, shutdown: &Shutdown, active: &CancelSlot) {
        if self.is_closed() {
            return;
        }
        self.cancel(shutdown, active);
    }

    /// The error a latched-closed wrapper reports: the cancel if one fired,
    /// else the wrapper's own `close()`/`Drop`. Every `closed`-latch exit
    /// goes through this — the latch alone cannot tell the two apart
    /// because `close()`, `Drop` and `cancel()` share `terminal`.
    pub(crate) fn error(&self) -> TransportError {
        if self.is_cancelled() {
            TransportError::ExplicitClose
        } else {
            TransportError::Closed
        }
    }
}

/// State shared between `ManagedTransport`, its `ManagedStatsHandle`
/// observers, and any spawned background worker (`background::worker_run`).
///
/// Three of the counters are themselves `Arc`s although this whole struct
/// already lives in one: `ManagedTransport::{attempts,reconnects,
/// reconnecting}_handle` hand those three out individually, so a binding
/// can poll them after the transport has moved into a sender shell without
/// holding — or knowing about — the rest of this state. Every
/// use site reads through `Deref` unchanged.
#[derive(Debug)]
pub(crate) struct ManagedShared {
    /// True while a background worker owns reconnect+drain.
    /// Transitions happen under the gap lock (invariant 2).
    pub(crate) bg_active: Arc<AtomicBool>,
    /// Set by a worker that exhausted max_attempts; consumed (swap false)
    /// by the next send_bytes, which reports Broken exactly once.
    pub(crate) gave_up: AtomicBool,
    /// Set alongside `gave_up` when the worker terminated abnormally
    /// (unwind, or a poisoned lock it can't recover from) rather than via
    /// the normal budget-exhausted path. Consumed the same way, and
    /// changes the reported message so a `max_attempts: None` policy
    /// doesn't claim "gave up after 0 attempts" for a crash.
    pub(crate) gave_up_abnormal: AtomicBool,
    /// factory() invocations (either mode).
    pub(crate) reconnect_attempts: Arc<AtomicU64>,
    /// Successful factory() installs (either mode).
    pub(crate) reconnect_successes: Arc<AtomicU64>,
    /// The last installed inner's `max_payload()`, published at
    /// construction and on every successful install so neither
    /// `send_bytes`'s size pre-check nor `ManagedTransport::max_payload()`
    /// (which `RawSender`/`MuxSender` call on every send) has to take the
    /// `inner` lock — the drain worker holds that across one unbounded
    /// inner send. A value that went stale between a read and the drain
    /// is benign: the drain's own `TooLarge` handling drops a queued
    /// message the rebuilt transport can no longer carry.
    pub(crate) max_payload: AtomicUsize,
    /// An armed sender's attempts spent in the current outage on
    /// connections that never answered: where the next reconnect cycle's
    /// budget starts. Zeroed by an answer and by a give-up. Read and
    /// written only when silent-peer detection is armed.
    pub(crate) unanswered_attempts: AtomicU32,
}

impl Default for ManagedShared {
    fn default() -> Self {
        Self {
            bg_active: Arc::new(AtomicBool::new(false)),
            gave_up: AtomicBool::new(false),
            gave_up_abnormal: AtomicBool::new(false),
            reconnect_attempts: Arc::new(AtomicU64::new(0)),
            reconnect_successes: Arc::new(AtomicU64::new(0)),
            // Hand-written solely for this field: a derived `Default`
            // would seed the ceiling at 0, which reads as "refuse every
            // send". `ManagedTransport::new` overwrites it with the
            // initial inner's real ceiling immediately, so this constant
            // is only ever the pre-construction placeholder.
            max_payload: AtomicUsize::new(SRT_TS_BUNDLE_BYTES),
            unanswered_attempts: AtomicU32::new(0),
        }
    }
}

/// The transport's own error code of a failed factory call, for the log
/// line of that attempt. Read from `Broken` only: that is how a factory
/// reports a dial that was refused.
pub(crate) fn broken_errno(e: &TransportError) -> Option<i32> {
    match e {
        TransportError::Broken { errno_code, .. } => *errno_code,
        _ => None,
    }
}

/// A connection the silent-peer rule gave up on: its window and how long
/// its sends went unanswered, for the retire's log line.
pub(crate) struct Trip {
    pub(crate) window: Duration,
    pub(crate) unanswered: Duration,
}

/// The silent-peer sample after an inner send returned `Ok`, with `inner`
/// held: the clock, then the inner's reading when the rule asks for one.
/// The rule's lock is a leaf (locking invariant 6), so it is released
/// before the reading is taken. `Some` when the peer has stopped answering.
/// The inner's first answer gives the reconnect budget back.
pub(crate) fn sample_after_send<T: Transport>(
    silent: &Mutex<SilentPeer>,
    transport: &T,
    shared: &ManagedShared,
) -> Option<Trip> {
    let now = Instant::now();
    let lock = || silent.lock().unwrap_or_else(PoisonError::into_inner);
    if !lock().sent(now) {
        return None;
    }
    let reading = transport.peer_activity();
    let mut rule = lock();
    match rule.reading(now, reading) {
        Verdict::Silent { unanswered } => Some(Trip {
            window: rule.window(),
            unanswered,
        }),
        Verdict::Answered => {
            shared.unanswered_attempts.store(0, Ordering::Relaxed);
            None
        }
        Verdict::Healthy => None,
    }
}

/// Close a connection the silent-peer rule gave up on, without waiting for
/// its unsent data, and say so. Called with no lock held.
pub(crate) fn retire_silent<T: Transport>(mut old: T, trip: Trip) {
    old.abandon();
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    warn!(
        target: "tst_pipeline::reconnect",
        window_ms = ms(trip.window),
        unanswered_ms = ms(trip.unanswered),
        "peer silent: abandoning the connection",
    );
}

/// Outcome of [`install_fresh_inner`].
pub(crate) enum Install {
    /// The fresh inner is installed, its wake handle published, and the
    /// success counted. The caller may drain into it.
    Installed,
    /// A cancel/close latched `closed` while the factory ran: the fresh
    /// inner was installed (so the latched slot fired its handle), then
    /// taken back out and closed outside the lock. Nothing was counted
    /// and nothing may be drained.
    Closed,
    /// The inner lock is poisoned; nothing was installed. Each caller
    /// reports this in its own idiom (inline: `Broken`; worker: abnormal
    /// give-up).
    InnerPoisoned,
}

/// The one post-factory install sequence shared by the inline path
/// (`ManagedTransport::reconnect_and_drain`) and the worker
/// (`background::worker_run`), so a
/// cancel that lands while the factory runs is answered identically on
/// both: take the wake handle → install under `inner` → publish the
/// handle after the lock drops → honour a latched close (close the fresh
/// inner OUTSIDE the lock) → count the success and publish the fresh
/// ceiling. Before this the worker counted the success first and left the
/// (already socket-cancelled) fresh inner installed; the inline path
/// neither counted nor kept it.
///
/// An armed sender's silent-peer rule (`silent`) is reset in the same
/// `inner` critical section that installs the fresh inner, so no sample
/// ever measures the new inner against the old one's run.
pub(crate) fn install_fresh_inner<T: Transport>(
    inner: &Mutex<Option<T>>,
    active: &CancelSlot,
    latch: &Latch,
    shared: &ManagedShared,
    silent: Option<&Mutex<SilentPeer>>,
    new_inner: T,
) -> Install {
    // Take the wake handle (and read the ceiling) before the transport
    // moves into the mutex; publish the handle after the lock drops, so
    // the slot's own firing (a cancel that landed while the factory was
    // building) never runs under `inner`.
    let new_cancel = new_inner.cancel_handle();
    let new_max_payload = new_inner.max_payload();
    {
        let Ok(mut guard) = inner.lock() else {
            return Install::InnerPoisoned;
        };
        *guard = Some(new_inner);
        if let Some(silent) = silent {
            // A leaf lock under `inner` (locking invariant 6).
            silent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .reset();
        }
    }
    if let Some(h) = new_cancel {
        active.install(h);
    }
    // Honor a cancel that landed while the factory ran. The slot latched,
    // so the install above already fired the fresh inner's wake handle;
    // without this check the drain would still write the gap buffer
    // through a connection the caller has asked to abandon (and a real
    // socket, closed by that cancel, would turn the caller-initiated close
    // into a wire-looking `Broken`).
    if latch.is_closed() {
        // Recover on poison rather than skip: the slot is a plain `Option`
        // a panic can never leave half-updated, and the fresh inner must
        // be closed on this path regardless of how an earlier holder
        // exited.
        let fresh = inner.lock().unwrap_or_else(|p| p.into_inner()).take();
        // Close outside the lock: an inner's close may block (libsrt
        // lingers) and must never hold `inner` while it does — the same
        // rule every send path follows.
        if let Some(mut t) = fresh {
            t.close();
        }
        return Install::Closed;
    }
    // Publish the fresh ceiling for the lock-free size pre-check. Only on
    // the Installed path: a fresh inner that was closed above never
    // becomes the one a send is checked against.
    shared.max_payload.store(new_max_payload, Ordering::Relaxed);
    // `Release` makes this increment the publication point for the ceiling
    // stored just above: a thread that observes the bumped counter through
    // the matching `Acquire` load in `ManagedTransportStats` also observes
    // the new ceiling. Callers do wait on `reconnect_successes` and then
    // send a message sized against the rebuilt ceiling, and with both
    // accesses `Relaxed` a weakly-ordered target could pair the new count
    // with the old ceiling and reject that send as `TooLarge`.
    shared.reconnect_successes.fetch_add(1, Ordering::Release);
    Install::Installed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> (Latch, Shutdown, CancelSlot) {
        (Latch::new(), Shutdown::new(), CancelSlot::new())
    }

    #[test]
    fn fresh_latch_is_open() {
        let (l, _, _) = parts();
        assert!(!l.is_closed() && !l.is_cancelled());
    }

    #[test]
    fn cancel_then_close_reports_explicit_close() {
        let (l, s, a) = parts();
        l.cancel(&s, &a);
        l.terminal(&s, &a);
        assert!(l.is_closed() && l.is_cancelled());
        assert!(matches!(l.error(), TransportError::ExplicitClose));
    }

    #[test]
    fn close_then_cancel_reports_closed() {
        let (l, s, a) = parts();
        l.terminal(&s, &a);
        l.inner_cancel(&s, &a);
        assert!(l.is_closed() && !l.is_cancelled());
        assert!(matches!(l.error(), TransportError::Closed));
    }

    #[test]
    fn inner_cancel_on_open_latch_is_a_cancel() {
        let (l, s, a) = parts();
        l.inner_cancel(&s, &a);
        assert!(l.is_closed() && l.is_cancelled());
        assert!(matches!(l.error(), TransportError::ExplicitClose));
    }

    #[test]
    fn terminal_fires_the_slot() {
        let (l, s, a) = parts();
        l.terminal(&s, &a);
        assert!(a.is_cancelled());
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn wait_timeout_elapses_without_signal() {
        let s = Shutdown::new();
        let t0 = Instant::now();
        assert!(!s.wait_timeout(Duration::from_millis(50)));
        assert!(t0.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn signal_interrupts_wait_promptly() {
        let s = Arc::new(Shutdown::new());
        let s2 = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            s2.signal();
        });
        let t0 = Instant::now();
        assert!(
            s.wait_timeout(Duration::from_secs(30)),
            "must report signaled"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "must not wait out the 30s"
        );
        h.join().unwrap();
    }

    #[test]
    fn signal_before_wait_returns_immediately() {
        let s = Shutdown::new();
        s.signal();
        assert!(s.wait_timeout(Duration::from_secs(30)));
    }
}
