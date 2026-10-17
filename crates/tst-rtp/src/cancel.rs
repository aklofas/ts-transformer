//! Cancellation primitive for the `RtpTransport` / `RtpRecvTransport`
//! send/recv UDP socket wrappers (defined in the `transport` module).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The transport's blocking send/recv loops use a 100 ms socket-level
//! timeout (`UdpSocket::set_read_timeout` / `set_write_timeout`); on each
//! timeout they check this `AtomicBool` and either continue or return
//! `TransportError::ExplicitClose`. This mirrors `tst-srt`'s
//! `SRTO_RCVTIMEO`/`SNDTIMEO` pattern — see `crates/tst-srt/src/socket.rs`
//! for the libsrt-side precedent.
//!
//! Cancel is `Send + Sync` so consumers can stash it in an
//! `Arc<dyn TransportCancel>` and share across worker threads.

use std::sync::Arc;
use tst_core::transport::TransportCancel;

/// Shared cancellation flag.
///
/// Construct one with [`Self::new`]; both the transport (poll path) and
/// any caller-facing cancel handle hold an `Arc<Self>`. Setting the flag
/// via [`Self::cancel`] wakes any thread parked in the transport on its
/// next ~100 ms timeout.
#[derive(Debug, Default)]
pub struct RtpCancelHandle(tst_core::cancel::FlagCancel);

impl RtpCancelHandle {
    /// New un-cancelled handle.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark this handle cancelled. Idempotent.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Has cancellation been requested?
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl TransportCancel for RtpCancelHandle {
    fn cancel(&self) {
        RtpCancelHandle::cancel(self);
    }
    fn is_cancelled(&self) -> bool {
        RtpCancelHandle::is_cancelled(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn new_is_not_cancelled() {
        let h = RtpCancelHandle::new();
        assert!(!h.is_cancelled());
    }

    #[test]
    fn cancel_sets_flag() {
        let h = RtpCancelHandle::new();
        h.cancel();
        assert!(h.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let h = RtpCancelHandle::new();
        h.cancel();
        h.cancel();
        h.cancel();
        assert!(h.is_cancelled());
    }

    #[test]
    fn cancel_visible_across_threads() {
        let h = RtpCancelHandle::new();
        let h2 = h.clone();
        // Spin until the cancel set by the main thread becomes visible. The flag
        // is atomic, so the child is guaranteed to observe it eventually — there
        // is no give-up bound (an earlier `0..1000` cap could exhaust before a
        // descheduled main thread ran `cancel()`, flaking on loaded runners). A
        // genuine cross-thread visibility regression would spin forever and be
        // caught by the per-test timeout.
        let t = thread::spawn(move || {
            while !h2.is_cancelled() {
                std::thread::yield_now();
            }
            true
        });
        h.cancel();
        assert!(t.join().unwrap(), "child thread never observed cancel");
    }

    #[test]
    fn rtsp_client_handle_is_a_transport_cancel() {
        fn assert_tc<T: TransportCancel + Clone + Send + Sync + 'static>() {}
        assert_tc::<crate::rtsp::client::RtspCancelHandle>();
    }

    #[test]
    fn satisfies_transport_cancel_trait() {
        fn accept(_: &dyn TransportCancel) {}
        let h = RtpCancelHandle::new();
        accept(&*h);
    }
}
