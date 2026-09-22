//! `TstTcpReceiver` handle type and data-path entry points.
//!
//! Open a TCP-backed raw TS byte receiver with `tst_tcp_recv_open`.
//! Pull 188-byte MPEG-TS packets one at a time with
//! `tst_tcp_receiver_recv_ts`. Free the handle with
//! `tst_tcp_receiver_close`.
//!
//! Data-path bodies (recv_ts, get_stats, get_socket_stats, reset_stats)
//! are thin forwarders to generic impls in `crate::transport_impls`.
//!
//! **Single transport type:** TCP uses one `TcpTransport` that implements
//! both `Transport` and `RecvTransport`. `Receiver<TcpTransport>` uses the
//! `RecvTransport` side. Construction uses `TcpTransportBuilder::from_url`
//! (same as the sender path — role is determined by the pipeline shell).
//!
//! **No `_cancel` entry point yet:** the Rust `TcpTransport` exposes
//! `cancel_handle()` (since PR #198) and the handle's `CHandle` carries
//! it, so `_close` IS cancel-first — it fires that cancel before taking
//! the slot, which unblocks a thread parked in a data-path call. What the
//! C ABI still lacks is a standalone `tst_tcp_receiver_cancel` entry point
//! (additive candidate, ABI 0.22 — R4 builds it on
//! `handle.inner.cancel()`). The handle therefore DOES carry the
//! binding-shared cancel state, and a call ended by that close reports
//! `TST_E_CLOSED`, not `TST_E_END_OF_STREAM`.

use std::os::raw::c_char;

use tst_pipeline::{Receiver, ReceiverConfig};
use tst_tcp::{TcpTransport, TcpTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstReceiverStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a TCP-backed raw TS byte receiver.
///
/// Returned by [`tst_tcp_recv_open`]. Freed with
/// [`tst_tcp_receiver_close`].
pub struct TstTcpReceiver {
    pub(crate) inner: CHandle<Receiver<TcpTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a TCP receiver connecting to the endpoint described by `url`.
/// Returns `NULL` on error.
///
/// URL grammar:
/// - `tcp://host:port` — connect to a plain TCP listener
/// - `tcps://host:port` — connect with TLS (disabled if built without `tls` feature)
/// - Query params: `?nodelay=1`, `?rcvbuf=N`, `?sndbuf=N`, `?pkt_size=N`,
///   `?connect_timeout=Ns`
///
/// For a listener-accepted connection, use `tst_tcp_listener_accept_receiver`
/// instead.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string. The returned handle must
/// eventually be freed with `tst_tcp_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_recv_open(url: *const c_char) -> *mut TstTcpReceiver {
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
        let receiver = Receiver::new(transport, ReceiverConfig::default());
        // TCP has a real `TcpCancelHandle`, so `cancel_or_latch` passes it
        // straight through.
        let cancel = cancel_or_latch(receiver.cancel_handle());
        Box::into_raw(Box::new(TstTcpReceiver {
            inner: CHandle::new(receiver, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_tcp_receiver_t`.
///
/// Safe to call with `NULL` (no-op). See `tst_tcp_sender_close` for
/// the ownership semantics.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstTcpReceiver` returned
/// by `tst_tcp_recv_open` or `tst_tcp_listener_accept_receiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_receiver_close(p: *mut TstTcpReceiver) {
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

/// Block until one 188-byte MPEG-TS packet is ready, then copy it
/// into the caller's buffer.
///
/// `buf` MUST point to a buffer of at least `buf_len` bytes (at least
/// 188 bytes). On success, `*out_n` is set to the number of bytes
/// written (always 188). On failure the contents of `buf` are
/// unspecified.
///
/// TCP is a reliable bytestream: the library accumulates incoming bytes
/// until it has a complete 188-byte TS packet (synchronised on the 0x47
/// sync byte). Backpressure is handled by the OS TCP flow-control window —
/// this call blocks until the receiver buffer fills or the peer closes.
///
/// Returns:
/// - `0` on success (188 bytes written to `buf`, `*out_n` = 188)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close / EOF
/// - `TST_E_CLOSED` (-7) if the handle was `_close`'d
/// - `TST_E_TRANSPORT` (-8) on transport failure
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments or too-small buffer
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstTcpReceiver`. `buf` must be
/// writable for `buf_len` bytes. `out_n` must be a valid `*mut usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_receiver_recv_ts(
    p: *mut TstTcpReceiver,
    buf: *mut u8,
    buf_len: usize,
    out_n: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_recv_ts(&handle.inner, buf, buf_len, out_n) }
}

/// Snapshot stats for a `tst_tcp_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpReceiver` opened via `tst_tcp_recv_open`
/// or `tst_tcp_listener_accept_receiver`.
/// `out` must point to a writable `TstReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_receiver_get_stats(
    p: *mut TstTcpReceiver,
    out: *mut TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying TCP socket.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if no live socket stats are available, or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpReceiver` opened via `tst_tcp_recv_open`
/// or `tst_tcp_listener_accept_receiver`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_receiver_get_socket_stats(
    p: *mut TstTcpReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "tcp receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_tcp_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstTcpReceiver` opened via `tst_tcp_recv_open`
/// or `tst_tcp_listener_accept_receiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_tcp_receiver_reset_stats(p: *mut TstTcpReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null tcp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::receiver_reset_stats(&handle.inner)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_close_is_safe() {
        unsafe { tst_tcp_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_recv_ts_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let rc = unsafe {
            tst_tcp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstReceiverStats::default();
        let rc = unsafe { tst_tcp_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_tcp_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
