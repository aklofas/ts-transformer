//! `TstTcpSender` handle type and data-path entry points.
//!
//! Open a TCP-backed raw TS byte sender with `tst_tcp_sender_open`.
//! Push pre-muxed TS bytes with `tst_tcp_sender_send_ts`. Free the
//! handle with `tst_tcp_sender_close`.
//!
//! Data-path bodies (send_ts, get_stats, get_socket_stats, reset_stats)
//! are thin forwarders to generic impls in `crate::transport_impls`.
//!
//! **Single transport type:** unlike UDP which has separate `UdpTransport`
//! and `UdpRecvTransport`, TCP has one `TcpTransport` that implements both
//! `Transport` and `RecvTransport`. The role is determined by which pipeline
//! shell consumes it. Here `Sender<TcpTransport>` uses it as a sender.
//!
//! **Cancel:** the Rust `TcpTransport` exposes `cancel_handle()` (since
//! PR #198) but the C ABI has no `tst_tcp_sender_cancel` entry point yet
//! (additive candidate, ABI 0.22) and no `was_cancelled` side-channel —
//! `_close` simply drops the handle. Consequence since deep review #4
//! WP-4b: a send against a peer that has stopped reading blocks until the
//! peer resumes, the peer resets the connection, or this handle is closed
//! from the same thread — it no longer returns `TST_E_TRANSPORT` after
//! ~100 ms.

use std::os::raw::c_char;

use tst_pipeline::{Sender, SenderConfig};
use tst_tcp::{TcpTransport, TcpTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::Handle;
use crate::stats::TstSenderStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a TCP-backed raw TS byte sender.
///
/// Returned by [`tst_tcp_sender_open`]. Freed with
/// [`tst_tcp_sender_close`].
pub struct TstTcpSender {
    pub(crate) inner: Handle<Sender<TcpTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a TCP sender to the endpoint described by `url`. Returns `NULL`
/// on error; check `tst_get_last_error()` for the negative error code and
/// `tst_get_last_error_str()` for a detail message.
///
/// URL grammar:
/// - `tcp://host:port` — plain TCP caller
/// - `tcps://host:port` — TLS caller (disabled if built without `tls` feature)
/// - Query params: `?nodelay=1`, `?rcvbuf=N`, `?sndbuf=N`, `?pkt_size=N`,
///   `?connect_timeout=Ns`
///
/// The connection is established synchronously. Default connect timeout is
/// 10 seconds; override via `?connect_timeout=`.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string valid for the duration of
/// this call. The returned handle must eventually be freed with
/// `tst_tcp_sender_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_open(url: *const c_char) -> *mut TstTcpSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::TcpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let builder = match TcpTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::TcpConfig, &format!("tcp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "tcp connect");
                return std::ptr::null_mut();
            }
        };
        let sender = Sender::new(transport, SenderConfig::default());
        Box::into_raw(Box::new(TstTcpSender {
            inner: Handle::new(sender),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_tcp_sender_t`.
///
/// Safe to call with `NULL` (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstTcpSender` returned
/// by `tst_tcp_sender_open` or `tst_tcp_listener_accept_sender`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_close(p: *mut TstTcpSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

// ---------------------------------------------------------------------------
// Data-path entry points
// ---------------------------------------------------------------------------

/// Push pre-muxed TS bytes through the TCP sender.
///
/// `bytes` must point to a buffer of `len` bytes. `len` SHOULD be a
/// multiple of 188 (one or more MPEG-TS packets); the underlying
/// sender will accept any non-zero length but non-aligned buffers
/// may cause sync issues at the receiver.
///
/// TCP is a reliable bytestream: the library writes all `len` bytes before
/// returning (or returns an error). Unlike UDP there is no datagram
/// boundary, so the receiver MUST have its own framing — this library's
/// TCP receiver re-synchronises on the 0x47 sync byte.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpSender`. `bytes` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_send_ts(
    p: *mut TstTcpSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_send_ts(&handle.inner, bytes, len) }
}

/// Snapshot stats for a `tst_tcp_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpSender` opened via `tst_tcp_sender_open`
/// or `tst_tcp_listener_accept_sender`.
/// `out` must point to a writable `TstSenderStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_get_stats(
    p: *mut TstTcpSender,
    out: *mut TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying TCP socket.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if the transport has no live stats
/// (e.g., socket not yet connected or already closed), or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpSender` opened via `tst_tcp_sender_open`
/// or `tst_tcp_listener_accept_sender`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_get_socket_stats(
    p: *mut TstTcpSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "tcp sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_tcp_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpSender` opened via `tst_tcp_sender_open`
/// or `tst_tcp_listener_accept_sender`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_sender_reset_stats(p: *mut TstTcpSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::sender_reset_stats(&handle.inner)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_close_is_safe() {
        unsafe { tst_tcp_sender_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_send_ts_returns_invalid_config() {
        let rc = unsafe { tst_tcp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstSenderStats::default();
        let rc = unsafe { tst_tcp_sender_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_tcp_sender_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
