//! Cancellation primitive for the `RtpTransport` / `RtpRecvTransport`
//! send/recv UDP socket wrappers (defined in the `transport` module,
//! Task 7+).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The transport's blocking send/recv loops use a 100 ms socket-level
//! timeout (`UdpSocket::set_read_timeout` / `set_write_timeout`); on each
//! timeout they check this `AtomicBool` and either continue or return
//! `TransportError::ExplicitClose`. This mirrors `tst-srt`'s
//! `SRTO_RCVTIMEO`/`SNDTIMEO` pattern — see `feedback_repo_standalone_guardrail.md`
//! and `crates/tst-srt/src/socket.rs` for the libsrt-side precedent.
//!
//! Cancel is `Send + Sync` so consumers can stash it in an
//! `Arc<dyn TransportCancel>` and share across worker threads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tst_core::transport::TransportCancel;

/// Shared cancellation flag.
///
/// Construct one with [`Self::new`]; both the transport (poll path) and
/// any caller-facing cancel handle hold an `Arc<Self>`. Setting the flag
/// via [`Self::cancel`] wakes any thread parked in the transport on its
/// next ~100 ms timeout.
#[derive(Debug, Default)]
pub struct RtpCancelHandle {
    flag: AtomicBool,
}

impl RtpCancelHandle {
    /// New un-cancelled handle.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark this handle cancelled. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// Has cancellation been requested?
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
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

/// Cancel handle for an `RtspServer` (introduced in Phase 3 Task 7).
///
/// `cancel()` is the HARD cancel — equivalent to a SIGKILL on the
/// server's tokio Runtime tasks: the listener stops accepting new
/// connections, per-session tasks abort at their next poll, and no
/// RTSP TEARDOWN-style notification is sent to connected clients
/// (clients will see TCP RST or a half-closed connection).
///
/// For graceful shutdown with session-end notification + bounded drain,
/// use `RtspServer::stop` instead.
///
/// The handle is `Clone + Send + Sync`; multiple holders can race the
/// cancel call (idempotent — repeated `cancel()` calls are a no-op).
///
/// Requires the `rtsp-server` feature — this handle only exists paired
/// with a real `RtspServer`.
#[cfg(feature = "rtsp-server")]
#[derive(Clone, Debug)]
pub struct RtspServerCancelHandle {
    pub(crate) cancel: Arc<AtomicBool>,
}

#[cfg(feature = "rtsp-server")]
impl RtspServerCancelHandle {
    /// Construct a fresh handle. Internal — `RtspServer::from_builder`
    /// (Task 7) creates one and exposes it via `cancel_handle()`.
    pub(crate) fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal the server to break out of all blocking I/O at the next
    /// poll. The listener exits its accept loop within ~100 ms; per-session
    /// tasks exit at their next `select!` wake.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Has [`Self::cancel`] been called?
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "rtsp-server")]
impl Default for RtspServerCancelHandle {
    fn default() -> Self {
        Self::new()
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
    fn satisfies_transport_cancel_trait() {
        fn accept(_: &dyn TransportCancel) {}
        let h = RtpCancelHandle::new();
        accept(&*h);
    }
}

#[cfg(all(test, feature = "rtsp-server"))]
mod phase3_server_cancel_tests {
    use super::*;

    #[test]
    fn server_cancel_handle_toggles() {
        let h = RtspServerCancelHandle::new();
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
    }

    #[test]
    fn server_cancel_handle_clone_shares_flag() {
        let h1 = RtspServerCancelHandle::new();
        let h2 = h1.clone();
        h1.cancel();
        assert!(h2.is_cancelled());
    }

    #[test]
    fn server_cancel_handle_idempotent() {
        let h = RtspServerCancelHandle::new();
        h.cancel();
        h.cancel();
        h.cancel();
        assert!(h.is_cancelled());
    }

    #[test]
    fn server_cancel_handle_default_is_not_canceled() {
        let h = RtspServerCancelHandle::default();
        assert!(!h.is_cancelled());
    }
}
