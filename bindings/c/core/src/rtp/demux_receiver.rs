//! `TstRtpDemuxReceiver` handle type and data-path entry points.
//!
//! Open an RTP-backed `DemuxReceiver` with `tst_rtp_demux_receiver_open`.
//! Pull typed `TstEvent` items with `tst_rtp_demux_receiver_next_event`.
//! Cancel with `tst_rtp_demux_receiver_cancel`. Free with
//! `tst_rtp_demux_receiver_close`.
//!
//! Every body here is a thin forwarder to a generic impl in
//! `crate::transport_impls` — `next_event` included: the
//! cancel state it needs to tell peer-EOF from caller-cancel lives on
//! the handle's `CHandle`, which the generic body reads itself. The
//! end-reason cell lives in that handle's snapshot.

use std::os::raw::c_char;
use std::sync::Mutex;

use tst_pipeline::DemuxReceiver;
use tst_rtp::RtpRecvTransport;

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, set_last_error};
use crate::event::{EventArena, TstEvent};
use crate::handle::CHandle;
use crate::rtp::RtpRecvSnap;
use crate::stream_end_reason::{TstStreamEndReason, convert_end_reason};

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for an RTP-backed demux receiver.
///
/// Returned by [`tst_rtp_demux_receiver_open`]. Freed with
/// [`tst_rtp_demux_receiver_close`].
pub struct TstRtpDemuxReceiver {
    /// Slot + cancel handle + cancel latch, plus the construction-constant
    /// end-reason cell in the snapshot. Captured from the underlying
    /// `RtpRecvTransport` both by `tst_rtp_demux_receiver_open` and by
    /// `tst_rtsp_session_into_demux_receiver` (the latter captures it AFTER
    /// `RtspSession::into_recv_transport()` has already swapped in the owning
    /// `RtspClient`'s shared slot, so it reflects reasons recorded by the
    /// RTSP keepalive/pump threads too).
    pub(crate) inner: CHandle<DemuxReceiver<RtpRecvTransport>, RtpRecvSnap>,
    /// Reusable backing storage for `tst_rtp_demux_receiver_next_event` output.
    /// Allocated at open time so the data-path call never allocates on the hot path.
    /// Wrapped in Mutex for re-entrant safety within the Handle's closure.
    pub(crate) arena: Mutex<EventArena>,
    /// Per-stream stats snapshot buffer (borrowed-buffer design §4.5).
    pub(crate) stream_stats_buf: Mutex<Vec<crate::stats::TstStreamStats>>,
    /// The RTSP client of a receiver bridged from an RTSP session; `None`
    /// on every other receiver. Holding it keeps the control connection,
    /// its keepalive and (for TCP-interleaved) the pump that feeds this
    /// receiver alive; dropping it on close sends TEARDOWN.
    pub(crate) rtsp_client: Mutex<Option<tst_rtp::RtspClient>>,
}

// ---------------------------------------------------------------------------
// Open
// ---------------------------------------------------------------------------

/// Open an RTP-backed `DemuxReceiver`. `demux_cfg` may be `NULL`, in
/// which case default demux options apply (lenient / CFI-tolerant mode).
/// Returns `NULL` on error.
///
/// For unicast, pass `rtp://0.0.0.0:port`. For multicast, pass the group
/// address (`rtp://239.0.0.1:port?iface=eth0`).
///
/// `?pkt_size=` is send-side only and is rejected on receive URLs.
///
/// `?recv_timeout=<ms>` configures a persistent receive deadline (see
/// [`tst_rtp_demux_receiver_next_event`]'s doc for how expiry surfaces).
///
/// # Safety
///
/// `url` is a NUL-terminated C string. `demux_cfg` may be NULL or a
/// valid `tst_demux_config_t*`. The returned handle must eventually be
/// freed with `tst_rtp_demux_receiver_close`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_open(
    url: *const c_char,
    demux_cfg: *const TstDemuxConfig,
) -> *mut TstRtpDemuxReceiver {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        let url = unsafe { super::url::parse_url(url) };
        match unsafe {
            crate::transport_impls::open_demux_receiver(
                url,
                demux_cfg,
                super::listen,
                |t: &tst_rtp::RtpRecvTransport| RtpRecvSnap {
                    end_reason: t.end_reason_handle(),
                },
            )
        } {
            Some(inner) => Box::into_raw(Box::new(TstRtpDemuxReceiver {
                inner,
                arena: Mutex::new(EventArena::new()),
                stream_stats_buf: Mutex::new(Vec::new()),
                rtsp_client: Mutex::new(None),
            })),
            None => std::ptr::null_mut(),
        }
    })
}

