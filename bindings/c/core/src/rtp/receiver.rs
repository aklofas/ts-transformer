//! `TstRtpReceiver` handle type and data-path entry points.
//!
//! Open an RTP-backed raw TS byte receiver with `tst_rtp_recv_open`.
//! Pull 188-byte MPEG-TS packets one at a time with
//! `tst_rtp_receiver_recv_ts`. Cancel a blocked receive with
//! `tst_rtp_receiver_cancel`. Free the handle with
//! `tst_rtp_receiver_close`.
//!
//! Stats bodies (get_stats, get_socket_stats, reset_stats) are thin
//! forwarders to generic impls in `crate::transport_impls`. `recv_ts`
//! stays family-local (its 188-byte copy differs from the generic body);
//! the cancel state and the end-reason cell both live on the handle's
//! `CHandle` — the latch in the cancel slot, the cell in the snapshot.

use std::os::raw::c_char;

use tst_core::RecvTransport;
use tst_core::mpegts::common::TS_PACKET_SIZE;
use tst_pipeline::{Receiver, ReceiverConfig};
use tst_rtp::RtpRecvTransport;

use crate::error::{TstError, record_recv_error, set_last_error};
use crate::handle::{CHandle, cancel_or_latch};
use crate::rtp::RtpRecvSnap;
use crate::rtp::end_reason::convert_end_reason;
use crate::stats::TstReceiverStats;
use crate::stream_end_reason::TstStreamEndReason;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for an RTP-backed raw TS byte receiver.
///
/// Returned by [`tst_rtp_recv_open`]. Freed with
/// [`tst_rtp_receiver_close`].
pub struct TstRtpReceiver {
    /// Slot + cancel handle + cancel latch, plus the construction-constant
    /// end-reason cell in the snapshot — all read without the data-path
    /// lock a parked `_recv_ts` holds.
    pub(crate) inner: CHandle<Receiver<RtpRecvTransport>, RtpRecvSnap>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open an RTP receiver listening on the unicast or multicast endpoint
/// described by `url`. Returns `NULL` on error.
///
/// For unicast, pass `rtp://0.0.0.0:port` or `rtp://127.0.0.1:port`
/// (host is the bind address). For multicast, pass the group address
/// (`rtp://239.0.0.1:port?iface=eth0`); the socket joins the group on
/// `iface` (or the OS-default interface when absent).
///
/// Port `0` causes the kernel to assign an ephemeral port.
///
/// `?pkt_size=` is send-side only and is rejected on receive URLs.
///
/// `?recv_timeout=<ms>` configures a persistent receive deadline (see
/// [`tst_rtp_receiver_recv_ts`]'s doc for how expiry surfaces).
///
/// # Safety
///
/// `url` must be a NUL-terminated C string. The returned handle must
/// eventually be freed with `tst_rtp_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_recv_open(url: *const c_char) -> *mut TstRtpReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let rtp_url = match unsafe { super::url::parse_url(url) } {
            Some(u) => u,
            None => return std::ptr::null_mut(),
        };
        // `listen_with` takes the parsed `RtpUrl` directly (not a
        // field-by-field rebuild), so every present-and-future `RtpUrl`
        // field applies automatically: `recv_timeout` arms the transport,
        // and the `pkt_size`/`pt` receive-URL rejections
        // (`listen_with_rtcp`'s own checks) actually fire — see the
        // regression test below.
        let transport = match RtpRecvTransport::listen_with(&rtp_url) {
            Ok(t) => t,
            Err(e) => {
                set_last_error(TstError::RtpTransport, &format!("rtp listen: {e}"));
                return std::ptr::null_mut();
            }
        };
        // Both observers captured BEFORE the transport moves into the shell.
        let cancel = cancel_or_latch(transport.cancel_handle());
        let end_reason = transport.end_reason_handle();
        let receiver = Receiver::new(transport, ReceiverConfig::default());
        Box::into_raw(Box::new(TstRtpReceiver {
            inner: CHandle::new(receiver, cancel, RtpRecvSnap { end_reason }),
        }))
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_rtp_receiver_t`.
///
/// Safe to call with `NULL` (no-op). See `tst_rtp_sender_close` for
/// the ownership semantics.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpReceiver` returned
/// by `tst_rtp_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_close(p: *mut TstRtpReceiver) {
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
/// - `TST_E_CLOSED` (-7) if the handle was `_cancel`'d or `_close`'d
/// - `TST_E_TRANSPORT` (-8) on transport failure
/// - `TST_E_BUFFER_FULL` (-4) if the receiver was opened with
///   `?recv_timeout=<ms>` and no packet arrived before the configured
///   deadline — retryable, the session stays alive (the same code SRT's
///   recv-deadline expiry already returns to C consumers)
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments or too-small buffer
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRtpReceiver`. `buf` must be
/// writable for `buf_len` bytes. `out_n` must be a valid `*mut usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_recv_ts(
    p: *mut TstRtpReceiver,
    buf: *mut u8,
    buf_len: usize,
    out_n: *mut usize,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    if buf.is_null() {
        set_last_error(TstError::InvalidConfig, "null buf pointer");
        return TstError::InvalidConfig as i32;
    }
    if out_n.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_n pointer");
        return TstError::InvalidConfig as i32;
    }
    if buf_len < TS_PACKET_SIZE {
        set_last_error(
            TstError::InvalidConfig,
            &format!("buf_len {buf_len} too small (need at least {TS_PACKET_SIZE})"),
        );
        return TstError::InvalidConfig as i32;
    }
    // See `tst_receiver_recv_packet`: the latch is read on both sides of the
    // park, and `broken_is_eos` is `true` because a Broken on a plain
    // transport that nobody cancelled means the peer went away.
    let cancelled = handle.inner.is_cancelled();
    handle.inner.with_inner_mut(|rx| match rx.next_packet() {
        Ok(pkt) => {
            // SAFETY: buf non-null + writable for >= TS_PACKET_SIZE bytes per guard.
            unsafe {
                std::ptr::copy_nonoverlapping(pkt.as_ptr(), buf, TS_PACKET_SIZE);
                *out_n = TS_PACKET_SIZE;
            }
            0
        }
        Err(e) => record_recv_error(&e, cancelled || handle.inner.is_cancelled(), true),
    })
}

