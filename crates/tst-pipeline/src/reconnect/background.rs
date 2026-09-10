//! Background-reconnect machinery for `ManagedTransport`:
//! the interruptible-wait primitive, the shared worker flags/counters,
//! and the worker's reconnect + drain state machine.
//!
//! Locking invariants (shared with `send_managed` in mod.rs):
//! 1. Lock order where both are held: `inner` -> `gap`. Never the reverse.
//! 2. `bg_active` transitions and the send-path enqueue decision happen
//!    under the `gap` lock, so worker exit and pump enqueue linearize.
//! 3. `spawn_worker` is never called while holding the `gap` lock.
//! 4. The worker holds `gap` across one inner send during drain — this
//!    pins the front message so a concurrent `DropOldest` eviction can't
//!    pop the message in flight (clone-then-pop would desync the queue).
//! 5. The worker publishes each fresh inner's wake handle into the shared
//!    `active` cancel slot (and clears it on tear-down) so a cancel can
//!    reach a drain send without taking `inner` — see the same invariant
//!    in `reconnect::mod`'s type docs.

use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};

use super::{GapBuffer, ReconnectPolicy};

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

/// State shared between `ManagedTransport`, its `ManagedStatsHandle`
/// observers, and any spawned background worker (see `worker_run` below).
#[derive(Debug, Default)]
pub(crate) struct ManagedShared {
    /// True while a background worker owns reconnect+drain.
    /// Transitions happen under the gap lock (invariant 2).
    pub(crate) bg_active: AtomicBool,
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
    pub(crate) reconnect_attempts: AtomicU64,
    /// Successful factory() installs (either mode).
    pub(crate) reconnect_successes: AtomicU64,
}

/// Backpressure retry cadence while draining on the worker — there is no
/// caller to propagate to, so the worker absorbs it. Interruptible.
const DRAIN_BACKPRESSURE_RETRY: Duration = Duration::from_millis(20);

/// Everything the worker thread owns. All `Arc`s — the worker never
/// borrows from `ManagedTransport`, so `Drop` can detach it safely.
pub(crate) struct WorkerCtx<T: Transport> {
    pub(crate) inner: Arc<Mutex<Option<T>>>,
    pub(crate) factory: Arc<dyn Fn() -> Result<T, TransportError> + Send + Sync>,
    pub(crate) gap: Arc<Mutex<GapBuffer>>,
    pub(crate) closed: Arc<AtomicBool>,
    pub(crate) shutdown: Arc<Shutdown>,
    pub(crate) shared: Arc<ManagedShared>,
    pub(crate) policy: ReconnectPolicy,
    /// The wrapper's cancel slot: the worker publishes each inner it
    /// installs and clears it when one is torn down, so a cancel reaches a
    /// parked drain send without touching `inner` (invariant 5).
    pub(crate) active: Arc<CancelSlot>,
}

enum DrainStep {
    Sent,
    Empty,
    Backpressure,
    Broken,
}

/// Clears `bg_active` (invariant 2) when `worker_run` exits — including an
/// **unwind**: a user-supplied `factory()` panicking (e.g. `unwrap()` on
/// DNS/socket setup), or the drain phase's poisoned-gap `.expect()`.
///
/// Without this, a panicking worker leaves `bg_active` stuck `true`
/// forever: every subsequent `send_bytes` takes the send gate's
/// worker-active branch (enqueue, return `Ok`) forever, no replacement
/// worker can ever spawn (`spawn_worker` requires `!bg_active`), and
/// `is_alive()` reports `true` unconditionally — with `DropOldest` the
/// gap buffer never fills, so this is unbounded silent loss reported as
/// healthy, exactly the class of stall this feature exists to eliminate.
///
/// Constructed once at the top of `worker_run` so it covers every exit
/// path (normal `return` or unwind) via `Drop`.
///
/// The Empty-protocol exit (drain phase, gap goes empty) still clears
/// `bg_active` in place under the gap lock it's already holding, for the
/// same-critical-section linearization with the send gate (invariant 2).
/// That in-place clear also **hands ownership of `bg_active` away** from
/// this worker: the send gate may immediately enqueue a fresh break and
/// spawn a brand-new worker (setting `bg_active = true` again) before this
/// thread's `Drop` runs — `spawn_worker`'s `prev.join()` waits on exactly
/// this `Drop`, so the old worker's stack can still be unwinding (or just
/// finishing its `return`) while a newer cycle is already live. If `Drop`
/// then stored `bg_active = false` unconditionally, it would clobber that
/// newer cycle's `true` — the newest worker would run "unowned" (no one
/// believes it's active), a subsequent gate call would spawn yet another
/// worker on top of it, and `spawn_worker`'s join on the still-live worker
/// could hang indefinitely under `max_attempts: None`. The `skip` flag,
/// set by the Empty-exit in that same critical section, tells `Drop` "you
/// no longer own `bg_active` — do not touch it again": once ownership has
/// been handed off, a re-clear here is not idempotent, it's a clobber.
struct ActiveClearGuard {
    gap: Arc<Mutex<GapBuffer>>,
    shared: Arc<ManagedShared>,
    /// Set true (under the gap lock, from the Empty-exit's own critical
    /// section) once `bg_active` has already been cleared in place and
    /// ownership handed off. `Drop` checks this under the same lock, so
    /// the set-site and the check-site linearize.
    skip: AtomicBool,
}

