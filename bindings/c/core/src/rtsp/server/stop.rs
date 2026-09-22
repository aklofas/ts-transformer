//! Server-level lifecycle: stats, cancel, stop, free.
//!
//! The entry points covering the back half of the `TstRtspServer` lifecycle
//! (opened + started earlier; these query, cancel, stop, and free it):
//!
//! ```text
//! tst_rtsp_server_get_stats(server, *out)   — snapshot aggregate counters
//! tst_rtsp_server_cancel_handle(server)     — obtain a hard-cancel handle
//! tst_rtsp_cancel_handle_cancel(cancel)     — fire the hard cancel
//! tst_rtsp_cancel_handle_free(cancel)       — drop the cancel handle
//! tst_rtsp_server_stop(server, drain_ms)    — graceful shutdown (two-phase)
//! tst_rtsp_server_free(server)              — drop the Box after stop
//! ```
//!
//! # Two-phase server lifecycle
//!
//! Mirrors the `tst_muxer_close` / `tst_muxer_free` pattern:
//! 1. `tst_rtsp_server_stop` — sends RFC 7826 §13.5.1 Notice 5402
//!    "Server-Initiated TEARDOWN" to each session, cancels all sessions,
//!    fires the global cancel, and sleeps for `graceful_shutdown_drain + 1 s`
//!    to let in-flight RTP drain. Sets the inner `Option` to `None`.
//! 2. `tst_rtsp_server_free` — drops the `Box<TstRtspServer>`. Safe to call
//!    either directly (hard-cancel via Drop) or after `_stop` (inner is already
//!    None; Drop's hard-cancel is idempotent via the AtomicBool).
//!
//! # Hard-cancel handle
//!
//! `tst_rtsp_server_cancel_handle` clones the `RtspServerCancelHandle` stored
//! on `TstRtspServer.cancel` without acquiring the inner Mutex. This guarantees
//! that the cancel can fire even when `_stop` is holding the lock (cross-thread
//! interrupt pattern). The returned `TstRtspCancelHandle` is a heap-allocated
//! opaque handle freed with `tst_rtsp_cancel_handle_free`.

use crate::error::{TstError, set_last_error};
use crate::panic::ffi_catch;
use crate::rtsp::server::types::TstRtspServer;
use crate::stats::{TstServerStats, fill_server_stats};

/// Opaque cancel handle for an RTSP server.
///
/// Obtained from [`tst_rtsp_server_cancel_handle`]. Clone-free — each call to
/// `_cancel_handle` allocates a new heap-boxed copy of the underlying
/// `RtspServerCancelHandle`, which carries a cheap `Arc<AtomicBool>`.
/// Multiple outstanding handles can race `cancel()` calls — all are
/// idempotent.
///
/// Free with [`tst_rtsp_cancel_handle_free`].
pub struct TstRtspCancelHandle {
    pub(crate) inner: tst_rtp::RtspServerCancelHandle,
}

/// Snapshot aggregate server stats into `*out`.
///
/// `out` must be a non-NULL pointer to a caller-allocated
/// `tst_server_stats_t` struct. The struct is filled atomically from the
/// server's internal counters and returned by value (the caller owns the
/// memory). The server must be live (not freed); the call is safe after
/// `tst_rtsp_server_stop` as long as `tst_rtsp_server_free` has not been
/// called yet.
///
/// Returns 0 (`TST_E_SUCCESS`) on success, negative `TST_E_*` on failure.
///
/// # Safety
///
/// - `server` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_builder_start`.
/// - `out` must be a non-NULL, writable pointer to a `tst_server_stats_t`
///   that is valid for this call. The caller retains ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_get_stats(
    server: *mut TstRtspServer,
    out: *mut TstServerStats,
) -> libc::c_int {
    ffi_catch(TstError::Internal as libc::c_int, || {
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "out is null");
            return TstError::InvalidConfig as libc::c_int;
        }
        let Some(handle) = (unsafe { server.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "server is null");
            return TstError::InvalidConfig as libc::c_int;
        };
        let guard = match handle.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "server mutex poisoned");
                return TstError::Internal as libc::c_int;
            }
        };
        let server_ref = match guard.as_ref() {
            Some(s) => s,
            None => {
                set_last_error(TstError::Closed, "server is stopped or freed");
                return TstError::Closed as libc::c_int;
            }
        };
        let snapshot = server_ref.stats();
        // SAFETY: caller guarantees out is a valid, writable pointer.
        let dst = unsafe { &mut *out };
        fill_server_stats(dst, &snapshot);
        TstError::Success as libc::c_int
    })
}

