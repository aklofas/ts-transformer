//! `ManagedRecvTransport<R>` — reconnect on receive break.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Sibling to [`ManagedTransport`][crate::reconnect::ManagedTransport]:
//! same factory-closure + [`ReconnectPolicy`] cadence pattern, applied to the
//! receive direction. There is **no gap buffer** — receive-side bytes that
//! never arrived can't be replayed, so reconnect simply restarts the recv
//! loop on a fresh transport and lets the higher-level demux re-align.
//!
//! ## Composition shape
//!
//! `ManagedRecvTransport` implements [`RecvTransport`], so it slots into
//! any of the receive shells (`RawReceiver`, `Receiver`, `DemuxReceiver`)
//! transparently:
//!
//! ```ignore
//! let factory = || SrtTransport::connect(addr, &cfg);
//! let inner = factory()?;
//! let managed = ManagedRecvTransport::new(inner, Box::new(factory), ReconnectPolicy::default());
//! let rx = DemuxReceiver::new(managed);
//! ```
//!
//! ## Asymmetry vs. `ManagedTransport`
//!
//! The send-side decorator uses `Arc<dyn Fn() + Send + Sync>` for the
//! factory and `Arc<Mutex<Option<T>>>` for the inner transport — it must
//! support concurrent close-from-any-thread on top of `&mut self` sends.
//! The receive side has no such requirement: `recv_bytes(&mut self, …)` is
//! exclusive-mutable, so a `Box<dyn FnMut + Send>` factory and a plain
//! `Option<R>` inner suffice. This is intentional, not a copy-paste miss.
//!
//! ## Behavior on reconnect
//!
//! When `recv_bytes` returns `Closed` or `Broken`, the inner transport is
//! dropped, `policy.next_delay(attempt)` decides whether to wait-and-retry
//! or give up. On retry the factory rebuilds a fresh inner; on give-up the
//! decorator latches `closed = true` and all subsequent `recv_bytes` return
//! `Closed`.
//!
//! Backpressure (`TransportError::Backpressure`) is propagated unchanged —
//! it indicates a recv-timeout on a still-alive transport, no reconnect.
//! `TransportError::TooLarge` is not produced by `recv_bytes` (a recv-side
//! cap mismatch surfaces elsewhere) but is propagated unchanged for safety.
//!
//! ## Limitations worth knowing about
//!
//! - **Demuxer / sync state outlives reconnect at THIS layer.** This
//!   decorator only replaces the byte source. If the consumer wraps it
//!   directly in `Receiver` / `DemuxReceiver`, the syncer's internal
//!   buffer and the demuxer's PSI/PES state carry over from the dead
//!   connection — bytes from the dropped connection can splice into
//!   the new connection's framing and produce corrupted samples.
//!   Use [`ManagedDemuxReceiver`][crate::ManagedDemuxReceiver] to
//!   wire reconnect detection into both layers automatically. The
//!   [`ManagedRecvTransport::reconnects_count`] accessor exposes the
//!   rebuild count for callers that want to build their own reset
//!   logic.
//! - **`max_payload` during reconnect.** While the inner is alive the
//!   reported value is the live inner's current value. While mid-reconnect
//!   (`inner` is `None`) the last live inner's value is returned from a
//!   cached field — never a fixed constant that could understate the
//!   deliverable ceiling. Consumers that cache `max_payload` at construction
//!   time (e.g. `Receiver`'s `recv_buf`) still won't re-size on reconnect,
//!   but they won't be told a falsely-small ceiling either.
//! - **Demuxer flush is not invoked.** Terminal `TransportError::Closed`
//!   from this decorator means the reconnect budget is exhausted; the
//!   higher-level shell (`DemuxReceiver`) is responsible for calling
//!   `Demuxer::flush()` to drain any partial PES at end-of-stream.

use crate::reconnect::background::Shutdown;
use crate::reconnect::{ReconnectMode, ReconnectPolicy};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{debug, info, warn};
use tst_core::cancel::CancelSlot;
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;

