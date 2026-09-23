//! Cancellation primitives: [`SrtCancelHandle`] (one fixed handle) and
//! [`CancelSlot`] (whichever handle is currently able to unblock a worker).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//! That tier is the module's; [`CancelSlot`] is the one item that diverges
//! from it and is **Provisional**, as its own docs and the table's `cancel`
//! row both record.
//!
//! [`SrtCancelHandle`] wraps a libsrt `SRTSOCKET` (or any other integer
//! handle) plus a caller-supplied closer closure. Calling `cancel()` from
//! any thread atomically swaps the handle to a sentinel and invokes the
//! closer exactly once. Subsequent `cancel()` calls are no-ops.
//!
//! It is used by `srt::Socket` and `srt::Listener` so a thread parked in
//! `srt_sendmsg` / `srt_recvmsg` / `srt_accept` can be woken from
//! another thread by closing the underlying SRT handle. Per libsrt's
//! semantics, closing a socket that another thread is parked on causes
//! the parked syscall to return with an error (`SRT_ECONNLOST` or
//! similar) — `Socket::send` / `Socket::recv` report `ConnectionBroken`,
//! and `SrtTransport` turns that into `TransportError::ExplicitClose` when
//! its own handle has fired (the normative table in
//! [`crate::transport`]).
//!
//! [`CancelSlot`] (std-only) generalizes that to a *replaceable* target:
//! a worker publishes the handle that can wake its current blocking call
//! and clears it afterwards, while a canceller fires whatever is installed
//! and latches so a late publish cancels itself.

use alloc::boxed::Box;
use alloc::sync::Arc;
use portable_atomic::{AtomicBool, AtomicI64, Ordering};

/// Sentinel stored in the atomic once cancel has run. Picked as `i64::MIN`
/// because libsrt's `SRTSOCKET` (= `c_int`) cannot legally take this value
/// (and even libsrt's own `SRT_INVALID_SOCK = -1` won't collide).
const CANCELLED: i64 = i64::MIN;

/// Type-erased closer the handle invokes on its first `cancel()` call.
type Closer = Box<dyn Fn(i64) + Send + Sync>;

/// Thread-safe one-shot socket-close primitive.
///
/// Construct via `Socket::cancel_handle()` / `Listener::cancel_handle()`
/// (or the test-only `SrtCancelHandle::new`). Clone freely — every clone
/// shares the same atomic state, so calling `cancel()` on any clone
/// fires the closer exactly once.
#[derive(Clone)]
pub struct SrtCancelHandle {
    state: Arc<State>,
}

struct State {
    handle: AtomicI64,
    closer: Closer,
    /// The CALLER-visible cancel latch, deliberately separate from the
    /// `handle` sentinel (WP-C1).
    ///
    /// The sentinel says "the closer has run", which happens on the owner's
    /// internal teardown too — `Socket::drop`, and every transport error
    /// path that retires its socket. `is_cancelled()` must mean "a CALLER
    /// cancelled", never "this socket is dead": the bindings choose between
    /// "caller closed" and "stream ended" from exactly this bit, so reading
    /// the sentinel turned every peer disconnect into a reported caller
    /// close (`TST_E_CLOSED` instead of `TST_E_END_OF_STREAM`).
    cancelled: AtomicBool,
}

impl SrtCancelHandle {
    /// Build a handle. The closer is invoked at most once, with the
    /// handle value passed in here. Public so callers outside the `srt`
    /// module (e.g. test mocks) can construct one; production code uses
    /// `Socket::cancel_handle()` / `Listener::cancel_handle()`.
    pub fn new<F>(handle: i64, closer: F) -> Self
    where
        F: Fn(i64) + Send + Sync + 'static,
    {
        Self {
            state: Arc::new(State {
                handle: AtomicI64::new(handle),
                closer: Box::new(closer),
                cancelled: AtomicBool::new(false),
            }),
        }
    }

    /// Trigger the closer if it hasn't already run, and latch
    /// [`Self::is_cancelled`] — the CALLER-initiated path.
    ///
    /// Idempotent: extra calls (including from other threads) are no-ops.
    /// The closer always runs to completion on the thread that wins the
    /// atomic swap.
    pub fn cancel(&self) {
        // Latch BEFORE waking: firing the closer unblocks a parked call on
        // another thread, and that thread's next act is to ask
        // `is_cancelled()` whether what it saw was caller-initiated.
        self.state.cancelled.store(true, Ordering::Release);
        self.fire();
    }