/// Cancel a `tst_rtp_receiver_t`. Signals the underlying RTP socket to
/// stop, unblocking any thread parked in `_recv_ts`. Safe to call from
/// any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, `_recv_ts` returns `TST_E_CLOSED` (not
/// `TST_E_END_OF_STREAM`). The handle must still be `_close`'d to free.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_cancel(p: *mut TstRtpReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Read the recorded reason this `tst_rtp_receiver_t` receive session
/// ended, if any.
///
/// Writes `TstStreamEndReason::None` (returns `0`) when the session
/// hasn't ended yet, or ended through a path this arc doesn't
/// instrument (e.g. a plain `rtp://` receiver that was never `_cancel`'d
/// or `_close`'d) — and in that case the thread-local last-error channel
/// is left untouched (any pending failure from an earlier call is still
/// readable). A recorded reason is data, not a getter failure — this
/// only returns a nonzero code for a null-pointer argument.
///
/// **Last-error side effect on every ACTUALLY-recorded reason:** unlike
/// the "hasn't ended" case above, once the session has ended this getter
/// unconditionally resets the thread-local last-error channel to
/// `TST_E_SUCCESS` with a detail message — the `KeepaliveFailed` /
/// `TransportFailed` / `ProtocolError` reasons write their underlying
/// detail; `CleanTeardown` / `SessionExpired` / `Cancelled` write an
/// EMPTY message (so `tst_get_last_error_str()` never carries a stale
/// message left over from some earlier, unrelated failure once a reason
/// has been recorded). Read any pending failure from an earlier call
/// BEFORE calling this getter, or it is overwritten — see the exception
/// noted on [`crate::error::tst_get_last_error`].
///
/// Side-channel: reads directly off the end-reason handle captured at
/// `_open` time WITHOUT acquiring this handle's data-path Mutex — same
/// rationale as `tst_rtp_receiver_cancel` (a concurrent `_recv_ts` may
/// be blocked holding it). This is what makes the getter safe to poll
/// from a watchdog thread while another thread drives `_recv_ts`. One
/// consequence: this call never itself returns `TST_E_CLOSED` — after
/// `_close` the whole handle is freed, and calling anything on it,
/// including this getter, is a use-after-free the caller must avoid
/// (not something this function can detect from a dangling pointer).
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRtpReceiver` opened via
/// `tst_rtp_recv_open`. `out` must point to a writable
/// `TstStreamEndReason`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_end_reason(
    p: *mut TstRtpReceiver,
    out: *mut TstStreamEndReason,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        let reason = match handle.inner.snapshot().end_reason.get() {
            Some(r) => convert_end_reason(&r),
            None => TstStreamEndReason::None,
        };
        // SAFETY: out non-null per guard above.
        unsafe { *out = reason };
        0
    })
}

/// Snapshot stats for a `tst_rtp_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpReceiver` opened via `tst_rtp_recv_open`.
/// `out` must point to a writable `TstReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_get_stats(
    p: *mut TstRtpReceiver,
    out: *mut TstReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying RTP socket.
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
/// `p` must be a valid `*mut TstRtpReceiver` opened via `tst_rtp_recv_open`.
/// `out` must point to a writable `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_get_socket_stats(
    p: *mut TstRtpReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::receiver_get_socket_stats(
            &handle.inner,
            out,
            "rtp receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Reset stats counters for a `tst_rtp_receiver_t` to zero.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpReceiver` opened via `tst_rtp_recv_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_receiver_reset_stats(p: *mut TstRtpReceiver) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp receiver pointer");
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
        unsafe { tst_rtp_receiver_close(std::ptr::null_mut()) };
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_rtp_receiver_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_recv_ts_returns_invalid_config() {
        let mut buf = [0u8; 188];
        let mut n: usize = 0;
        let rc = unsafe {
            tst_rtp_receiver_recv_ts(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len(), &mut n)
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_get_stats_returns_invalid_config() {
        let mut stats = TstReceiverStats::default();
        let rc = unsafe { tst_rtp_receiver_get_stats(std::ptr::null_mut(), &mut stats) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_reset_stats_returns_invalid_config() {
        let rc = unsafe { tst_rtp_receiver_reset_stats(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn small_buf_returns_invalid_config() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        let mut buf = [0u8; 100]; // too small for 188-byte packet
        let mut n: usize = 0;
        let rc = unsafe { tst_rtp_receiver_recv_ts(handle, buf.as_mut_ptr(), buf.len(), &mut n) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
        unsafe { tst_rtp_receiver_close(handle) };
    }

    #[test]
    fn null_end_reason_returns_invalid_config() {
        let mut out = TstStreamEndReason::None;
        let rc = unsafe { tst_rtp_receiver_end_reason(std::ptr::null_mut(), &mut out) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_out_end_reason_returns_invalid_config() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        let rc = unsafe { tst_rtp_receiver_end_reason(handle, std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
        unsafe { tst_rtp_receiver_close(handle) };
    }

    #[test]
    fn fresh_receiver_end_reason_is_none() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        // Seed a pending failure that a "hasn't ended" result must NOT
        // clobber (see the getter's doc: only an ACTUALLY-recorded reason
        // touches last-error).
        set_last_error(TstError::Internal, "sentinel-untouched");

        let mut out = TstStreamEndReason::Cancelled; // seed with a non-None value
        let rc = unsafe { tst_rtp_receiver_end_reason(handle, &mut out) };
        assert_eq!(rc, 0);
        assert!(matches!(out, TstStreamEndReason::None));

        assert_eq!(
            unsafe { crate::error::tst_get_last_error() },
            TstError::Internal as i32
        );
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { std::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "sentinel-untouched");

        unsafe { tst_rtp_receiver_close(handle) };
    }

    /// `_cancel` alone only flags the transport; the reason is recorded
    /// by the underlying `RtpRecvTransport` the moment a recv attempt
    /// actually observes the cancel signal (see `recv_raw`'s
    /// cancel-checked-first loop) — so this drives one `_recv_ts` call
    /// after cancelling to make the reason observable, matching what a
    /// real caller's recv loop would do.
    #[test]
    fn cancel_then_recv_records_cancelled_end_reason() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        let cancel_rc = unsafe { tst_rtp_receiver_cancel(handle) };
        assert_eq!(cancel_rc, 0);

        let mut buf = [0u8; 188];
        let mut n: usize = 0;
        let recv_rc =
            unsafe { tst_rtp_receiver_recv_ts(handle, buf.as_mut_ptr(), buf.len(), &mut n) };
        assert_eq!(recv_rc, TstError::Closed as i32);

        // recv_rc above already set last-error to (Closed, "...cancelled or
        // closed by caller..."). Do NOT clear it here — the getter below
        // must overwrite THAT pending state on its own, per the documented
        // last-error contract (see tst_rtp_receiver_end_reason's doc).
        let mut out = TstStreamEndReason::None;
        let rc = unsafe { tst_rtp_receiver_end_reason(handle, &mut out) };
        assert_eq!(rc, 0);
        assert!(matches!(out, TstStreamEndReason::Cancelled));

        // Pin the last-error contract through the real C entry points:
        // Cancelled has no msg, so the getter must reset last-error to
        // (Success, "") — overwriting the recv_ts Closed error above.
        assert_eq!(unsafe { crate::error::tst_get_last_error() }, 0);
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { std::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "");

        unsafe { tst_rtp_receiver_close(handle) };
    }

    /// `?recv_timeout=` must actually arm the transport through
    /// `tst_rtp_recv_open` — regression test for the gap where the open
    /// path reconstructed a fresh `RtpUrl` (host/port/iface only) instead
    /// of retaining the parsed URL's `recv_timeout`, silently dropping the
    /// knob. A quiet socket (no sender) must return `TST_E_BUFFER_FULL`
    /// well inside the 5 s CI-flake margin, not block indefinitely.
    #[test]
    fn url_recv_timeout_arms_transport_via_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?recv_timeout=200").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        assert!(
            !handle.is_null(),
            "tst_rtp_recv_open should succeed for a valid ?recv_timeout= URL"
        );
        let mut buf = [0u8; 188];
        let mut n: usize = 0;
        let start = std::time::Instant::now();
        let rc = unsafe { tst_rtp_receiver_recv_ts(handle, buf.as_mut_ptr(), buf.len(), &mut n) };
        let elapsed = start.elapsed();
        assert_eq!(rc, TstError::BufferFull as i32, "expected deadline expiry");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "recv_ts blocked past the configured 200ms deadline: {elapsed:?}"
        );
        unsafe { tst_rtp_receiver_close(handle) };
    }

    /// `?pkt_size=` is documented as "send-side only and is rejected on
    /// receive URLs" (see `tst_rtp_recv_open`'s doc) — but that rejection
    /// lives inside `RtpRecvTransport::listen_with_rtcp`, which only
    /// fires if the open path hands it the ORIGINAL parsed `RtpUrl`.
    /// Regression test for the same class of bug `recv_timeout` hit: the
    /// prior field-by-field builder rebuild silently dropped `pkt_size`
    /// too, so this URL would have wrongly succeeded before the
    /// `listen_with` fix.
    #[test]
    fn url_pkt_size_rejected_on_recv_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?pkt_size=1316").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        assert!(
            handle.is_null(),
            "tst_rtp_recv_open must reject ?pkt_size= on a receive URL"
        );
    }

    /// `?pt=` is meaningless on a raw-TS receiver — same regression class
    /// as `?pkt_size=` above (the field-by-field builder rebuild would
    /// have silently dropped it too).
    #[test]
    fn url_pt_rejected_on_recv_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?pt=96").unwrap();
        let handle = unsafe { tst_rtp_recv_open(url.as_ptr()) };
        assert!(
            handle.is_null(),
            "tst_rtp_recv_open must reject ?pt= on a receive URL"
        );
    }
}