/// Cross-thread wake for a [`ManagedRecvTransport`] reconnect factory that
/// blocks — the listener-mode re-accept, where the factory sits in
/// `Listener::accept()` until a peer shows up and nothing else can reach
/// that listener.
///
/// The factory calls [`install`](tst_core::cancel::CancelSlot::install) with
/// the handle that can unblock it (a `Listener::cancel_handle()`) right
/// before blocking and [`clear`](tst_core::cancel::CancelSlot::clear) right
/// after. The managed transport's own cancel handle calls
/// [`cancel`](tst_core::cancel::CancelSlot::cancel), which fires whatever is
/// installed and latches, so an `install` that lands after the cancel fires
/// the handle immediately — closing the race where the cancel arrives
/// between the factory's bind and its install. Once cancelled, the factory
/// should return `TransportError::ExplicitClose` and the managed transport
/// reports the caller-initiated close on its next turn.
///
/// Share one `Arc<FactoryCancel>` between the factory closure and
/// [`ManagedRecvTransport::new_with_factory_cancel`].
///
/// This is an alias for the neutral [`tst_core::cancel::CancelSlot`]
/// primitive; the name is kept because the factory protocol above is what
/// this slot means to a managed receiver.
pub type FactoryCancel = tst_core::cancel::CancelSlot;

/// Receive-side reconnect decorator.
///
/// Wraps any [`RecvTransport`] with a factory closure for rebuilding it
/// on `Closed` / `Broken` failure, gated by a [`ReconnectPolicy`]. See
/// the module docs for the full semantics.
///
/// # Lock poisoning policy (post-Wave-4.B)
///
/// - **`active` cancel slot** (publishes the live inner's cancel handle):
///   [`CancelSlot`] recovers its own lock on poison — its state is two plain
///   fields no panic can leave half-written, and cancel is best-effort by
///   contract — so neither `recv_bytes` nor `cancel_handle().cancel()` can
///   fail here.
/// - **No gap lock**: gap-accumulator is `ManagedTransport`-only (send side).
pub struct ManagedRecvTransport<R: RecvTransport> {
    /// Currently-live inner transport. `None` between a tear-down and a
    /// successful factory rebuild.
    inner: Option<R>,
    /// Builds a fresh inner on demand. `FnMut` (rather than `Fn`) lets the
    /// caller carry mutable state — e.g. round-robin a list of fallback
    /// addrs across reconnects.
    factory: Box<dyn FnMut() -> Result<R, TransportError> + Send>,
    /// Backoff cadence + retry budget.
    policy: ReconnectPolicy,
    /// Local latched-close. Set by both `close(&mut self)` and the
    /// reconnect-budget-exhausted path. Checked by `is_alive()`.
    closed: bool,
    /// Set only by caller-initiated paths (`close()` or
    /// `cancel_handle().cancel()`). The entry-gate uses this to decide
    /// whether to return `ExplicitClose` (caller-initiated) or `Closed`
    /// (budget-exhausted, latched from a prior call).
    explicit_close: bool,
    /// Shared latched-close, set by the cancel handle from any thread.
    /// Read at every loop iteration in `recv_bytes`.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// The cancel handle that can currently unblock this receiver: the live
    /// inner's, installed at construction and re-installed on each
    /// successful rebuild, cleared when the inner is torn down. Held in an
    /// `Arc<CancelSlot>` so the cancel handle (a separate object) can fire it
    /// without owning `&mut self`. The slot latches, so a cancel that lands
    /// while the factory is building the next inner still fires that inner's
    /// handle the moment it is published — the caller never ends up reading
    /// from a connection they already asked to abandon.
    active: Arc<CancelSlot>,
    /// Number of times the factory has been successfully invoked to
    /// rebuild a fresh inner transport (does NOT include the
    /// initial `new()` inner). Higher-level shells
    /// (`ManagedDemuxReceiver`) poll this between `recv_bytes` calls
    /// to detect a transport reconnect and reset their parse state.
    /// Atomic so a future cross-thread observer (e.g. a stats thread)
    /// can read it without locking. Stored as `Arc<AtomicU64>` so
    /// observers can hold a handle independent of the decorator's
    /// lifetime.
    reconnects: Arc<AtomicU64>,
    /// True while `inner` is absent — set the moment a broken/closed
    /// inner is torn down (before the factory is consulted), cleared the
    /// moment a fresh inner is successfully installed. Stays `true`
    /// forever once the reconnect budget is exhausted (there is no
    /// further attempt to clear it) — callers that need to distinguish
    /// "still retrying" from "gave up permanently" should pair this with
    /// [`Self::is_alive`]. Exposed via [`Self::reconnecting_handle`] for
    /// the same reason `reconnects` is: higher-level shells poll it after
    /// this decorator has been moved into a `Receiver`.
    reconnecting: Arc<AtomicBool>,
    /// Deliverable ceiling reported while `inner` is `None`
    /// (mid-reconnect): the most recent live inner's `max_payload()`.
    /// Initialized from the construction-time inner and refreshed on
    /// every successful factory rebuild, so `max_payload()` keeps
    /// reporting the last live ceiling through the window (see the
    /// module-doc "`max_payload` during reconnect" bullet) — never a
    /// fixed constant that could understate it. Deliberate asymmetry
    /// with the send-side wrapper (see `reconnect::mod` max_payload):
    /// understating a send budget is safe; understating a recv ceiling
    /// was the PR #97 bug class.
    last_live_max_payload: usize,
    /// Interruptible backoff wait: `cancel_handle().cancel()` and `close()`
    /// signal it so a wait between reconnect attempts ends at once instead
    /// of riding out the full delay (the send side's PR #158 shape).
    shutdown: Arc<Shutdown>,
    /// Slot the reconnect factory installs its wake handle into while it
    /// blocks (listener re-accept); fired by the cancel handle. `None` for
    /// factories that never block on anything cancel can reach.
    factory_cancel: Option<Arc<FactoryCancel>>,
}

