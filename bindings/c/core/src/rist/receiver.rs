//! `TstRistReceiver` handle type and data-path entry points.
//!
//! Open a RIST-backed raw TS byte receiver with `tst_rist_recv_open`.
//! Pull 188-byte MPEG-TS packets one at a time with
//! `tst_rist_receiver_recv_ts`. Free the handle with
//! `tst_rist_receiver_close`.
//!
//! Data-path bodies (recv_ts, get_stats, get_socket_stats, reset_stats)
//! are thin forwarders to generic impls in `crate::transport_impls`.
//!
//! **Cancel:** the RIST transport exposes a real cancel handle since Arc 2
//! WP-D and this handle's `CHandle` slot holds it, so `_close` cancels
//! first. Note the data path does NOT park: `_recv_ts` is one ~100 ms librist
//! poll that reports `TST_E_BUFFER_FULL` when nothing arrived, so callers
//! poll in a loop. A cancel is observed at the end of the current tick and
//! the loop's next call reports `TST_E_CLOSED`. Do NOT call `_close` from
//! another thread while such a loop runs — `_close` frees the handle and
//! the poller holds no lock between calls. Use `tst_rist_receiver_cancel`
//! (ABI 0.22) instead: it is the NON-FREEING cross-thread cancel, so the
//! poller's pointer stays valid and the owner still calls `_close` once the
//! loop has ended (pinned by
//! `tests/transports/rist_cancel_from_other_thread.rs`).
//!
//! **Construction differs from UDP:** RIST receivers use a bind URL with
//! the ffmpeg `@` prefix (`rist://@host:port`). The builder is
//! `RistRecvTransportBuilder::new(url)?.listen()`.

use std::os::raw::c_char;

use tst_pipeline::{Receiver, ReceiverConfig};
use tst_rist::{RistRecvTransport, RistRecvTransportBuilder};

use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::stats::TstReceiverStats;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a RIST-backed raw TS byte receiver.
///
/// Returned by [`tst_rist_recv_open`]. Freed with
/// [`tst_rist_receiver_close`].
pub struct TstRistReceiver {
    pub(crate) inner: CHandle<Receiver<RistRecvTransport>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open a RIST receiver listening on the bind endpoint described by
/// `url`. Returns `NULL` on error.
///
/// URL grammar (receiver always uses `@` bind prefix):
/// - `rist://@0.0.0.0:port` — bind on all interfaces
/// - `rist://@host:port` — bind on a specific interface address
/// - Query params: `?profile=simple|main`, `?buffer=N` (recovery buffer ms,
///   controls the retransmission window), `?cname=...`
///
/// Encryption (requires mbedtls feature, forces Main Profile):
/// - `?aes-type=128|192|256&secret=<psk>` — AES PSK decryption.
///   Returns `TST_E_RIST_ENCRYPTION_DISABLED (-41)` if the mbedtls
///   feature was disabled at build time.
///
/// Port `0` causes the kernel to assign an ephemeral port.
///
/// WHY the `@` prefix?
///   RIST and MPEG-TS-over-UDP share the ffmpeg convention: `@host:port`
///   means "bind/listen" while `host:port` (no `@`) means "connect/send".
///   A RIST receiver always binds and waits for a sender to connect it.
///
/// # Safety
///
/// `url` must be a NUL-terminated C string. The returned handle must
/// eventually be freed with `tst_rist_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_recv_open(url: *const c_char) -> *mut TstRistReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url_str = match unsafe { crate::c_str::parse_c_str(url, TstError::RistConfig, "url") } {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        // RIST receive builder: new() validates the `@` bind prefix and
        // parses query params; listen() establishes the librist context.
        // URL / config parse failures map to RistConfig (-39) directly.
        // librist runtime failures route through `record_with_context`.
        let builder = match RistRecvTransportBuilder::new(url_str) {
            Ok(b) => b,
            Err(e) => {
                set_last_error(TstError::RistConfig, &format!("rist url parse: {e}"));
                return std::ptr::null_mut();
            }
        };
        let transport = match builder.listen() {
            Ok(t) => t,
            Err(e) => {
                crate::error::record_with_context(e, "rist listen");
                return std::ptr::null_mut();
            }
        };
        let receiver = Receiver::new(transport, ReceiverConfig::default());
        // `cancel_or_latch` resolves the shell's `Option`; since Arc 2 WP-D
        // this transport's `cancel_handle()` is `Some`, so the handle below
        // carries the REAL cancel and `_close` wakes what it can.
        let cancel = cancel_or_latch(receiver.cancel_handle());
        Box::into_raw(Box::new(TstRistReceiver {
            inner: CHandle::new(receiver, cancel, ()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_rist_receiver_t`.
///
/// Safe to call with `NULL` (no-op). See `tst_rist_sender_close` for
/// the ownership semantics.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRistReceiver` returned
/// by `tst_rist_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_close(p: *mut TstRistReceiver) {
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

/// Interrupt a `tst_rist_receiver_recv_ts` on another thread; that call
/// returns `TST_E_CLOSED` at the end of the current ~100 ms librist tick. Callable from any thread,
/// lock-free (never takes the handle's slot), idempotent.
///
/// This is the NON-FREEING cross-thread interrupt: unlike `tst_rist_receiver_close`
/// it leaves the handle valid, so the owner still frees it with
/// `tst_rist_receiver_close` once no other thread is using it.
///
/// The cancel is terminal: the call that observes it and every later
/// `_recv_*` / `_next_event` on this handle return `TST_E_CLOSED`.
///
/// Returns 0, or `TST_E_INVALID_CONFIG` if `p` is null.
///
/// # Safety
///
/// `p` must be NULL or a valid, not-yet-closed `*mut TstRistReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_cancel(p: *mut TstRistReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rist receiver pointer");
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

/// Block until one 188-byte MPEG-TS packet is ready, then copy it
/// into the caller's buffer.
///
/// `buf` MUST point to a buffer of at least `buf_len` bytes (at least
/// 188 bytes). On success, `*out_n` is set to the number of bytes
/// written (always 188). On failure the contents of `buf` are
/// unspecified.
///
/// RIST delivers TS bytes with ARQ-based reliability. If the sender
/// drops a packet and the retransmission window (`?buffer=N` ms) has
/// not expired, RIST will request a retransmit before surfacing the
/// packet here. Once the window expires, a lost packet is skipped and
/// the next available packet is returned. Set `buffer` large enough
/// for your link RTT + jitter.
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
/// `p` must be a valid non-freed `*mut TstRistReceiver`. `buf` must be
/// writable for `buf_len` bytes. `out_n` must be a valid `*mut usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_recv_ts(
    p: *mut TstRistReceiver,
    buf: *mut u8,
    buf_len: usize,
    out_n: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_recv_ts(&handle.inner, buf, buf_len, out_n) }
}

/// Snapshot stats for a `tst_rist_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRistReceiver` opened via `tst_rist_recv_open`.
/// `out` must point to a writable `TstReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_get_stats(
    p: *mut TstRistReceiver,
    out: *mut TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying RIST transport.
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
/// `p` must be a valid `*mut TstRistReceiver` opened via `tst_rist_recv_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_get_socket_stats(
    p: *mut TstRistReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "rist receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_rist_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRistReceiver` opened via `tst_rist_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rist_receiver_reset_stats(p: *mut TstRistReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rist receiver pointer");
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
        unsafe { tst_rist_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_recv_ts_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let mut n = 0usize;
        let rc = unsafe {
            tst_rist_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstReceiverStats::default();
        let rc = unsafe { tst_rist_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_rist_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }
}
