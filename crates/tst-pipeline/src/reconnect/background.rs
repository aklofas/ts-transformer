//! The background reconnect worker of `ManagedTransport`: its reconnect +
//! drain state machine. The coordination state it shares with the wrapper
//! (the interruptible wait, the latch, the worker flags and counters, the
//! install sequence) lives in `reconnect::state`.
//!
//! Locking invariants (shared with `send_managed` in mod.rs):
//! 1. Lock order where both are held: `inner` -> `gap`. Never the reverse.
//! 2. `bg_active` transitions and the send-path enqueue decision happen
//!    under the `gap` lock, so worker exit and pump enqueue linearize.
//! 3. `spawn_worker` is never called while holding the `gap` lock.
//! 4. The worker NEVER holds `gap` across an inner send, and neither does
//!    the `Blocking` drain (`drain_gap_if_alive`). Each takes the lock
//!    only for the buffer's own critical sections: mark the front
//!    message in flight (`begin_send`), release, send with just `inner`
//!    held, then re-take it to settle (`finish_send` / `abort_send`).
//!    The in-flight message is pinned by the buffer's `in_flight` mark
//!    — `DropOldest` eviction skips it — not by the lock. That matters
//!    because one inner send is unbounded against a peer that stops
//!    draining (tst-tcp's write loop, SRT's default `send_timeout:
//!    None`): holding `gap` there stalled both `stats()` and the
//!    producer's enqueue for as long as the peer sulked. `inner` IS
//!    held across that send, so the producer's path must not take it
//!    either — both the size pre-check and `max_payload()` (which every
//!    sender shell calls per send) read `ManagedShared::max_payload`.
//! 5. The worker publishes each fresh inner's wake handle into the shared
//!    `active` cancel slot (and clears it on tear-down) so a cancel can
//!    reach a drain send without taking `inner` — see the same invariant
//!    in `reconnect::mod`'s type docs.
//! 6. `reconnect_successes` is the publication point for the ceiling of
//!    invariant 4: `install_fresh_inner` stores `max_payload` and then
//!    increments the counter with `Release`, and `stats()` loads it with
//!    `Acquire`. So a caller that waits for the count to move and then
//!    sends is guaranteed the rebuilt ceiling, not the previous inner's
//!    — which `Relaxed` on both would not give on a weakly-ordered
//!    target.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};

use super::silent_peer::SilentPeer;
use super::state::{Install, Latch, ManagedShared, Shutdown, broken_errno, install_fresh_inner};
use super::{GapBuffer, ReconnectPolicy};

/// Backpressure retry cadence while draining on the worker — there is no
/// caller to propagate to, so the worker absorbs it. Interruptible.
const DRAIN_BACKPRESSURE_RETRY: Duration = Duration::from_millis(20);

/// Everything the worker thread owns. All `Arc`s — the worker never
/// borrows from `ManagedTransport`, so `Drop` can detach it safely.
pub(crate) struct WorkerCtx<T: Transport> {
    pub(crate) inner: Arc<Mutex<Option<T>>>,
    pub(crate) factory: Arc<dyn Fn() -> Result<T, TransportError> + Send + Sync>,
    pub(crate) gap: Arc<Mutex<GapBuffer>>,
    /// The wrapper's closed/cancelled latch; the worker records an inner's
    /// (or the factory's) `ExplicitClose` on it via `Latch::inner_cancel`.
    pub(crate) latch: Arc<Latch>,
    pub(crate) shutdown: Arc<Shutdown>,
    pub(crate) shared: Arc<ManagedShared>,
    pub(crate) policy: ReconnectPolicy,
    /// The wrapper's cancel slot: the worker publishes each inner it
    /// installs and clears it when one is torn down, so a cancel reaches a
    /// parked drain send without touching `inner` (invariant 5).
    pub(crate) active: Arc<CancelSlot>,
    /// The wrapper's silent-peer rule, when it is armed.
    pub(crate) silent: Option<Arc<Mutex<SilentPeer>>>,
}