// ---------------------------------------------------------------------------
// Close
// ---------------------------------------------------------------------------

/// Close and free a `tst_rtp_demux_receiver_t`.
///
/// For a receiver from `tst_rtsp_session_into_demux_receiver` this also
/// closes the RTSP control connection, sending TEARDOWN (bounded to
/// 500 ms).
///
/// Safe to call with `NULL` (no-op).
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpDemuxReceiver`
/// returned by `tst_rtp_demux_receiver_open`,
/// `tst_rtsp_session_into_demux_receiver` or
/// `tst_rtsp_publish_mount_into_demux_receiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_close(p: *mut TstRtpDemuxReceiver) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        // `CHandle::close` is cancel-first (see `tst_receiver_close`).
        boxed.inner.close();
        // The RTSP client goes only after the data plane is closed, so its
        // TEARDOWN never waits behind a parked recv.
        drop(boxed.rtsp_client.lock().ok().and_then(|mut g| g.take()));
        drop(boxed);
    });
}

// ---------------------------------------------------------------------------
// Data-path entry points
// ---------------------------------------------------------------------------

/// Block until one typed `TstEvent` is ready, then populate
/// `*out_event` with the converted event.
///
/// **Borrowed buffer lifetime (design §4.5):** pointer fields on
/// `*out_event` borrow from this handle's `EventArena`. They are
/// valid until the next `_next_event` / `_close` call on the same
/// handle. Callers wanting longer lifetime memcpy out before the
/// next call.
///
/// Returns:
/// - `0` on success (`*out_event` populated)
/// - `TST_E_END_OF_STREAM` (-12) once the receiver is closed locally, or when an
///   RTSP-over-TCP session ends with a clean server teardown (plain `rtp://`
///   has no peer close to report)
/// - `TST_E_CLOSED` (-7) if the handle was `_cancel`'d or `_close`'d
/// - `TST_E_TRANSPORT` (-8) on a socket error, with the transport's own message
/// - `TST_E_BUFFER_FULL` (-4) if the receiver was opened with
///   `?recv_timeout=<ms>` and no event arrived before the configured
///   deadline — retryable, the session stays alive (the same code SRT's
///   recv-deadline expiry already returns to C consumers)
/// - `TST_E_INVALID_TS` (-3) on a demuxer error (strict-mode rejection,
///   unrecoverable packet malformation, or a malformed PES)
/// - `TST_E_TOO_LARGE` (-6) when the pre-sync buffer outgrew `sync_buf_cap`
/// - `TST_E_INVALID_CONFIG` (-1) on null pointer arguments
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRtpDemuxReceiver`. `out_event`
/// must be a valid writable `*mut TstEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_next_event(
    p: *mut TstRtpDemuxReceiver,
    out_event: *mut TstEvent,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    // The generic body owns the null-check on `out_event`, the cancel-aware
    // relabel (it reads the latch off the `CHandle` itself) and the arena
    // borrow. `broken_is_eos = true`: this is a plain transport.
    unsafe {
        crate::transport_impls::demux_receiver_next_event(&handle.inner, &handle.arena, out_event)
    }
}

/// Cancel a `tst_rtp_demux_receiver_t`. Signals the underlying RTP socket
/// to stop, unblocking any thread parked in `_next_event`. Safe to call
/// from any thread. Idempotent.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null.
///
/// After cancel, `_next_event` returns `TST_E_CLOSED`. The handle must
/// still be `_close`'d to free.
///
/// # Safety
///
/// `p` must be NULL or a valid non-freed `*mut TstRtpDemuxReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_cancel(p: *mut TstRtpDemuxReceiver) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
            return TstError::InvalidConfig as i32;
        };
        handle.inner.cancel();
        0
    })
}