impl Drop for ActiveClearGuard {
    fn drop(&mut self) {
        // Take the gap lock purely to linearize this clear (or no-op)
        // with the send gate (invariant 2) — the operations below don't
        // touch the gap buffer, so this reads like unlocked state, but
        // `guard` staying alive through the whole body is what makes it
        // safe. Poisoned gap: acquire anyway (the Result's poisoned arm
        // still embeds — and holds — the underlying MutexGuard) and
        // proceed regardless; a panic elsewhere is already unwinding.
        let guard = self.gap.lock();
        if self.skip.load(Ordering::Acquire) {
            // Ownership already handed off by the Empty-exit (see the
            // struct doc) — this worker must not write bg_active again.
            // A panic can't reach here: the Empty branch returns
            // immediately after setting skip, with nothing left running.
            drop(guard);
            return;
        }
        if std::thread::panicking() {
            // Unwinding: no normal exit path ran, so nobody reported a
            // give-up. Report an abnormal one so the next send_bytes
            // surfaces Broken instead of silently queuing forever.
            self.shared.gave_up_abnormal.store(true, Ordering::Release);
            self.shared.gave_up.store(true, Ordering::Release);
        }
        self.shared.bg_active.store(false, Ordering::Release);
        drop(guard);
    }
}

/// One outage's worth of reconnect + drain. Spawned on break, exits when
/// the gap fully drains (Empty protocol), the budget exhausts (give-up),
/// or shutdown is signaled.
pub(crate) fn worker_run<T: Transport>(ctx: WorkerCtx<T>) {
    // Cleared on every exit path via Drop — including an unwind. See
    // ActiveClearGuard's doc comment for why that matters (and for the
    // `skip` flag the Empty-exit below sets).
    let active_guard = ActiveClearGuard {
        gap: Arc::clone(&ctx.gap),
        shared: Arc::clone(&ctx.shared),
        skip: AtomicBool::new(false),
    };
    // The budget covers ONE continuous outage: reset after each
    // successful install. `max_attempts` bounds attempts per outage, not
    // per transport lifetime — matching Blocking, where every
    // `send_bytes` call ran a fresh `reconnect_and_drain` budget.
    let mut attempt: u32 = 0;
    'reconnect: loop {
        if ctx.closed.load(Ordering::Acquire) {
            return;
        }
        attempt += 1;
        let Some(wait) = ctx.policy.next_delay(attempt) else {
            let max = ctx.policy.max_attempts.unwrap_or(0);
            warn!(
                target: "tst_pipeline::reconnect",
                attempts_made = attempt - 1,
                max_attempts = max,
                "background reconnect gave up — next send_bytes reports Broken",
            );
            // gave_up before returning: keeps the give-up cycle's report
            // deterministic for the send path's swap-consume (the guard
            // clears bg_active afterward, on the way out).
            ctx.shared.gave_up.store(true, Ordering::Release);
            return;
        };
        info!(
            target: "tst_pipeline::reconnect",
            attempt,
            max_attempts = ctx.policy.max_attempts.unwrap_or(0),
            backoff_ms = wait.as_millis() as u64,
            "background reconnect attempt",
        );
        if ctx.shutdown.wait_timeout(wait) {
            return;
        }
        ctx.shared
            .reconnect_attempts
            .fetch_add(1, Ordering::Relaxed);
        let new_inner = match (ctx.factory)() {
            Ok(t) => t,
            Err(_) => continue 'reconnect,
        };
        // Take the wake handle before the transport moves into the mutex;
        // publish it after the lock drops, so the slot's own firing (a
        // cancel that landed while the factory was building) never runs
        // under `inner`.
        let new_cancel = new_inner.cancel_handle();
        {
            let Ok(mut guard) = ctx.inner.lock() else {
                // Inner lock poisoned — unrecoverable from a worker with
                // no caller. Surface as an abnormal give-up so the next
                // send reports Broken instead of queuing forever. Store
                // the abnormal flag FIRST, matching the guard's Drop
                // order (Finding A): otherwise a send_bytes landing
                // between the two stores could observe gave_up = true
                // with gave_up_abnormal still false and report the wrong
                // (budget) message for this poison abort.
                ctx.shared.gave_up_abnormal.store(true, Ordering::Release);
                ctx.shared.gave_up.store(true, Ordering::Release);
                return;
            };
            *guard = Some(new_inner);
        }
        if let Some(h) = new_cancel {
            ctx.active.install(h);
        }
        ctx.shared
            .reconnect_successes
            .fetch_add(1, Ordering::Relaxed);
        attempt = 0; // fresh budget for any subsequent break

        // ---- drain phase ----
        loop {
            if ctx.closed.load(Ordering::Acquire) {
                return;
            }
            // Per-message lock scope, order inner -> gap (invariant 1).
            // The gap lock is held across this one send on purpose
            // (invariant 4): it pins the front message so a concurrent
            // DropOldest eviction can't pop the message in flight. The
            // pump blocks on the gap lock for at most one inner send.
            let step = {
                let Ok(mut transport_guard) = ctx.inner.lock() else {
                    // Inner lock poisoned mid-drain — same abnormal
                    // give-up (and the same abnormal-first store order,
                    // Finding A) as the reconnect-phase poison path above.
                    ctx.shared.gave_up_abnormal.store(true, Ordering::Release);
                    ctx.shared.gave_up.store(true, Ordering::Release);
                    return;
                };
                let mut gap = ctx
                    .gap
                    .lock()
                    .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                if gap.front().is_none() {
                    // Empty protocol: clear active while STILL holding the
                    // gap lock — the send gate checks bg_active under this
                    // same lock, so it can never enqueue into a
                    // worker-less buffer (invariant 2). Mark the guard
                    // skip-on-drop in this SAME critical section: from
                    // this point on, bg_active belongs to whatever the
                    // send gate does next (possibly a brand-new worker),
                    // not to this one — see ActiveClearGuard's doc for
                    // why an unconditional re-clear on Drop would clobber
                    // that ownership handoff (Finding B).
                    ctx.shared.bg_active.store(false, Ordering::Release);
                    active_guard.skip.store(true, Ordering::Release);
                    DrainStep::Empty
                } else if let Some(transport) = transport_guard.as_mut() {
                    let msg = gap.front().expect("checked non-empty above");
                    match transport.send_bytes(msg) {
                        Ok(()) => {
                            gap.pop_front();
                            DrainStep::Sent
                        }
                        Err(TransportError::Backpressure { .. }) => DrainStep::Backpressure,
                        Err(TransportError::TooLarge { len, max }) => {
                            // The rebuilt inner's ceiling shrank below a
                            // queued message. With no caller to bounce it
                            // to, keeping it would wedge the drain forever
                            // — drop it, count it, keep going.
                            if let Some(dropped) = gap.pop_front() {
                                gap.bytes_dropped += dropped.len() as u64;
                                gap.messages_dropped += 1;
                            }
                            warn!(
                                target: "tst_pipeline::reconnect",
                                len,
                                max,
                                "dropping queued message larger than the reconnected transport's max_payload",
                            );
                            DrainStep::Sent
                        }
                        Err(_) => {
                            // Broken / Closed / unknown-future — rebuild.
                            // Front message stays queued for the retry.
                            // Un-publish the dead inner's wake handle with
                            // the inner it belongs to; the next successful
                            // install republishes.
                            *transport_guard = None;
                            ctx.active.clear();
                            DrainStep::Broken
                        }
                    }
                } else {
                    // Inner vanished (only the worker clears it — belt and
                    // braces for future refactors): treat as broken.
                    DrainStep::Broken
                }
            };
            match step {
                DrainStep::Sent => continue,
                DrainStep::Empty => return, // active already cleared under the gap lock
                DrainStep::Backpressure => {
                    if ctx.shutdown.wait_timeout(DRAIN_BACKPRESSURE_RETRY) {
                        return;
                    }
                }
                DrainStep::Broken => continue 'reconnect,
            }
        }
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