impl<R: RecvTransport> ManagedRecvTransport<R> {
    /// Build a new decorator around an already-connected `inner`.
    ///
    /// `factory` is called when `inner` later fails; on the very first
    /// `recv_bytes` it is not consulted because `inner` is provided
    /// up-front. Subsequent reconnects build via `factory` only.
    pub fn new(
        inner: R,
        factory: Box<dyn FnMut() -> Result<R, TransportError> + Send>,
        policy: ReconnectPolicy,
    ) -> Self {
        Self::build(inner, factory, policy, None)
    }

    /// Like [`new`](Self::new), with a [`FactoryCancel`] slot shared with
    /// the factory so a cancel can reach a factory that blocks (a
    /// listener-mode re-accept). The managed transport's cancel handle
    /// fires the slot; the factory installs its wake handle into it around
    /// its blocking call.
    pub fn new_with_factory_cancel(
        inner: R,
        factory: Box<dyn FnMut() -> Result<R, TransportError> + Send>,
        policy: ReconnectPolicy,
        factory_cancel: Arc<FactoryCancel>,
    ) -> Self {
        Self::build(inner, factory, policy, Some(factory_cancel))
    }

    fn build(
        inner: R,
        factory: Box<dyn FnMut() -> Result<R, TransportError> + Send>,
        policy: ReconnectPolicy,
        factory_cancel: Option<Arc<FactoryCancel>>,
    ) -> Self {
        if policy.mode == ReconnectMode::Background {
            // Recv-side has no gap buffer and no background worker — there
            // is nothing for the caller's thread to be freed from. Warn
            // once at construction (rather than silently downgrading) so a
            // caller who copy-pasted a send-side policy notices the no-op
            // instead of assuming their receiver never blocks on reconnect.
            warn!(
                target: "tst_pipeline::reconnect",
                "ReconnectMode::Background is send-side only; this receiver reconnects on the caller's thread",
            );
        }
        let active = Arc::new(CancelSlot::new());
        if let Some(h) = inner.cancel_handle() {
            active.install(h);
        }
        let last_live_max_payload = inner.max_payload();
        Self {
            inner: Some(inner),
            factory,
            policy,
            closed: false,
            explicit_close: false,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active,
            reconnects: Arc::new(AtomicU64::new(0)),
            reconnecting: Arc::new(AtomicBool::new(false)),
            last_live_max_payload,
            shutdown: Arc::new(Shutdown::new()),
            factory_cancel,
        }
    }

