//! `TstRtpSender` handle type and data-path entry points.
//!
//! Open an RTP-backed raw TS byte sender with `tst_rtp_sender_open`.
//! Push pre-muxed TS bytes with `tst_rtp_sender_send_ts`. Cancel a
//! blocked send (or the caller thread) with `tst_rtp_sender_cancel`.
//! Free the handle with `tst_rtp_sender_close`.
//!
//! `send_ts` and stats bodies are thin forwarders to generic impls in
//! `crate::transport_impls`. Cancel stays family-local only because the
//! literal `extern "C"` signature must sit where cbindgen can see it — the
//! cancel state itself lives on the handle's `CHandle`.

use std::os::raw::c_char;

use tst_core::Transport;
use tst_pipeline::{Sender, SenderConfig};
use tst_rtp::{RtpSocketBuilder, RtpTransport};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstSenderStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for an RTP-backed raw TS byte sender.
///
/// Returned by [`tst_rtp_sender_open`]. Freed with
/// [`tst_rtp_sender_close`].
pub struct TstRtpSender {
    /// Slot + cancel handle + cancel latch in one; no snapshot (this family
    /// has no construction-constant getter).
    pub(crate) inner: CHandle<Sender<RtpTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open an RTP sender to the unicast or multicast endpoint described by
/// `url`. Returns `NULL` on error; check `tst_get_last_error()` for the
/// negative error code and `tst_get_last_error_str()` for a detail message.
///
/// URL form: `rtp://host:port[?ttl=N&iface=eth0&pkt_size=1316&ssrc=N]`.
/// The transport is a UDP socket that sends RTP packets wrapping 7
/// MPEG-TS packets per datagram (RFC 2250 §2). Multicast destinations
/// (`224.0.0.0/4` for IPv4, `ff00::/8` for IPv6) are detected
/// automatically from the destination address.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string valid for the duration of
/// this call. The returned handle must eventually be freed with
/// `tst_rtp_sender_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_open(url: *const c_char) -> *mut TstRtpSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let rtp_url = match unsafe { super::url::parse_url(url) } {
            Some(u) => u,
            None => return std::ptr::null_mut(),
        };
        let mut builder = RtpSocketBuilder::new(rtp_url.host.clone(), rtp_url.port);
        if let Some(ttl) = rtp_url.ttl {
            builder.ttl(ttl);
        }
        if let Some(ref iface) = rtp_url.iface {
            builder.iface(iface.clone());
        }
        if let Some(n) = rtp_url.pkt_size {
            builder.pkt_size(n);
        }
        if let Some(ssrc) = rtp_url.ssrc {
            builder.ssrc(ssrc);
        }
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                set_last_error(TstError::RtpTransport, &format!("rtp connect: {e}"));
                return std::ptr::null_mut();
            }
        };
        // Captured BEFORE the transport moves into the shell.
        let cancel = cancel_or_latch(transport.cancel_handle());
        let sender = Sender::new(transport, SenderConfig::default());
        Box::into_raw(Box::new(TstRtpSender {
            inner: CHandle::new(sender, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_rtp_sender_t`.
///
/// Safe to call with `NULL` (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpSender` returned
/// by `tst_rtp_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_close(p: *mut TstRtpSender) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        // `CHandle::close` is cancel-first (see `tst_receiver_close`).
        boxed.inner.close();
        drop(boxed);
    });
}

// ---------------------------------------------------------------------------
// Data-path entry points
// ---------------------------------------------------------------------------

/// Push pre-muxed TS bytes through the RTP sender.
///
/// `bytes` must point to a buffer of `len` bytes. `len` SHOULD be a
/// multiple of 188 (one or more MPEG-TS packets); the underlying
/// sender will accept any non-zero length but non-aligned buffers
/// may cause sync issues at the receiver.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRtpSender`. `bytes` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_send_ts(
    p: *mut TstRtpSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_send_ts(&handle.inner, bytes, len) }
}

/// Cancel a `tst_rtp_sender_t`. Signals the underlying RTP socket to
/// stop, unblocking any thread parked in `_send_ts`. Safe to call from
/// any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, the handle must still be `_close`'d to free.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpSender`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_cancel(p: *mut TstRtpSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtp sender pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Snapshot stats for a `tst_rtp_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpSender` opened via `tst_rtp_sender_open`.
/// `out` must point to a writable `TstSenderStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_get_stats(
    p: *mut TstRtpSender,
    out: *mut TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying RTP socket.
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
/// `p` must be a valid `*mut TstRtpSender` opened via `tst_rtp_sender_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_get_socket_stats(
    p: *mut TstRtpSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "rtp sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_rtp_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpSender` opened via `tst_rtp_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_sender_reset_stats(p: *mut TstRtpSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp sender pointer");
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
        unsafe { tst_rtp_sender_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_rtp_sender_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_send_ts_returns_invalid_config() {
        let rc = unsafe { tst_rtp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstSenderStats::default();
        let rc = unsafe { tst_rtp_sender_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_rtp_sender_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