    /// Trigger the closer WITHOUT latching [`Self::is_cancelled`] — the
    /// owner's internal teardown path.
    ///
    /// For `Socket`/`Listener` `Drop` and the transport error paths that
    /// retire a dead socket: the libsrt handle must still be closed and any
    /// parked call still woken, but no caller asked for this, so the
    /// caller-visible latch must stay clear. Using [`Self::cancel`] here
    /// makes a peer disconnect indistinguishable from a caller cancel and
    /// mislabels the stream's end in every binding.
    pub fn close_without_cancel(&self) {
        self.fire();
    }

    /// Swap in the sentinel and run the closer exactly once.
    fn fire(&self) {
        let prev = self.state.handle.swap(CANCELLED, Ordering::AcqRel);
        if prev != CANCELLED {
            (self.state.closer)(prev);
        }
    }

    /// Returns `true` once [`Self::cancel`] has been called on this handle
    /// (or any clone of it). Advisory — the underlying socket close may not
    /// have completed yet on another thread.
    ///
    /// A teardown through [`Self::close_without_cancel`] does NOT set this,
    /// and neither does a peer disconnect: this is the caller's intent, not
    /// the socket's liveness.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// `true` once the closer has run by any path (caller cancel OR owner
    /// teardown). Crate-internal: the sentinel is an implementation detail,
    /// `is_cancelled()` is the contract.
    #[cfg(test)]
    fn closer_has_run(&self) -> bool {
        self.state.handle.load(Ordering::Acquire) == CANCELLED
    }
}

/// A `SrtCancelHandle` is itself a [`TransportCancel`](crate::transport::TransportCancel):
/// this lets a `Listener`'s handle (the cross-thread wake for a parked
/// `accept()`) be installed wherever a `dyn TransportCancel` is expected —
/// notably a managed receiver's reconnect-factory cancel slot.
impl crate::transport::TransportCancel for SrtCancelHandle {
    fn cancel(&self) {
        SrtCancelHandle::cancel(self);
    }
    fn is_cancelled(&self) -> bool {
        SrtCancelHandle::is_cancelled(self)
    }
}

impl core::fmt::Debug for SrtCancelHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SrtCancelHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// A latched, replaceable cancel target.
///
/// **Stability: Provisional** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
///
/// Where [`SrtCancelHandle`] wraps *one* fixed handle, a `CancelSlot` holds
/// *whichever* [`TransportCancel`](crate::transport::TransportCancel) can
/// currently unblock a worker — a thing that changes as the worker moves
/// from one blocking call to the next. A producer publishes the current one
/// with [`install`](Self::install) before it blocks and drops it with
/// [`clear`](Self::clear) after; a canceller calls [`cancel`](Self::cancel),
/// which latches and fires whatever is installed.
///
/// The latch is what closes the publish/cancel race: an `install` that lands
/// *after* the cancel fires its target immediately rather than parking a
/// worker nobody can reach any more. [`is_cancelled`](Self::is_cancelled)
/// lets a worker that came back from a blocking call tell "cancelled by us"
/// from "the transport faulted".
///
/// Targets are always fired **outside** the internal lock, so a target may
/// re-enter the slot (e.g. read `is_cancelled`) without deadlocking.
///
/// `std`-only: it holds a `std::sync::Mutex`.
#[cfg(feature = "std")]
#[derive(Default)]
pub struct CancelSlot {
    state: std::sync::Mutex<CancelSlotState>,
}

#[cfg(feature = "std")]
#[derive(Default)]
struct CancelSlotState {
    cancelled: bool,
    handle: Option<Arc<dyn crate::transport::TransportCancel + Send + Sync>>,
}