    /// Total number of successful factory rebuilds since construction.
    ///
    /// Returns 0 if no reconnect has fired yet (the initial inner
    /// passed to [`Self::new`] does NOT count). Increments by 1 on
    /// each successful `(self.factory)()` call inside `recv_bytes`.
    ///
    /// Higher-level shells like
    /// [`ManagedDemuxReceiver`][crate::ManagedDemuxReceiver] poll this
    /// between `recv_bytes` calls; when the count rises, they reset
    /// their sync + demux state to prevent stale bytes from the dead
    /// connection from spliced into the new connection's parse state.
    #[must_use]
    pub fn reconnects_count(&self) -> u64 {
        self.reconnects.load(Ordering::Acquire)
    }

    /// Shared handle to the reconnect counter. Lets a higher-level
    /// shell that no longer owns `&self` (e.g. after the
    /// `ManagedRecvTransport` has been moved into a `Receiver`) still
    /// poll the count to detect reconnects.
    ///
    /// The returned `Arc<AtomicU64>` is updated by the decorator each
    /// time the factory successfully rebuilds the inner transport.
    /// Read with `.load(Ordering::Acquire)`.
    #[must_use]
    pub fn reconnects_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.reconnects)
    }

    /// Shared handle to whether `inner` is currently absent (mid-reconnect,
    /// or permanently after the reconnect budget is exhausted).
    ///
    /// Same rationale as [`Self::reconnects_handle`]: a higher-level shell
    /// that no longer owns `&self` (e.g. after this decorator has been
    /// moved into a `Receiver`) can still poll the state. Read with
    /// `.load(Ordering::Acquire)`.
    #[must_use]
    pub fn reconnecting_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reconnecting)
    }
}