/// Read the recorded reason this `tst_rtp_demux_receiver_t` receive
/// session ended, if any.
///
/// Writes `TstStreamEndReason::None` (returns `0`) when the session
/// hasn't ended yet, or ended through an uninstrumented path — and in
/// that case the thread-local last-error channel is
/// left untouched (any pending failure from an earlier call is still
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
/// open/conversion time WITHOUT acquiring this handle's data-path Mutex —
/// same rationale as `tst_rtp_demux_receiver_cancel` (a concurrent
/// `_next_event` may be blocked holding it). This is what makes the
/// getter safe to poll from a watchdog thread while another thread
/// drives `_next_event`. One consequence: this call never itself
/// returns `TST_E_CLOSED` — after `_close` the whole handle is freed,
/// and calling anything on it, including this getter, is a
/// use-after-free the caller must avoid.
///
/// # Safety
///
/// `p` must be a valid non-freed `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open` or `tst_rtsp_session_into_demux_receiver`.
/// `out` must point to a writable `TstStreamEndReason`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_end_reason(
    p: *mut TstRtpDemuxReceiver,
    out: *mut TstStreamEndReason,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
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

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Snapshot aggregate stats for a `tst_rtp_demux_receiver_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// NOTE: per-PID counters are NOT included here — call
/// `tst_rtp_demux_receiver_get_stream_stats` to retrieve them.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open`. `out` must point to a writable
/// `TstDemuxReceiverStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_get_stats(
    p: *mut TstRtpDemuxReceiver,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe { crate::transport_impls::demux_receiver_get_stats(&handle.inner, out) }
}

/// Read wire-level transport stats for the underlying RTP socket.
///
/// `out` MUST point to a writable `TstSocketStats`; the function zeros
/// the struct on failure.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is null,
/// `TST_E_NOT_AVAILABLE` if no live stats are available, or
/// `TST_E_CLOSED` if the handle was closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open`. `out` must point to a writable
/// `TstSocketStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_get_socket_stats(
    p: *mut TstRtpDemuxReceiver,
    out: *mut crate::stats::TstSocketStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_socket_stats(
            &handle.inner,
            out,
            "rtp demux receiver socket stats unavailable (transport not connected or closed)",
        )
    }
}

/// Snapshot codec-specific stats for one PID on a
/// `tst_rtp_demux_receiver_t`.
///
/// The returned struct is a tagged union — read `out->kind` first, then
/// the matching `out->u.<arm>` field.
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_rtp_demux_receiver_open`.
/// `out` must be a writable `tst_stream_codec_stats_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_get_stream_codec_stats(
    p: *mut TstRtpDemuxReceiver,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_codec_stats(
            &handle.inner,
            pid,
            out,
            &format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this rtp demux receiver)"
            ),
        )
    }
}

/// Read the Unix-epoch microsecond timestamp of the last item observed
/// on `pid` into `*out_epoch_micros`. `0` when `pid` has never been
/// observed on this handle (see
/// [`tst_demux_receiver_get_stream_last_seen_micros`](crate::receiver::demux_receiver::tst_demux_receiver_get_stream_last_seen_micros)
/// for full semantics — same shape, different handle type).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open`. `out_epoch_micros` must point to a
/// writable `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_get_stream_last_seen_micros(
    p: *mut TstRtpDemuxReceiver,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_last_seen_micros(
            &handle.inner,
            pid,
            out_epoch_micros,
        )
    }
}

/// Reset stats counters for a `tst_rtp_demux_receiver_t` to zero.
/// Also invalidates the borrowed `_get_stream_stats` snapshot
/// (design §4.5).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is null,
/// or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_reset_stats(
    p: *mut TstRtpDemuxReceiver,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    crate::transport_impls::demux_receiver_reset_stats(&handle.inner, &handle.stream_stats_buf)
}

