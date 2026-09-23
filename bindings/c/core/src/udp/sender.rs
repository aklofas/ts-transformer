//! `TstUdpSender` handle type and data-path entry points.
//!
//! Open a UDP-backed raw TS byte sender with `tst_udp_sender_open`.
//! Push pre-muxed TS bytes with `tst_udp_sender_send_ts`. Free the
//! handle with `tst_udp_sender_close`.
//!
//! Data-path bodies (send_ts, get_stats, get_socket_stats, reset_stats)
//! are thin forwarders to generic impls in `crate::transport_impls`.
//!
//! **Cancel:** the UDP transport exposes a real cancel handle since Arc 2
//! WP-D and this handle's `CHandle` slot holds it, so `_close` from ANY
//! thread cancels first and a `_send_ts` started after the close returns
//! `TST_E_CLOSED` at its entry check (a UDP send never parks — `send_to`
//! on a datagram socket returns at once). There is no `tst_udp_sender_cancel` entry
//! point yet — it is a new symbol and
//! rides the ABI 0.22 bump (see "C ABI cancel entry points for `tcp://`,
//! `udp://` and `rist://` transports" in
//! `docs/project/deferred-features.md`). Until then `_close` IS the
//! cross-thread cancel.

use std::os::raw::c_char;

use tst_pipeline::{Sender, SenderConfig};
use tst_udp::{UdpTransport, UdpTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstSenderStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a UDP-backed raw TS byte sender.
///
/// Returned by [`tst_udp_sender_open`]. Freed with
/// [`tst_udp_sender_close`].
pub struct TstUdpSender {
    pub(crate) inner: CHandle<Sender<UdpTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a UDP sender to the unicast or multicast endpoint described by
/// `url`. Returns `NULL` on error; check `tst_get_last_error()` for the
/// negative error code and `tst_get_last_error_str()` for a detail message.
///
/// URL grammar:
/// - `udp://host:port` — unicast send
/// - `udp://group:port` (group ∈ 224.0.0.0/4 or ff00::/8) — multicast send
/// - Query params: `?ttl=N`, `?iface=eth0`, `?tos=0xb8`, `?sndbuf=2M`,
///   `?pkt_size=1316`, `?localaddr=...`
///
/// # Safety
///
/// `url` must be a NUL-terminated C string valid for the duration of
/// this call. The returned handle must eventually be freed with
/// `tst_udp_sender_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_open(url: *const c_char) -> *mut TstUdpSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::UdpConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        let builder = match UdpTransportBuilder::from_url(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::UdpConfig, &format!("udp url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "udp build");
                return std::ptr::null_mut();
            }
        };
        let sender = Sender::new(transport, SenderConfig::default());
        // UDP/RIST expose no cancel handle until WP-D: `cancel_or_latch`
        // supplies the latch stand-in (delete the call in WP-D once
        // `cancel_handle()` is `Some`).
        let cancel = cancel_or_latch(sender.cancel_handle());
        Box::into_raw(Box::new(TstUdpSender {
            inner: CHandle::new(sender, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_udp_sender_t`.
///
/// Safe to call with `NULL` (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstUdpSender` returned
/// by `tst_udp_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_close(p: *mut TstUdpSender) {
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

/// Push pre-muxed TS bytes through the UDP sender.
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
/// `p` must be a valid non-freed `*mut TstUdpSender`. `bytes` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_send_ts(
    p: *mut TstUdpSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_send_ts(&handle.inner, bytes, len) }
}

/// Snapshot stats for a `tst_udp_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpSender` opened via `tst_udp_sender_open`.
/// `out` must point to a writable `TstSenderStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_get_stats(
    p: *mut TstUdpSender,
    out: *mut TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying UDP socket.
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
/// `p` must be a valid `*mut TstUdpSender` opened via `tst_udp_sender_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_get_socket_stats(
    p: *mut TstUdpSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "udp sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_udp_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstUdpSender` opened via `tst_udp_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_udp_sender_reset_stats(p: *mut TstUdpSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null udp sender pointer");
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
        unsafe { tst_udp_sender_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_send_ts_returns_invalid_config() {
        let rc = unsafe { tst_udp_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstSenderStats::default();
        let rc = unsafe { tst_udp_sender_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_udp_sender_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
