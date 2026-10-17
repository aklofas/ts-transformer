//! The closed/cancelled latch pair, written once.

use super::background::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use tst_core::cancel::CancelSlot;
use tst_core::transport::TransportError;

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