/// Snapshot per-PID stats for a `tst_rtp_demux_receiver_t` into the
/// handle's internal buffer; return a `(*const TstStreamStats, size_t)`
/// pair borrowing that buffer.
///
/// **Borrowed buffer lifetime (design §4.5):** `*out_array` is valid
/// until the next `_get_stream_stats` / `_reset_stats` / `_close`
/// call on the same handle. Callers wanting longer lifetime memcpy
/// the array out.
///
/// Capped at `TST_STATS_MAX_STREAMS = 64` entries (ascending PID order).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on any null pointer
/// arg, or `TST_E_CLOSED` if the receiver has been closed.
///
/// # Safety
///
/// `p` must be a valid `*mut TstRtpDemuxReceiver` opened via
/// `tst_rtp_demux_receiver_open`. `out_array` and `out_count` must be
/// valid non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtp_demux_receiver_get_stream_stats(
    p: *mut TstRtpDemuxReceiver,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> libc::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null rtp demux receiver pointer");
        return TstError::InvalidConfig as i32;
    };
    unsafe {
        crate::transport_impls::demux_receiver_get_stream_stats(
            &handle.inner,
            &handle.stream_stats_buf,
            out_array,
            out_count,
        )
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::null_entry_tests;

    null_entry_tests! {
        null_close_is_safe => no_crash { tst_rtp_demux_receiver_close(std::ptr::null_mut()) };
        null_cancel_returns_invalid_config => invalid_config {
            tst_rtp_demux_receiver_cancel(std::ptr::null_mut())
        };
        null_next_event_returns_invalid_config => invalid_config {
            let mut ev = TstEvent::default();
            tst_rtp_demux_receiver_next_event(std::ptr::null_mut(), &mut ev)
        };
        null_out_event_returns_invalid_config => invalid_config {
            tst_rtp_demux_receiver_next_event(std::ptr::null_mut(), std::ptr::null_mut())
        };
        null_get_stats_returns_invalid_config => invalid_config {
            let mut stats = crate::stats::TstDemuxReceiverStats::default();
            tst_rtp_demux_receiver_get_stats(std::ptr::null_mut(), &mut stats)
        };
        null_reset_stats_returns_invalid_config => invalid_config {
            tst_rtp_demux_receiver_reset_stats(std::ptr::null_mut())
        };
        null_get_stream_last_seen_micros_returns_invalid_config => invalid_config {
            let mut micros: u64 = 0;
            tst_rtp_demux_receiver_get_stream_last_seen_micros(
                std::ptr::null_mut(),
                0x1011,
                &mut micros,
            )
        };
        null_get_stream_stats_returns_invalid_config => invalid_config {
            let mut arr: *const crate::stats::TstStreamStats = std::ptr::null();
            let mut count: libc::size_t = 0;
            tst_rtp_demux_receiver_get_stream_stats(std::ptr::null_mut(), &mut arr, &mut count)
        };
        open_with_null_url_returns_null => null {
            tst_rtp_demux_receiver_open(std::ptr::null(), std::ptr::null())
        };
        null_end_reason_returns_invalid_config => invalid_config {
            let mut out = TstStreamEndReason::None;
            tst_rtp_demux_receiver_end_reason(std::ptr::null_mut(), &mut out)
        };
    }

    #[test]
    fn null_out_end_reason_returns_invalid_config() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        let rc = unsafe { tst_rtp_demux_receiver_end_reason(handle, std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
        unsafe { tst_rtp_demux_receiver_close(handle) };
    }

    #[test]
    fn fresh_demux_receiver_end_reason_is_none() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        // Seed a pending failure that a "hasn't ended" result must NOT
        // clobber (see the getter's doc: only an ACTUALLY-recorded reason
        // touches last-error).
        set_last_error(TstError::Internal, "sentinel-untouched");

        let mut out = TstStreamEndReason::Cancelled; // seed with a non-None value
        let rc = unsafe { tst_rtp_demux_receiver_end_reason(handle, &mut out) };
        assert_eq!(rc, 0);
        assert!(matches!(out, TstStreamEndReason::None));

        assert_eq!(
            unsafe { crate::error::tst_get_last_error() },
            TstError::Internal as i32
        );
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { std::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "sentinel-untouched");

        unsafe { tst_rtp_demux_receiver_close(handle) };
    }

    /// `_cancel` alone only flags the transport; the reason is recorded
    /// by the underlying `RtpRecvTransport` the moment a recv attempt
    /// actually observes the cancel signal — so this drives one
    /// `_next_event` call after cancelling to make the reason
    /// observable, matching what a real caller's event loop would do.
    #[test]
    fn cancel_then_next_event_records_cancelled_end_reason() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        if handle.is_null() {
            return; // skip if bind fails in CI
        }
        let cancel_rc = unsafe { tst_rtp_demux_receiver_cancel(handle) };
        assert_eq!(cancel_rc, 0);

        let mut ev = TstEvent::default();
        let next_rc = unsafe { tst_rtp_demux_receiver_next_event(handle, &mut ev) };
        assert_eq!(next_rc, TstError::Closed as i32);

        // next_rc above already set last-error to (Closed, "...cancelled or
        // closed by caller..."). Do NOT clear it here — the getter below
        // must overwrite THAT pending state on its own, per the documented
        // last-error contract (see tst_rtp_demux_receiver_end_reason's doc).
        let mut out = TstStreamEndReason::None;
        let rc = unsafe { tst_rtp_demux_receiver_end_reason(handle, &mut out) };
        assert_eq!(rc, 0);
        assert!(matches!(out, TstStreamEndReason::Cancelled));

        // Pin the last-error contract through the real C entry points:
        // Cancelled has no msg, so the getter must reset last-error to
        // (Success, "") — overwriting the next_event Closed error above.
        assert_eq!(unsafe { crate::error::tst_get_last_error() }, 0);
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { std::ffi::CStr::from_ptr(s_ptr) };
        assert_eq!(s.to_str().unwrap(), "");

        unsafe { tst_rtp_demux_receiver_close(handle) };
    }

    /// `?recv_timeout=` must actually arm the transport through
    /// `tst_rtp_demux_receiver_open` — regression test for the gap where
    /// the open path reconstructed a fresh `RtpUrl` (host/port/iface only)
    /// instead of retaining the parsed URL's `recv_timeout`, silently
    /// dropping the knob. A quiet socket (no sender) must return
    /// `TST_E_BUFFER_FULL` well inside the 5 s CI-flake margin, not block
    /// indefinitely.
    #[test]
    fn url_recv_timeout_arms_transport_via_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?recv_timeout=200").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        assert!(
            !handle.is_null(),
            "tst_rtp_demux_receiver_open should succeed for a valid ?recv_timeout= URL"
        );
        let mut ev = TstEvent::default();
        let start = std::time::Instant::now();
        let rc = unsafe { tst_rtp_demux_receiver_next_event(handle, &mut ev) };
        let elapsed = start.elapsed();
        assert_eq!(rc, TstError::BufferFull as i32, "expected deadline expiry");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "next_event blocked past the configured 200ms deadline: {elapsed:?}"
        );
        unsafe { tst_rtp_demux_receiver_close(handle) };
    }

    /// `?pkt_size=` is documented as "send-side only and is rejected on
    /// receive URLs" (see `tst_rtp_demux_receiver_open`'s doc) — same
    /// regression coverage as the receiver-family test: the prior
    /// field-by-field builder rebuild silently dropped `pkt_size`, so
    /// this URL would have wrongly succeeded before the `listen_with` fix.
    #[test]
    fn url_pkt_size_rejected_on_demux_receiver_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?pkt_size=1316").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        assert!(
            handle.is_null(),
            "tst_rtp_demux_receiver_open must reject ?pkt_size= on a receive URL"
        );
    }

    /// `?pt=` is meaningless on a raw-TS receiver — same regression class
    /// as `?pkt_size=` above (the field-by-field builder rebuild would
    /// have silently dropped it too).
    #[test]
    fn url_pt_rejected_on_demux_receiver_open() {
        let url = std::ffi::CString::new("rtp://127.0.0.1:0?pt=96").unwrap();
        let handle = unsafe { tst_rtp_demux_receiver_open(url.as_ptr(), std::ptr::null()) };
        assert!(
            handle.is_null(),
            "tst_rtp_demux_receiver_open must reject ?pt= on a receive URL"
        );
    }
}