/// Obtain a hard-cancel handle for this server.
///
/// The returned handle is heap-allocated and must be freed with
/// [`tst_rtsp_cancel_handle_free`]. Cloning is cheap — the underlying
/// `Arc<AtomicBool>` is shared with the server's internal cancel flag.
/// Multiple outstanding handles can race `_cancel` calls (all idempotent).
///
/// Unlike `tst_rtsp_server_stop`, cancel does NOT wait for sessions to
/// drain, does NOT send RTSP Notice 5402, and does NOT block. It is the
/// async / signal-handler–safe interrupt path.
///
/// Returns a non-NULL `tst_rtsp_cancel_handle_t*` on success, NULL on
/// failure with last-error set.
///
/// # Safety
///
/// - `server` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_builder_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_cancel_handle(
    server: *mut TstRtspServer,
) -> *mut TstRtspCancelHandle {
    ffi_catch(std::ptr::null_mut(), || {
        let Some(handle) = (unsafe { server.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "server is null");
            return std::ptr::null_mut();
        };
        // Clone the cancel handle WITHOUT acquiring the inner Mutex —
        // the cancel field is separate precisely for this pattern.
        let cancel = handle.cancel.clone();
        Box::into_raw(Box::new(TstRtspCancelHandle { inner: cancel }))
    })
}

/// Fire the hard cancel on a cancel handle.
///
/// Signals the server to break out of all blocking I/O at the next poll.
/// The listener exits its accept loop within ~100 ms; per-session tasks
/// exit at their next `tokio::select!` wake. No TEARDOWN is sent to
/// connected clients (they observe TCP RST or a half-close).
///
/// Idempotent — repeated calls are a no-op. Safe to call on NULL (no-op).
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_cancel_handle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_cancel_handle_cancel(handle: *mut TstRtspCancelHandle) {
    ffi_catch((), || {
        let Some(h) = (unsafe { handle.as_ref() }) else {
            return;
        };
        h.inner.cancel();
    });
}

/// Free a cancel handle obtained from `tst_rtsp_server_cancel_handle`.
///
/// Drops the `TstRtspCancelHandle`. After this call the pointer is invalid.
/// NULL is a no-op. Does NOT cancel the server — fire
/// [`tst_rtsp_cancel_handle_cancel`] before this call if you want to
/// cancel first.
///
/// # Safety
///
/// - `handle` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_cancel_handle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_cancel_handle_free(handle: *mut TstRtspCancelHandle) {
    ffi_catch((), || {
        if handle.is_null() {
            return;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let _ = unsafe { Box::from_raw(handle) };
    });
}

/// Graceful shutdown — two-phase (stop then free).
///
/// 1. For each active session: sends RFC 7826 §13.5.1 Notice 5402
///    "Server-Initiated TEARDOWN" ANNOUNCE over the session's TCP
///    control channel (best-effort, 1 s per-session timeout), then
///    cancels the per-session token.
/// 2. Fires the global cancel token so the listener stops accepting.
/// 3. Sleeps `drain_ms` + 1000 ms to allow in-flight RTP to drain.
/// 4. Sets the inner `Option` to `None` (subsequent calls on this handle
///    return `TST_E_CLOSED`).
///
/// Pass `drain_ms = 0` to use the builder's configured
/// `graceful_shutdown_drain` (the drain the Rust `RtspServer::stop`
/// method already adds 1 s on top of). For fine-grained control, build
/// the server with `tst_rtsp_server_builder_graceful_shutdown_drain_ms`.
///
/// Idempotent — a second call after shutdown completes returns
/// `TST_E_SUCCESS` immediately. Returns `TST_E_INVALID_USAGE` if called
/// before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// - `server` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_server_builder_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_stop(
    server: *mut TstRtspServer,
    _drain_ms: u32,
) -> libc::c_int {
    ffi_catch(TstError::Internal as libc::c_int, || {
        let Some(handle) = (unsafe { server.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "server is null");
            return TstError::InvalidConfig as libc::c_int;
        };
        let mut guard = match handle.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "server mutex poisoned");
                return TstError::Internal as libc::c_int;
            }
        };
        let server_ref = match guard.as_ref() {
            Some(s) => s,
            // Already stopped — idempotent.
            None => return TstError::Success as libc::c_int,
        };
        // Delegate to RtspServer::stop() which performs:
        //   - Notice 5402 ANNOUNCE per active session
        //   - per-session cancel + global cancel
        //   - sleep(graceful_shutdown_drain + 1 s)
        if let Err(e) = server_ref.stop() {
            return crate::error::record_with_context(e, "server stop failed");
        }
        // Mark the handle as stopped so subsequent calls return Closed.
        *guard = None;
        TstError::Success as libc::c_int
    })
}

