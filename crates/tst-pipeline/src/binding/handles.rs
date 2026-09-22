//! [`ManagedHandles`] — the five lock-free observers a binding keeps next
//! to a managed shell, collected once at open time (Arc 2 spec §3.4).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! No `Option`s: every managed transport's `cancel_handle()` is `Some` by
//! construction, so the `.expect("always Some")` that the bindings wrote
//! at ≥ 16 sites moves into the ONE place that builds this struct — a
//! transport crate's `from_url` family (`tst_srt::shells` today). Every
//! field is obtained from the managed transport BEFORE it moves into its
//! shell (the obtain-before-move rule), so nothing is ever read back out
//! of the shell's slot: reading a handle takes no lock the shell holds
//! while it is parked in a receive.
//!
//! Deliberately NOT here: the send side's
//! [`ManagedStatsHandle`](crate::ManagedStatsHandle), which carries the
//! gap-buffer counters. It has no public constructor, so no receiver-side
//! "never updated" value can be built for it, and an `Option` field would
//! reintroduce exactly the shape this struct exists to remove. The sender
//! `from_url` functions return it alongside these handles instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use tst_core::transport::TransportCancel;

use crate::reconnect::RecvEndReasonHandle;

/// Observers for one managed shell. Cheap to clone; every clone reads the
/// same live counters and fires the same cancel.
#[derive(Clone)]
pub struct ManagedHandles {
    /// The managed transport's cancel: latches the close, wakes a backoff
    /// wait or a factory parked in a re-accept, and closes the live inner.
    /// It never takes the shell's slot (the #189 lease-bug class), so it
    /// is safe to fire from a watchdog thread while another thread is
    /// parked inside the shell.
    pub cancel: Arc<dyn TransportCancel>,
    /// Why the stream ended, first-writer-wins. Recorded only by
    /// [`crate::ManagedDemuxReceiver`]. Every OTHER shell — the plain
    /// `Receiver` and both senders — gets a fresh handle that is never
    /// set: `get()` stays `None` for its whole life. Documented rather
    /// than `Option` so a binding's `end_reason()` getter has one shape.
    pub end_reason: RecvEndReasonHandle,
    /// Successful rebuilds — the factory returned a transport that was
    /// installed (`reconnect_successes` on the send side).
    pub reconnects: Arc<AtomicU64>,
    /// Factory CALLS — every attempt, successful or not (ARCH-08). This is
    /// what a binding's `reconnect_attempts` reports on both sides now; it
    /// used to be computed by a per-binding counting closure.
    pub attempts: Arc<AtomicU64>,
    /// Receive side: `true` while the inner is absent (mid-rebuild, or
    /// permanently once the budget is exhausted — pair with `is_alive` to
    /// tell the two apart). Send side: `true` only while a
    /// `ReconnectMode::Background` worker is active; always `false` in
    /// `Blocking` mode.
    pub reconnecting: Arc<AtomicBool>,
}

impl core::fmt::Debug for ManagedHandles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        use std::sync::atomic::Ordering;
        f.debug_struct("ManagedHandles")
            .field("cancel", &"<dyn TransportCancel>")
            .field("end_reason", &self.end_reason.get())
            .field("reconnects", &self.reconnects.load(Ordering::Acquire))
            .field("attempts", &self.attempts.load(Ordering::Acquire))
            .field("reconnecting", &self.reconnecting.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    struct Noop;

    // WP-C1 adds `fn is_cancelled(&self) -> bool` to the trait; this impl
    // gains `false` there (it is one of the "13 test implementors").
    impl TransportCancel for Noop {
        fn cancel(&self) {}
    }

    fn handles() -> ManagedHandles {
        ManagedHandles {
            cancel: Arc::new(Noop),
            end_reason: RecvEndReasonHandle::default(),
            reconnects: Arc::new(AtomicU64::new(0)),
            attempts: Arc::new(AtomicU64::new(0)),
            reconnecting: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Bindings hand these to watchdog threads.
    #[test]
    fn managed_handles_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ManagedHandles>();
    }

    /// The sender contract: a fresh handle reads `None` forever.
    #[test]
    fn a_fresh_end_reason_handle_reads_none() {
        assert!(handles().end_reason.get().is_none());
    }

    #[test]
    fn clones_share_the_counters() {
        let a = handles();
        let b = a.clone();
        a.attempts.fetch_add(1, Ordering::SeqCst);
        a.reconnecting.store(true, Ordering::SeqCst);
        assert_eq!(b.attempts.load(Ordering::SeqCst), 1);
        assert!(b.reconnecting.load(Ordering::SeqCst));
    }

    #[test]
    fn debug_renders_the_live_values_not_the_arcs() {
        let h = handles();
        h.attempts.fetch_add(2, Ordering::SeqCst);
        let s = format!("{h:?}");
        assert!(s.contains("attempts: 2"), "{s}");
        assert!(s.contains("end_reason: None"), "{s}");
    }
}