#[cfg(feature = "std")]
impl CancelSlot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the target that can wake the producer's current blocking
    /// call. Fires it at once if [`cancel`](Self::cancel) already ran.
    pub fn install(&self, handle: Arc<dyn crate::transport::TransportCancel + Send + Sync>) {
        let fire_now = {
            let mut s = self.lock();
            if s.cancelled {
                true
            } else {
                s.handle = Some(Arc::clone(&handle));
                false
            }
        };
        // Outside the lock: the handle may close a socket.
        if fire_now {
            handle.cancel();
        }
    }

    /// Forget the installed target (the blocking call returned).
    pub fn clear(&self) {
        self.lock().handle = None;
    }

    /// Latch cancelled and fire the installed target, if any.
    pub fn cancel(&self) {
        let handle = {
            let mut s = self.lock();
            s.cancelled = true;
            s.handle.take()
        };
        if let Some(h) = handle {
            h.cancel();
        }
    }

    /// `true` once [`cancel`](Self::cancel) has run. A producer checks this
    /// before it starts work (skip the whole attempt) and after its blocking
    /// call returns with an error (report a caller-initiated close, not a
    /// transport fault).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    // Recover on poison: the state is two plain fields, never left
    // half-updated by a panic; cancel is best-effort by contract.
    fn lock(&self) -> std::sync::MutexGuard<'_, CancelSlotState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a SrtCancelHandle around an integer "handle" using a stub
    /// closer that records its calls. Verifies idempotence: the closer
    /// runs at most once across any number of cancel() calls (including
    /// concurrent ones).
    #[test]
    fn cancel_runs_closer_once_across_many_calls() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let h = SrtCancelHandle::new(42, move |handle| {
            assert_eq!(handle, 42);
            calls_cl.fetch_add(1, Ordering::SeqCst);
        });

        h.cancel();
        h.cancel();
        h.cancel();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SrtCancelHandle>();
    }

    #[test]
    fn cancel_concurrent_runs_closer_once() {
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_cl = calls.clone();
        let h = std::sync::Arc::new(SrtCancelHandle::new(7, move |handle| {
            // Verify the closer receives the original handle value, not the
            // CANCELLED sentinel. Catches a future bug where someone might
            // "fix" cancel() to pass CANCELLED instead of prev.
            assert_eq!(handle, 7);
            calls_cl.fetch_add(1, Ordering::SeqCst);
        }));
        let barrier = std::sync::Arc::new(Barrier::new(16));

        let mut threads = Vec::new();
        for _ in 0..16 {
            let h2 = h.clone();
            let b2 = barrier.clone();
            threads.push(std::thread::spawn(move || {
                b2.wait();
                h2.cancel();
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn is_cancelled_flips_after_cancel() {
        let h = SrtCancelHandle::new(1, |_| {});
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
    }

    /// WP-C1: the owner's teardown path closes and wakes WITHOUT latching
    /// the caller-visible latch.
    ///
    /// `Socket::drop` fires the handle, and `SrtTransport` drops its socket
    /// on every peer-break path — so if teardown latched `is_cancelled()`, a
    /// peer disconnect would be indistinguishable from a caller cancel and
    /// every clean SRT end-of-stream would be reported as a caller close.
    #[test]
    fn close_without_cancel_runs_the_closer_but_does_not_latch() {
        use core::sync::atomic::AtomicU32;
        let runs = alloc::sync::Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        let h = SrtCancelHandle::new(5, move |handle| {
            assert_eq!(handle, 5);
            r.fetch_add(1, Ordering::SeqCst);
        });

        h.close_without_cancel();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the closer must still run");
        assert!(h.closer_has_run(), "the sentinel is set either way");
        assert!(
            !h.is_cancelled(),
            "owner teardown is not a caller cancel — this bit decides \
             END_OF_STREAM vs CLOSED in every binding"
        );

        // And a later caller cancel still latches, even though the closer
        // has already run (it will not run twice).
        h.cancel();
        assert!(h.is_cancelled());
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the closer runs once");
    }

    /// WP-C1: `is_cancelled` is part of the `TransportCancel` object contract,
    /// reachable through the trait object every binding and managed wrapper
    /// holds — not only through the inherent method. Pinned via the trait
    /// path so a missing trait method is a compile failure here.
    #[test]
    fn is_cancelled_is_reachable_through_the_trait_object() {
        use crate::transport::TransportCancel;
        let h = SrtCancelHandle::new(3, |_| {});
        let dyn_h: &dyn TransportCancel = &h;
        assert!(!dyn_h.is_cancelled());
        dyn_h.cancel();
        assert!(
            dyn_h.is_cancelled(),
            "the trait view must read the same latch as the inherent method"
        );
    }
}

#[cfg(all(test, feature = "std"))]
mod cancel_slot_tests {
    use super::CancelSlot;
    use crate::transport::TransportCancel;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Cancel target that counts its firings, so a test can pin "exactly
    /// once" rather than merely "at least once".
    struct Flag(AtomicU32);

    impl Flag {
        fn new() -> Self {
            Self(AtomicU32::new(0))
        }

        fn fired(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl TransportCancel for Flag {
        fn cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst) > 0
        }
    }

    /// The race the slot exists to close: the cancel lands between the
    /// producer's bind and its install. Installing into an already-cancelled
    /// slot fires at once — and does not retain the target, so a later
    /// `cancel()` cannot fire it a second time.
    #[test]
    fn install_after_cancel_fires_immediately_and_is_not_retained() {
        let slot = CancelSlot::new();
        slot.cancel();
        assert!(slot.is_cancelled());

        let target = Arc::new(Flag::new());
        slot.install(target.clone());
        assert_eq!(
            target.fired(),
            1,
            "install() into a cancelled slot did not fire the target"
        );

        slot.cancel();
        assert_eq!(
            target.fired(),
            1,
            "a target fired by install() must not be retained by the slot"
        );
    }

    #[test]
    fn cancel_fires_installed_target_once_and_latches() {
        let slot = CancelSlot::new();
        let target = Arc::new(Flag::new());
        slot.install(target.clone());
        assert!(!slot.is_cancelled());

        slot.cancel();
        assert_eq!(target.fired(), 1);
        assert!(slot.is_cancelled(), "cancel() must latch");

        slot.cancel();
        assert_eq!(
            target.fired(),
            1,
            "the target is taken on the first cancel, so extra cancels fire nothing"
        );
    }

    #[test]
    fn clear_forgets_target_so_cancel_fires_nothing() {
        let slot = CancelSlot::new();
        let target = Arc::new(Flag::new());
        slot.install(target.clone());
        slot.clear();

        slot.cancel();
        assert_eq!(target.fired(), 0, "a cleared target must not fire");
        assert!(slot.is_cancelled(), "clear() must not un-latch a cancel");
    }

    #[test]
    fn install_replaces_previous_target() {
        let slot = CancelSlot::new();
        let first = Arc::new(Flag::new());
        let second = Arc::new(Flag::new());
        slot.install(first.clone());
        slot.install(second.clone());

        slot.cancel();
        assert_eq!(first.fired(), 0, "the replaced target must not fire");
        assert_eq!(second.fired(), 1);
    }

    /// A target that calls back into the slot must not deadlock: `cancel()`
    /// and `install()` fire targets OUTSIDE the slot mutex. Pinned because
    /// a "simplification" that fired under the guard would hang here rather
    /// than fail visibly.
    #[test]
    fn target_may_reenter_is_cancelled_without_deadlock() {
        struct Reenter(Arc<CancelSlot>, AtomicBool);

        impl TransportCancel for Reenter {
            fn cancel(&self) {
                assert!(
                    self.0.is_cancelled(),
                    "the latch must be visible to a target firing from cancel()"
                );
                self.1.store(true, Ordering::SeqCst);
            }
            fn is_cancelled(&self) -> bool {
                self.1.load(Ordering::SeqCst)
            }
        }

        let slot = Arc::new(CancelSlot::new());
        let target = Arc::new(Reenter(Arc::clone(&slot), AtomicBool::new(false)));
        slot.install(target.clone());

        slot.cancel(); // would deadlock if the target fired under the lock
        assert!(target.1.load(Ordering::SeqCst));

        // `install()` fires targets too — into an already-cancelled slot —
        // and that firing has its own outside-the-lock requirement. Pinned
        // separately: the assertion above passes even if only `cancel()`
        // fires outside the guard.
        let late = Arc::new(Reenter(Arc::clone(&slot), AtomicBool::new(false)));
        slot.install(late.clone()); // would deadlock if install() fired under the lock
        assert!(
            late.1.load(Ordering::SeqCst),
            "install() into a cancelled slot did not fire the target"
        );
    }

    #[test]
    fn cancel_slot_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CancelSlot>();
    }
}