/// Free the RTSP server handle.
///
/// Drops the `Box<TstRtspServer>`. If `tst_rtsp_server_stop` was called
/// first the inner `Option` is `None` and Drop is a no-op (the runtime
/// was already shut down by `stop`). If `_free` is called WITHOUT a
/// prior `_stop`, Drop fires the hard-cancel path: all per-session tasks
/// abort at their next poll and the tokio Runtime is shut down with a 5 s
/// budget. No RTSP TEARDOWN is sent to connected clients in this case
/// (they observe TCP RST or half-close).
///
/// NULL is a no-op. After this call the pointer is invalid.
///
/// # Safety
///
/// - `server` must be NULL, or a non-freed pointer from
///   `tst_rtsp_server_builder_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_free(server: *mut TstRtspServer) {
    ffi_catch((), || {
        if server.is_null() {
            return;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let _ = unsafe { Box::from_raw(server) };
        // Box drops: if inner is Some, RtspServer::drop fires hard-cancel
        // + runtime shutdown_timeout(5s). If inner is None (already stopped),
        // the drop is a no-op.
    });
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::TstRtspServerBuilder;

    /// Helper: start a server on 127.0.0.1:0 and return the raw handle.
    fn start_test_server() -> *mut TstRtspServer {
        let b = TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0").expect("test url parses");
        let raw_b = TstRtspServerBuilder::into_raw(Box::new(b));
        unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(raw_b) }
    }

    #[test]
    fn null_server_get_stats_returns_error() {
        let mut out = TstServerStats::default();
        let rc = unsafe { tst_rtsp_server_get_stats(std::ptr::null_mut(), &mut out) };
        assert_eq!(rc, TstError::InvalidConfig as libc::c_int);
    }

    #[test]
    fn null_out_get_stats_returns_error() {
        let server = start_test_server();
        assert!(!server.is_null());
        let rc = unsafe { tst_rtsp_server_get_stats(server, std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as libc::c_int);
        unsafe { tst_rtsp_server_free(server) };
    }

    #[test]
    fn get_stats_on_live_server_succeeds() {
        let server = start_test_server();
        assert!(!server.is_null());
        let mut out = TstServerStats::default();
        let rc = unsafe { tst_rtsp_server_get_stats(server, &mut out) };
        assert_eq!(rc, TstError::Success as libc::c_int);
        assert_eq!(out.active_sessions, 0);
        assert_eq!(out.mounts, 0);
        assert_eq!(out.total_rtp_packets_sent, 0);
        assert_eq!(out.total_rtp_bytes_sent, 0);
        unsafe { tst_rtsp_server_free(server) };
    }

    #[test]
    fn cancel_handle_round_trip() {
        let server = start_test_server();
        assert!(!server.is_null());
        let ch = unsafe { tst_rtsp_server_cancel_handle(server) };
        assert!(!ch.is_null());
        // Verify cancel is observable.
        unsafe { tst_rtsp_cancel_handle_cancel(ch) };
        assert!(unsafe { (*ch).inner.is_cancelled() });
        unsafe { tst_rtsp_cancel_handle_free(ch) };
        unsafe { tst_rtsp_server_free(server) };
    }

    #[test]
    fn null_cancel_handle_is_noop() {
        unsafe { tst_rtsp_cancel_handle_cancel(std::ptr::null_mut()) };
        unsafe { tst_rtsp_cancel_handle_free(std::ptr::null_mut()) };
    }

    #[test]
    fn null_server_cancel_handle_returns_null() {
        let ch = unsafe { tst_rtsp_server_cancel_handle(std::ptr::null_mut()) };
        assert!(ch.is_null());
    }

    #[test]
    fn server_free_null_is_noop() {
        unsafe { tst_rtsp_server_free(std::ptr::null_mut()) };
    }

    #[test]
    fn server_free_live_server_succeeds() {
        let server = start_test_server();
        assert!(!server.is_null());
        // Hard-cancel via free (no prior stop).
        unsafe { tst_rtsp_server_free(server) };
        // No panic / hang means the Drop fired cleanly.
    }
}