impl<R: RecvTransport> RecvTransport for ManagedRecvTransport<R> {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // Caller-initiated paths (close() or cancel_handle().cancel()) return
        // ExplicitClose. The receive-side shell's kind_from_transport maps this
        // to ShellErrorKind::Closed (→ TST_E_CLOSED -7), distinguishing from
        // peer-EOS which arrives as TransportError::Closed from the inner
        // transport's recv_bytes and maps to ShellErrorKind::EndOfStream
        // (→ TST_E_END_OF_STREAM -12). See
        // docs/plans/2026-05-20-transport-semantics-and-mutex-policy.md.
        //
        // The entry gate distinguishes two latched-close scenarios:
        // - explicit_close || cancelled → caller-initiated → ExplicitClose.
        // - closed only (set by budget-exhausted path) → Closed (peer-EOS-ish).
        if self.explicit_close || self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            // Latch on the way out: a cancel that landed between calls set
            // only the shared flag, so without this `is_alive()` would keep
            // reporting live for a receiver that can never deliver again.
            // Same pair the mid-loop cancel check below latches.
            self.closed = true;
            self.explicit_close = true;
            return Err(TransportError::ExplicitClose);
        }
        if self.closed {
            return Err(TransportError::Closed);
        }
        let mut attempt: u32 = 0;
        loop {
            if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                // Cross-thread cancel fired mid-loop. Latch both flags:
                // closed for is_alive(); explicit_close so re-entry returns
                // ExplicitClose (caller-initiated, not budget-exhausted).
                self.closed = true;
                self.explicit_close = true;
                return Err(TransportError::ExplicitClose);
            }
            // Get-or-rebuild inner. The factory may itself fail (e.g. DNS
            // didn't resolve, peer is still down) — treat factory failure
            // exactly like a recv break and back off via the policy.
            if self.inner.is_none() {
                attempt = attempt.saturating_add(1);
                let Some(delay) = self.policy.next_delay(attempt) else {
                    let max = self.policy.max_attempts.unwrap_or(0);
                    // attempts_made = attempt - 1 because this iteration
                    // never got past the budget check (no factory call
                    // happened on this turn).
                    warn!(
                        target: "tst_pipeline::managed_receive",
                        attempts_made = attempt - 1,
                        max_attempts = max,
                        "reconnect gave up — propagating final error to caller",
                    );
                    // Peer is unreachable beyond the reconnect budget — semantically
                    // "stream is over from the inner-transport's perspective." Shell's
                    // kind_from_transport maps Closed → EndOfStream (→ TST_E_END_OF_STREAM)
                    // for the receive side, distinguishing from caller-initiated ExplicitClose
                    // above. See docs/plans/2026-05-20-transport-semantics-and-mutex-policy.md
                    // for the disposition rationale.
                    self.closed = true;
                    return Err(TransportError::Closed);
                };
                info!(
                    target: "tst_pipeline::managed_receive",
                    attempt,
                    max_attempts = self.policy.max_attempts.unwrap_or(0),
                    backoff_ms = delay.as_millis() as u64,
                    "reconnect attempt",
                );
                if !delay.is_zero() {
                    debug!(
                        target: "tst_pipeline::managed_receive",
                        backoff_ms = delay.as_millis() as u64,
                        "backoff before next attempt",
                    );
                }
                // Interruptible: a cross-thread cancel (or close) signals
                // `shutdown` and this returns early instead of riding out
                // the delay — with the default exponential policy that
                // could be 10 s of ignoring a Ctrl-C.
                if self.shutdown.wait_timeout(delay) {
                    self.closed = true;
                    self.explicit_close = true;
                    return Err(TransportError::ExplicitClose);
                }
                match (self.factory)() {
                    Ok(t) => {
                        // Publish the fresh transport's wake handle BEFORE
                        // anyone reads from it. The slot latches, so if a
                        // cancel landed while the factory was building `t`
                        // this fires that handle at once instead of
                        // retaining a target nobody will ever consult.
                        if let Some(h) = t.cancel_handle() {
                            self.active.install(h);
                        }
                        // …and then honor the cancel ourselves. Without this
                        // the cancel would be silently lost: it fired against
                        // whatever was installed at the time (nothing —
                        // the previous inner was torn down before the
                        // factory ran) and the caller would go on to receive
                        // from a connection they already asked to abandon.
                        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            // `t` is dropped here, never stored and never
                            // read from, so `inner` stays `None`. Latch both
                            // flags: closed for is_alive(); explicit_close so
                            // re-entry reports the caller-initiated close.
                            self.closed = true;
                            self.explicit_close = true;
                            return Err(TransportError::ExplicitClose);
                        }
                        self.last_live_max_payload = t.max_payload();
                        self.inner = Some(t);
                        self.reconnecting.store(false, Ordering::Release);
                        // Observable post-rebuild — higher-level shells
                        // (`ManagedDemuxReceiver`) read this counter between
                        // `recv_bytes` calls to detect a fresh transport and
                        // reset their sync/demux state. Release ordering so a
                        // reader using Acquire sees the new inner installed
                        // above. Increment AFTER `self.inner = Some(t)` so a
                        // reader that races observes count rise only when the
                        // new inner is in place.
                        self.reconnects.fetch_add(1, Ordering::Release);
                    }
                    Err(_) => continue,
                }
            }

            // Safety: just constructed above if it was None. unwrap is sound.
            let t = self.inner.as_mut().unwrap();
            match t.recv_bytes(buf) {
                Ok(n) => return Ok(n),
                Err(TransportError::Closed) | Err(TransportError::Broken { .. }) => {
                    // Transport is dead. Drop it; next loop iteration
                    // reconnects via the factory under the configured backoff.
                    // Un-publish its cancel handle with the inner it belongs
                    // to — nothing can be woken until the factory installs
                    // the replacement.
                    self.inner = None;
                    self.active.clear();
                    self.reconnecting.store(true, Ordering::Release);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn max_payload(&self) -> usize {
        // Live inner: report its current value. Mid-reconnect (None):
        // report the most recent live inner's value — never a fixed
        // constant that could understate the deliverable ceiling.
        self.inner
            .as_ref()
            .map(|i| i.max_payload())
            .unwrap_or(self.last_live_max_payload)
    }

    fn is_alive(&self) -> bool {
        !self.closed
    }

    fn close(&mut self) {
        // Latch both flags: closed for is_alive(); explicit_close so
        // subsequent recv_bytes calls return ExplicitClose (not Closed).
        self.closed = true;
        self.explicit_close = true;
        self.shutdown.signal();
        // Un-publish the inner's wake handle, same as the tear-down site in
        // `recv_bytes`: this inner is being closed right here, so nothing
        // reachable through the slot can still need waking.
        self.active.clear();
        if let Some(t) = self.inner.as_mut() {
            t.close();
        }
    }

    fn cancel_handle(&self) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        Some(Arc::new(ManagedRecvCancel {
            cancelled: self.cancelled.clone(),
            shutdown: Arc::clone(&self.shutdown),
            factory_cancel: self.factory_cancel.clone(),
            active: Arc::clone(&self.active),
        }))
    }

    fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        // Mirror max_payload() shape: forward to inner when alive; None
        // when mid-reconnect or after close. The C ABI maps None to
        // TST_E_NOT_AVAILABLE.
        self.inner.as_ref().and_then(|r| r.socket_stats())
    }
}

