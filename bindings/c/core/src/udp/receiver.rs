//! `TstUdpReceiver` handle type and data-path entry points.
//!
//! Open a UDP-backed raw TS byte receiver with `tst_udp_recv_open`.
//! Pull 188-byte MPEG-TS packets one at a time with
//! `tst_udp_receiver_recv_ts`. Free the handle with
//! `tst_udp_receiver_close`.
//!
//! Data-path bodies are thin forwarders to generic impls in
//! `crate::transport_impls`.
//!
//! **No `_cancel` entry point yet:** the C ABI exposes no cancel for this
//! family (additive candidate, ABI 0.22 — R4). The handle already carries
//! the binding-shared cancel state (`CHandle`), so `_close` is
//! cancel-first; to unblock a thread parked in a data-path call, close the
//! handle from that thread or use the transport's timeout knobs. The
//! transport itself still has no `cancel_handle()` until WP-D, so the
//! handle's cancel slot holds `binding::FlagCancel` — a latch that records
//! the caller's intent but wakes nothing.

use std::os::raw::c_char;

use tst_pipeline::{Receiver, ReceiverConfig};
use tst_udp::{UdpRecvTransport, UdpRecvTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstReceiverStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a UDP-backed raw TS byte receiver.
///
/// Returned by [`tst_udp_recv_open`]. Freed with
/// [`tst_udp_receiver_close`].
pub struct TstUdpReceiver {
    pub(crate) inner: CHandle<Receiver<UdpRecvTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a UDP receiver listening on the unicast or multicast endpoint
/// described by `url`. Returns `NULL` on error.
///
/// URL grammar:
/// - `udp://host:port` — unicast bind on host:port
/// - `udp://@group:port` (`@` prefix is ffmpeg convention) — multicast recv
/// - Query params: `?iface=eth0`, `?rcvbuf=8M`
///
/// Port `0` causes the kernel to assign an ephemeral port.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string. The returned handle must
/// eventually be freed with `tst_udp_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_recv_open(url: *const c_char) -> *mut TstUdpReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::UdpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let builder = match UdpRecvTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::UdpConfig, &format!("udp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "udp recv build");
                return std::ptr::null_mut();
            }
        };
        let receiver = Receiver::new(transport, ReceiverConfig::default());
        // UDP/RIST expose no cancel handle until WP-D: `cancel_or_latch`
        // supplies the latch stand-in (delete the call in WP-D once
        // `cancel_handle()` is `Some`).
        let cancel = cancel_or_latch(receiver.cancel_handle());
        Box::into_raw(Box::new(TstUdpReceiver {
            inner: CHandle::new(receiver, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_udp_receiver_t`.
///
/// Safe to call with `NULL` (no-op). See `tst_udp_sender_close` for
/// the ownership semantics.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstUdpReceiver` returned
/// by `tst_udp_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_receiver_close(p: *mut TstUdpReceiver) {
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
/// Returns:
/// - `0` on success (188 bytes written to `buf`, `*out_n` = 188)
/// - `TST_E_END_OF_STREAM` (-12) on graceful peer close / EOF
/// - `TST_E_CLOSED` (-7) if the handle was `_close`'d
/// - `TST_E_TRANSPORT` (-8) on transport failure
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments or too-small buffer
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstUdpReceiver`. `buf` must be
/// writable for `buf_len` bytes. `out_n` must be a valid `*mut usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_receiver_recv_ts(
    p: *mut TstUdpReceiver,
    buf: *mut u8,
    buf_len: usize,
    out_n: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_recv_ts(&handle.inner, buf, buf_len, out_n) }
}

/// Snapshot stats for a `tst_udp_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpReceiver` opened via `tst_udp_recv_open`.
/// `out` must point to a writable `TstReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_receiver_get_stats(
    p: *mut TstUdpReceiver,
    out: *mut TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying UDP socket.
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
/// `p` must be a valid `*mut TstUdpReceiver` opened via `tst_udp_recv_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_receiver_get_socket_stats(
    p: *mut TstUdpReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "udp receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_udp_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpReceiver` opened via `tst_udp_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_receiver_reset_stats(p: *mut TstUdpReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp receiver pointer");
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
        unsafe { tst_udp_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_recv_ts_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let rc = unsafe {
            tst_udp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    /// Copilot PR #77: the `buf_len < TS_PACKET_SIZE` guard moved into the
    /// generic `transport_impls::receiver_recv_ts` body during the WP18 dedup,
    /// and the old per-transport small-buffer tests went with the old bodies.
    /// Exercise it through a VALID handle (ephemeral loopback bind, offline)
    /// so the guard — not the null-handle check — is what fires.
    #[test]
    fn small_buffer_recv_ts_returns_invalid_config() {
        let url = std::ffi::CString::new("udp://@127.0.0.1:0").unwrap();
        let rx = unsafe { tst_udp_recv_open(url.as_ptr()) };
        assert!(!rx.is_null(), "ephemeral loopback bind must succeed");
        let mut buf = [0u8; 100]; // < TS_PACKET_SIZE (188)
        let mut n = 0usize;
        let rc = unsafe { tst_udp_receiver_recv_ts(rx, buf.as_mut_ptr(), buf.len(), &mut n) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
        assert!(
            crate::error::test_last_error_msg().contains("too small"),
            "last_error must carry the small-buffer message"
        );
        unsafe { tst_udp_receiver_close(rx) };
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstReceiverStats::default();
        let rc = unsafe { tst_udp_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_udp_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