enum DrainStep {
    Sent,
    Empty,
    Backpressure,
    Broken,
    /// The inner reported `ExplicitClose`: terminal, no rebuild.
    Cancelled,
}

/// What the drain step's first (gap-locked) phase decided. Split out so
/// the gap lock is released before the inner send runs (invariant 4).
enum DrainPlan {
    /// Gap is empty — the Empty protocol already ran under the gap lock.
    Empty,
    /// No live inner to drain into; nothing was marked in flight.
    NoInner,
    /// The front message is marked in flight and must be settled with
    /// `finish_send`/`abort_send` once the send returns.
    Send { seq: u64, msg: Vec<u8> },
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
/// shutdown is signaled, or the inner or the factory reports
/// `ExplicitClose` (which latches the wrapper; the next `send_bytes`
/// reports it, and the backlog stays queued).
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
    // Why the latest factory call of this outage failed, for the give-up
    // line.
    let mut last_error: Option<TransportError> = None;
    'reconnect: loop {
        if ctx.latch.is_closed() {
            return;
        }
        attempt += 1;
        let Some(wait) = ctx.policy.next_delay(attempt) else {
            let max = ctx.policy.max_attempts.unwrap_or(0);
            warn!(
                target: "tst_pipeline::reconnect",
                attempts_made = attempt - 1,
                max_attempts = max,
                last_error = last_error.as_ref().map(tracing::field::display),
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
            // Recorded only when there is a limit.
            max_attempts = ctx.policy.max_attempts,
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
            Err(TransportError::ExplicitClose) => {
                // The dial was cancelled, not refused: stop dialling. No
                // lock is held here.
                ctx.latch.inner_cancel(&ctx.shutdown, &ctx.active);
                return;
            }
            Err(e) => {
                warn!(
                    target: "tst_pipeline::reconnect",
                    attempt,
                    max_attempts = ctx.policy.max_attempts,
                    errno_code = broken_errno(&e),
                    error = %e,
                    "background reconnect attempt failed",
                );
                last_error = Some(e);
                continue 'reconnect;
            }
        };
        match install_fresh_inner(
            &ctx.inner,
            &ctx.active,
            &ctx.latch,
            &ctx.shared,
            ctx.silent.as_deref(),
            new_inner,
        ) {
            Install::Installed => {}
            // close()/cancel() landed while the factory ran: the fresh
            // inner is already closed and nothing may be drained into it.
            // The guard clears bg_active on the way out.
            Install::Closed => return,
            Install::InnerPoisoned => {
                // Inner lock poisoned — unrecoverable from a worker with
                // no caller. Surface as an abnormal give-up so the next
                // send reports Broken instead of queuing forever. Store
                // the abnormal flag FIRST, matching the guard's Drop
                // order: otherwise a send_bytes landing
                // between the two stores could observe gave_up = true
                // with gave_up_abnormal still false and report the wrong
                // (budget) message for this poison abort.
                ctx.shared.gave_up_abnormal.store(true, Ordering::Release);
                ctx.shared.gave_up.store(true, Ordering::Release);
                return;
            }
        }
        attempt = 0; // fresh budget for any subsequent break
        last_error = None;

        // ---- drain phase ----
        loop {
            if ctx.latch.is_closed() {
                return;
            }
            // Per-message lock scope, order inner -> gap (invariant 1).
            // `inner` is held across the send (the transport needs
            // `&mut`); `gap` is NOT (invariant 4) — it is taken once to
            // mark the front message in flight, dropped for the duration
            // of the send, and re-taken to settle. One inner send is
            // unbounded against a peer that stops draining, so neither
            // the producer's enqueue nor `stats()` may be queued behind
            // it.
            let step = {
                let Ok(mut transport_guard) = ctx.inner.lock() else {
                    // Inner lock poisoned mid-drain — same abnormal
                    // give-up (and the same abnormal-first store order) as
                    // the reconnect-phase poison path above.
                    ctx.shared.gave_up_abnormal.store(true, Ordering::Release);
                    ctx.shared.gave_up.store(true, Ordering::Release);
                    return;
                };
                let plan = {
                    let mut gap = ctx
                        .gap
                        .lock()
                        .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                    if gap.is_empty() {
                        // Empty protocol: clear active while STILL holding
                        // the gap lock — the send gate checks bg_active
                        // under this same lock, so it can never enqueue
                        // into a worker-less buffer (invariant 2). Mark the
                        // guard skip-on-drop in this SAME critical section:
                        // from this point on, bg_active belongs to whatever
                        // the send gate does next (possibly a brand-new
                        // worker), not to this one — see ActiveClearGuard's
                        // doc for why an unconditional re-clear on Drop
                        // would clobber that ownership handoff.
                        ctx.shared.bg_active.store(false, Ordering::Release);
                        active_guard.skip.store(true, Ordering::Release);
                        DrainPlan::Empty
                    } else if transport_guard.is_some() {
                        let (seq, msg) = gap.begin_send().expect("checked non-empty above");
                        DrainPlan::Send { seq, msg }
                    } else {
                        // Inner vanished (only the worker clears it — belt
                        // and braces for future refactors): treat as
                        // broken. Nothing was marked in flight.
                        DrainPlan::NoInner
                    }
                }; // gap lock dropped — never held across the send below
                match plan {
                    DrainPlan::Empty => DrainStep::Empty,
                    DrainPlan::NoInner => DrainStep::Broken,
                    DrainPlan::Send { seq, msg } => {
                        let transport = transport_guard
                            .as_mut()
                            .expect("checked is_some above; only this worker clears it");
                        let outcome = transport.send_bytes(&msg);
                        // Re-take the gap lock ONLY to settle the entry.
                        // The message is still at the front: eviction
                        // skips an in-flight entry.
                        let mut gap = ctx
                            .gap
                            .lock()
                            .expect("BUG: gap lock poisoned — gap buffer is invariant-critical");
                        match outcome {
                            Ok(()) => {
                                gap.finish_send(seq);
                                DrainStep::Sent
                            }
                            Err(TransportError::Backpressure { .. }) => {
                                gap.abort_send(seq);
                                DrainStep::Backpressure
                            }
                            Err(TransportError::TooLarge { len, max }) => {
                                // The rebuilt inner's ceiling shrank below
                                // a queued message. With no caller to
                                // bounce it to, keeping it would wedge the
                                // drain forever — drop it, count it, keep
                                // going.
                                if let Some(dropped) = gap.finish_send(seq) {
                                    gap.bytes_dropped += dropped.len() as u64;
                                    gap.messages_dropped += 1;
                                }
                                drop(gap);
                                warn!(
                                    target: "tst_pipeline::reconnect",
                                    len,
                                    max,
                                    "dropping queued message larger than the reconnected transport's max_payload",
                                );
                                DrainStep::Sent
                            }
                            Err(TransportError::ExplicitClose) => {
                                // The inner was cancelled: terminal, not a
                                // break. The message stays queued (accepted,
                                // undelivered). The latch is set once both
                                // locks are released, below.
                                gap.abort_send(seq);
                                DrainStep::Cancelled
                            }
                            Err(_) => {
                                // Broken / Closed / unknown-future —
                                // rebuild. Front message stays queued for
                                // the retry. Un-publish the dead inner's
                                // wake handle with the inner it belongs
                                // to; the next successful install
                                // republishes.
                                gap.abort_send(seq);
                                drop(gap);
                                *transport_guard = None;
                                ctx.active.clear();
                                DrainStep::Broken
                            }
                        }
                    }
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
                DrainStep::Cancelled => {
                    ctx.latch.inner_cancel(&ctx.shutdown, &ctx.active);
                    return; // the guard clears bg_active on the way out
                }
            }
        }
    }
}