struct ManagedRecvCancel {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<Shutdown>,
    factory_cancel: Option<Arc<FactoryCancel>>,
    active: Arc<CancelSlot>,
}

impl tst_core::transport::TransportCancel for ManagedRecvCancel {
    fn cancel(&self) {
        // Latch FIRST, so the recv loop sees the cancel however far along it
        // is: whichever of the three wakes below lands, the loop rechecks
        // this flag before it does anything else with the transport.
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake a backoff wait and a factory parked in re-accept; both are
        // no-ops when nothing is waiting there.
        self.shutdown.signal();
        if let Some(fc) = &self.factory_cancel {
            fc.cancel();
        }
        // Wake a parked recv, and latch the slot so an inner published
        // after this point (the factory was mid-build) is cancelled on
        // arrival rather than parked on.
        self.active.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconnect::{BackoffStrategy, ReconnectPolicy};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Mock `RecvTransport` that returns `Ok(n)` for the first
    /// `ok_until_calls` `recv_bytes` calls (each writing one byte), then
    /// switches to `Err(Broken)` to drive the reconnect path.
    struct FlakyRecv {
        calls: u32,
        ok_until: u32,
    }

    impl RecvTransport for FlakyRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.calls += 1;
            if self.calls <= self.ok_until {
                buf[0] = self.calls as u8;
                Ok(1)
            } else {
                Err(TransportError::Broken {
                    msg: "flaky test transport".into(),
                    errno_code: None,
                })
            }
        }

        fn max_payload(&self) -> usize {
            1316
        }

