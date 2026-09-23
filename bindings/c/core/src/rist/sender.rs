//! `TstRistSender` handle type and data-path entry points.
//!
//! Open a RIST-backed raw TS byte sender with `tst_rist_sender_open`.
//! Push pre-muxed TS bytes with `tst_rist_sender_send_ts`. Free the
//! handle with `tst_rist_sender_close`.
//!
//! Data-path bodies (send_ts, get_stats, get_socket_stats, reset_stats)
//! are thin forwarders to generic impls in `crate::transport_impls`.
//!
//! **Cancel:** the RIST transport exposes a real cancel handle since Arc 2
//! WP-D and this handle's `CHandle` slot holds it, so `_close` fires it
//! before taking the slot. There is little for it to unblock on the send
//! side (`rist_sender_data_write` enqueues and never parks), and `_close`
//! FREES the handle: a `_send_ts` already in flight completes normally (the
//! close waits for the slot in `take()`), and there is no such thing as a
//! `_send_ts` after the close — that would be a use-after-free, not a
//! `TST_E_CLOSED`. `tst_rist_sender_cancel` (ABI 0.22) is the NON-FREEING
//! cross-thread cancel: it fires the same `RistCancelHandle` without taking
//! the slot and leaves the handle valid for its owner to `_close`.
//!
//! **Construction differs from UDP:** RIST uses a move-style builder
//! chain (`RistTransportBuilder::new(url)?.connect()?`) rather than
//! UDP's single `from_url()?.build()?`. URL query params
//! (`?profile=main`, `?buffer=200`, `?bandwidth=10000`,
//! `?aes-type=256&secret=...`, `?cname=...`) are parsed directly by
//! `RistTransportBuilder::new`; no separate C-level config chain is
//! needed for v1.

use std::os::raw::c_char;

use tst_pipeline::{Sender, SenderConfig};
use tst_rist::{RistTransport, RistTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstSenderStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a RIST-backed raw TS byte sender.
///
/// Returned by [`tst_rist_sender_open`]. Freed with
/// [`tst_rist_sender_close`].
pub struct TstRistSender {
    pub(crate) inner: CHandle<Sender<RistTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a RIST sender to the unicast or multicast endpoint described by
/// `url`. Returns `NULL` on error; check `tst_get_last_error()` for the
/// negative error code and `tst_get_last_error_str()` for a detail message.
///
/// URL grammar:
/// - `rist://host:port` — unicast send (Simple Profile by default)
/// - `rist://group:port` (group ∈ 224.0.0.0/4) — multicast send
/// - Query params: `?profile=simple|main`, `?buffer=N` (recovery buffer ms),
///   `?bandwidth=N` (kbps), `?cname=...` (RTCP CNAME)
///
/// Encryption (Main Profile only, requires mbedtls feature):
/// - `?aes-type=128|192|256&secret=<psk>` — AES PSK; forces Main Profile.
///   Returns `TST_E_RIST_ENCRYPTION_DISABLED (-41)` when built without
///   the `mbedtls` feature.
///
/// Example URLs:
/// - `rist://192.168.1.100:8000?buffer=200&profile=main` — Main Profile, 200 ms recovery.
/// - `rist://239.0.0.1:8000?aes-type=256&secret=my-psk&buffer=200` — AES-256, multicast.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string valid for the duration of
/// this call. The returned handle must eventually be freed with
/// `tst_rist_sender_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_open(url: *const c_char) -> *mut TstRistSender {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::RistConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        // RIST uses a move-style builder: new() parses the URL (including
        // query params for profile/buffer/bandwidth/encryption), then
        // connect() establishes the librist context + peer.
        // URL parse failures are definitively config errors (RistConfig -39).
        // Transport-level failures route through `record_with_context`.
        let builder = match RistTransportBuilder::new(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::RistConfig, &format!("rist url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.connect() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "rist connect");
                return std::ptr::null_mut();
            }
        };
        let sender = Sender::new(transport, SenderConfig::default());
        // `cancel_or_latch` resolves the shell's `Option`; since Arc 2 WP-D
        // this transport's `cancel_handle()` is `Some`, so the handle below
        // carries the REAL cancel and `_close` wakes what it can.
        let cancel = cancel_or_latch(sender.cancel_handle());
        Box::into_raw(Box::new(TstRistSender {
            inner: CHandle::new(sender, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_rist_sender_t`.
///
/// Safe to call with `NULL` (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRistSender` returned
/// by `tst_rist_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_close(p: *mut TstRistSender) {
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
// Cancel
// ---------------------------------------------------------------------------

/// Interrupt a `tst_rist_sender_send_ts` on another thread; that call
/// returns `TST_E_CLOSED`. Callable from any thread,
/// lock-free (never takes the handle's slot), idempotent.
///
/// This is the NON-FREEING cross-thread interrupt: unlike `tst_rist_sender_close`
/// it leaves the handle valid, so the owner still frees it with
/// `tst_rist_sender_close` once no other thread is using it.
///
/// Returns 0, or `TST_E_INVALID_CONFIG` if `p` is null.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut TstRistSender`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_cancel(p: *mut TstRistSender) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rist sender pointer");
            return TstError::InvalidConfig as i32;
        };
        // `CHandle::cancel` → `Owned::cancel`: fires the transport's
        // `RistCancelHandle` (Arc 2 WP-D) without taking the slot, so it
        // answers while a data-path call is in flight.
        handle.inner.cancel();
        0
    })
}

// ---------------------------------------------------------------------------
// Data-path entry points
// ---------------------------------------------------------------------------

/// Push pre-muxed TS bytes through the RIST sender.
///
/// `bytes` must point to a buffer of `len` bytes. `len` SHOULD be a
/// multiple of 188 (one or more MPEG-TS packets); the underlying
/// sender will accept any non-zero length but non-aligned buffers
/// may cause sync issues at the receiver.
///
/// RIST adds reliability via its ARQ (Automatic Repeat Request)
/// retransmission layer — the recovery buffer size (set in the URL via
/// `?buffer=N` ms) determines how aggressively the sender caches packets
/// for retransmission on a peer's NACK.
///
/// Returns 0 on success, a negative `TST_E_*` code on failure.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRistSender`. `bytes` must be
/// readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_send_ts(
    p: *mut TstRistSender,
    bytes: *const u8,
    len: usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_send_ts(&handle.inner, bytes, len) }
}

/// Snapshot stats for a `tst_rist_sender_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRistSender` opened via `tst_rist_sender_open`.
/// `out` must point to a writable `TstSenderStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_get_stats(
    p: *mut TstRistSender,
    out: *mut TstSenderStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::sender_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying RIST transport.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if the transport has no live stats
/// (e.g., transport not yet connected or already closed), or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRistSender` opened via `tst_rist_sender_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_get_socket_stats(
    p: *mut TstRistSender,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist sender pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::sender_get_socket_stats(
            &handle.inner,
            out,
            "rist sender socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_rist_sender_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the sender has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRistSender` opened via `tst_rist_sender_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_sender_reset_stats(p: *mut TstRistSender) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist sender pointer");
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
        unsafe { tst_rist_sender_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_send_ts_returns_invalid_config() {
        let rc = unsafe { tst_rist_sender_send_ts(std::ptr::null_mut(), std::ptr::null(), 0) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstSenderStats::default();
        let rc = unsafe { tst_rist_sender_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_rist_sender_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