        fn is_alive(&self) -> bool {
            self.calls < self.ok_until
        }
    }

    /// Build a policy with no real wait time so tests don't sleep.
    fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts,
            backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
            ..Default::default()
        }
    }

    /// On `Broken` from the inner, the decorator rebuilds via the factory
    /// and the next `recv_bytes` succeeds against the fresh transport.
    #[test]
    fn reconnects_on_broken() {
        let factory_calls = Arc::new(Mutex::new(0u32));
        let factory_calls_cl = factory_calls.clone();
        let factory = Box::new(move || {
            *factory_calls_cl.lock().unwrap() += 1;
            // Each rebuilt inner serves 1 byte then breaks.
            Ok(FlakyRecv {
                calls: 0,
                ok_until: 1,
            })
        });

        let initial = FlakyRecv {
            calls: 0,
            ok_until: 1,
        };
        let mut managed = ManagedRecvTransport::new(initial, factory, fast_policy(Some(5)));

        let mut buf = [0u8; 8];
        // First call: from initial inner, returns Ok(1).
        assert_eq!(managed.recv_bytes(&mut buf).unwrap(), 1);
        // Initial inner is now exhausted; second call triggers reconnect
        // via factory and the new inner returns Ok(1).
        assert_eq!(managed.recv_bytes(&mut buf).unwrap(), 1);
        assert!(*factory_calls.lock().unwrap() >= 1);
    }

    /// When the reconnect budget is exhausted, `recv_bytes` returns
    /// `Closed` and the decorator latches as closed for all future calls.
    #[test]
    fn gives_up_after_max_attempts() {
        // Factory always fails — budget gets exhausted immediately.
        let factory = Box::new(|| -> Result<FlakyRecv, TransportError> {
            Err(TransportError::Broken {
                msg: "factory always fails".into(),
                errno_code: None,
            })
        });

        let initial = FlakyRecv {
            calls: 0,
            ok_until: 0,
        }; // breaks immediately
        let mut managed = ManagedRecvTransport::new(initial, factory, fast_policy(Some(2)));

        let mut buf = [0u8; 8];
        let err = managed.recv_bytes(&mut buf).unwrap_err();
        assert_eq!(err, TransportError::Closed);
        assert!(!managed.is_alive());

        // Subsequent call short-circuits.
        let err2 = managed.recv_bytes(&mut buf).unwrap_err();
        assert_eq!(err2, TransportError::Closed);
    }

    /// `Backpressure` propagates unchanged — it indicates recv timeout on
    /// a still-alive transport, not a reason to reconnect.
    #[test]
    fn backpressure_propagates_without_reconnect() {
        struct BackpressureRecv;
        impl RecvTransport for BackpressureRecv {
            fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
                Err(TransportError::Backpressure {
                    msg: "recv timeout".into(),
                    errno_code: None,
                })
            }
            fn max_payload(&self) -> usize {
                1316
            }
            fn is_alive(&self) -> bool {
                true
            }
        }

        let factory_calls = Arc::new(Mutex::new(0u32));
        let factory_calls_cl = factory_calls.clone();
        let factory = Box::new(move || {
            *factory_calls_cl.lock().unwrap() += 1;
            Ok(BackpressureRecv)
        });

        let mut managed =
            ManagedRecvTransport::new(BackpressureRecv, factory, fast_policy(Some(5)));

        let mut buf = [0u8; 8];
        let err = managed.recv_bytes(&mut buf).unwrap_err();
        assert!(matches!(err, TransportError::Backpressure { .. }));
        // Factory must NOT have been called — backpressure is not a reason
        // to rebuild the transport.
        assert_eq!(*factory_calls.lock().unwrap(), 0);
    }

    /// `close()` latches the decorator closed and short-circuits future
    /// `recv_bytes` without consulting the factory.
    #[test]
    fn close_short_circuits() {
        let factory_calls = Arc::new(Mutex::new(0u32));
        let factory_calls_cl = factory_calls.clone();
        let factory = Box::new(move || {
            *factory_calls_cl.lock().unwrap() += 1;
            Ok(FlakyRecv {
                calls: 0,
                ok_until: 1,
            })
        });

        let initial = FlakyRecv {
            calls: 0,
            ok_until: 1,
        };
        let mut managed = ManagedRecvTransport::new(initial, factory, fast_policy(Some(5)));

        managed.close();
        assert!(!managed.is_alive());

        let mut buf = [0u8; 8];
        // close() is a caller-initiated path → ExplicitClose (not Closed).
        assert_eq!(
            managed.recv_bytes(&mut buf).unwrap_err(),
            TransportError::ExplicitClose
        );
        assert_eq!(*factory_calls.lock().unwrap(), 0);
    }

    /// Stub RecvTransport whose cancel_handle latches a flag.
    struct CancellableRecv {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }
    struct CancellableRecvCancel {
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    }
    impl tst_core::transport::TransportCancel for CancellableRecvCancel {
        fn cancel(&self) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl RecvTransport for CancellableRecv {
        fn recv_bytes(&mut self, _: &mut [u8]) -> Result<usize, TransportError> {
            if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                Err(TransportError::Broken {
                    msg: "cancelled".into(),
                    errno_code: None,
                })
            } else {
                Ok(0)
            }
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            !self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn cancel_handle(
            &self,
        ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
            Some(Arc::new(CancellableRecvCancel {
                cancelled: self.cancelled.clone(),
            }))
        }
    }

    /// Recv mock with an explicit deliverable ceiling; breaks after
    /// `ok_until` reads like FlakyRecv.
    struct CeilingRecv {
        ceiling: usize,
        calls: usize,
        ok_until: usize,
    }

    impl RecvTransport for CeilingRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.calls += 1;
            if self.calls <= self.ok_until {
                buf[0] = self.calls as u8;
                Ok(1)
            } else {
                Err(TransportError::Broken {
                    msg: "ceiling test transport".into(),
                    errno_code: None,
                })
            }
        }
        fn max_payload(&self) -> usize {
            self.ceiling
        }
        fn is_alive(&self) -> bool {
            self.calls < self.ok_until
        }
    }

    /// Mid-reconnect (inner torn down, factory exhausted), max_payload
    /// reports the LAST LIVE inner's ceiling — not a 1316 constant.
    /// 9000 is deliberately != 1316 and != any transport default so a
    /// regression to either is caught (non-vacuous).
    #[test]
    fn max_payload_mid_reconnect_reports_cached_ceiling() {
        let factory = Box::new(|| {
            Err(TransportError::Broken {
                msg: "factory always fails".into(),
                errno_code: None,
            })
        });
        let initial = CeilingRecv {
            ceiling: 9000,
            calls: 0,
            ok_until: 1,
        };
        let mut m = ManagedRecvTransport::new(initial, factory, fast_policy(Some(2)));
        assert_eq!(m.max_payload(), 9000, "live inner's ceiling");

        let mut buf = [0u8; 16];
        let _ = m.recv_bytes(&mut buf); // serves 1 byte
        let result = m.recv_bytes(&mut buf); // breaks; factory fails; budget exhausts
        assert!(result.is_err(), "budget-exhausted recv must error");
        assert_eq!(
            m.max_payload(),
            9000,
            "cached last-live ceiling during/after the None window, not 1316"
        );
    }

    /// A successful rebuild refreshes the cache to the NEW inner's ceiling.
    #[test]
    fn max_payload_refreshes_on_successful_rebuild() {
        let factory = Box::new(|| {
            Ok(CeilingRecv {
                ceiling: 7000,
                calls: 0,
                ok_until: 10,
            })
        });
        let initial = CeilingRecv {
            ceiling: 9000,
            calls: 0,
            ok_until: 1,
        };
        let mut m = ManagedRecvTransport::new(initial, factory, fast_policy(Some(5)));

        let mut buf = [0u8; 16];
        let _ = m.recv_bytes(&mut buf); // initial serves 1
        let n = m.recv_bytes(&mut buf).expect("reconnects to fresh inner");
        assert_eq!(n, 1);
        assert_eq!(m.max_payload(), 7000, "ceiling follows the rebuilt inner");
    }

    #[test]
    fn managed_recv_cancel_handle_latches_and_cancels_inner() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = CancellableRecv {
            cancelled: cancelled.clone(),
        };
        let cancelled_cl = cancelled.clone();
        let factory = Box::new(move || -> Result<CancellableRecv, TransportError> {
            Ok(CancellableRecv {
                cancelled: cancelled_cl.clone(),
            })
        });
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)));

        let h = managed.cancel_handle().expect("cancellable inner -> Some");
        h.cancel();
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A cancel that lands while nobody is inside `recv_bytes` is only seen
    /// by the entry gate on the next call. That gate must latch the local
    /// flags as well as report — otherwise every later call keeps returning
    /// `ExplicitClose` while `is_alive()` goes on claiming the stream is
    /// live, which is exactly backwards for a caller polling liveness.
    #[test]
    fn managed_recv_entry_gate_latches_closed_on_prior_cancel() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = CancellableRecv {
            cancelled: cancelled.clone(),
        };
        let cancelled_cl = cancelled.clone();
        let factory = Box::new(move || -> Result<CancellableRecv, TransportError> {
            Ok(CancellableRecv {
                cancelled: cancelled_cl.clone(),
            })
        });
        let mut managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)));

        managed
            .cancel_handle()
            .expect("cancellable inner -> Some")
            .cancel();

        let mut buf = [0u8; 8];
        assert_eq!(
            managed.recv_bytes(&mut buf).unwrap_err(),
            TransportError::ExplicitClose,
            "a prior cancel is caller-initiated, not peer-EOS"
        );
        assert!(
            !managed.is_alive(),
            "the entry gate must latch closed, not merely report ExplicitClose"
        );
    }
}
