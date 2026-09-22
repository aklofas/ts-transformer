//! Generic transport body implementations (SIMP-CBIND-1).
//!
//! Each function here implements one logical C entry-point body, generic over
//! a pipeline shell type (`MuxSender<T>`, `Sender<T>`, `Receiver<R>`,
//! `DemuxReceiver<R>`). The transport-family-specific `*.rs` files contain
//! the literal `#[unsafe(no_mangle)] pub unsafe extern "C" fn …` wrappers
//! that do the null-check with the protocol-specific error string and then
//! call into this module.
//!
//! **Governing constraints:**
//! - No `extern "C"` declarations here — cbindgen must see them at their
//!   literal call sites in the family modules.
//! - This module requires `std` (all transport features gate on it).
//! - The functions are generic via monomorphization; they do NOT add new
//!   symbols to the ABI.
//! - A body consumed by only some transport families carries a `#[cfg]`
//!   matching exactly those families (and so do the imports only it needs):
//!   CI runs `clippy -D warnings` per single-feature combo, so an ungated
//!   body with no caller in that combo is a hard failure, not a warning.
//!
//! Organization:
//! - MuxSender (push/stats)
//! - Sender (raw TS bytes)
//! - Receiver (raw TS bytes recv)
//! - DemuxReceiver (event recv + stats)

use std::sync::Mutex;

use tst_core::mpegts::common::Pts90khz;
#[cfg(any(feature = "udp", feature = "tcp", feature = "rist"))]
use tst_core::mpegts::common::TS_PACKET_SIZE;
use tst_core::mpegts::mux::{
    AudioStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::{DemuxReceiver, MuxSender, Receiver, Sender};
#[cfg(feature = "srt")]
use tst_pipeline::{ManagedDemuxReceiver, RawReceiver, RawSender};

#[cfg(feature = "srt")]
use crate::error::record_internal;
use crate::error::{
    TstError, record_mux_error, record_not_available, record_not_found, record_shell_error,
    set_last_error, tst_get_last_error,
};
#[cfg(any(feature = "rtp", feature = "udp", feature = "tcp", feature = "rist"))]
use crate::error::{record_recv_closed, record_recv_error};
#[cfg(any(feature = "rtp", feature = "udp", feature = "tcp", feature = "rist"))]
use crate::event::{EventArena, TstEvent};
use crate::handle::CHandle;

// ============================================================================
// MuxSender<T> generic push impls
// ============================================================================
//
// Each function receives a `&CHandle<MuxSender<T>, S>` (after null-check in the
// family forwarder) and raw FFI parameters. No null-check on the handle itself
// (already done by caller); null-checks on pointer args remain here.

/// Generic body for `tst_*_mux_sender_push_video`.
///
/// # Safety
/// `nal` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_video<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_video(slice, pts, key_frame) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_klv`.
///
/// # Safety
/// `klv` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_klv<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_klv(slice, pts, 0x00) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_audio`.
///
/// # Safety
/// `frames` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_audio<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_audio(slice, pts) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_subtitle`.
///
/// # Safety
/// `payload` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_subtitle<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_subtitle(slice, pts) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_video_to`.
///
/// Includes the `try_from_raw` trust-boundary guard (rejects forged
/// stream handles before they reach the push-time range check).
///
/// # Safety
/// `nal` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_video_to<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    stream_handle: u32,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(stream_handle) {
        Ok(s) => s,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_video_to(stream, slice, pts, key_frame) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_klv_to`.
///
/// # Safety
/// `klv` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_klv_to<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    stream_handle: u32,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match KlvStreamHandle::try_from_raw(stream_handle) {
        Ok(s) => s,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_klv_to(stream, slice, pts, 0x00) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_audio_to`.
///
/// # Safety
/// `frames` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_audio_to<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    stream_handle: u32,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match AudioStreamHandle::try_from_raw(stream_handle) {
        Ok(s) => s,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_audio_to(stream, slice, pts) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

/// Generic body for `tst_*_mux_sender_push_subtitle_to`.
///
/// # Safety
/// `payload` must be readable for `len` bytes.
pub(crate) unsafe fn mux_sender_push_subtitle_to<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    stream_handle: u32,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match SubtitleStreamHandle::try_from_raw(stream_handle) {
        Ok(s) => s,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.with_inner_ref(|s| match s.send_subtitle_to(stream, slice, pts) {
        Ok(()) => 0,
        Err(e) => {
            record_shell_error(&e);
            unsafe { tst_get_last_error() }
        }
    })
}

// ============================================================================
// MuxSender<T> generic stats impls
// ============================================================================

/// Generic body for `tst_*_mux_sender_get_mux_sender_stats`.
///
/// Handles the null-check on `out`; the handle null-check is already done by
/// the family forwarder.
///
/// # Safety
/// `out` must be a valid writable `*mut TstMuxSenderStats` when non-null.
pub(crate) unsafe fn mux_sender_get_mux_sender_stats<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    out: *mut crate::stats::TstMuxSenderStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|s| {
        unsafe { *out = crate::stats::mux_sender_stats_to_c(&s.stats()) };
        0
    })
}

/// Generic body for `tst_*_mux_sender_get_socket_stats`.
///
/// Zeros `*out` before the inner call so callers see a defined value on error.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
pub(crate) unsafe fn mux_sender_get_socket_stats<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|s| match s.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_mux_sender_get_stream_codec_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstStreamCodecStats` when non-null.
pub(crate) unsafe fn mux_sender_get_stream_codec_stats<T: Transport, S>(
    h: &CHandle<MuxSender<T>, S>,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
    not_found_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|s| match s.stream_codec_stats(pid) {
        Some(stats) => {
            unsafe { *out = crate::stats::codec_stats_to_c(stats) };
            0
        }
        None => record_not_found(not_found_msg),
    })
}

/// Generic body for `tst_*_mux_sender_reset_stats`.
pub(crate) fn mux_sender_reset_stats<T: Transport, S>(h: &CHandle<MuxSender<T>, S>) -> i32 {
    h.with_inner_ref(|s| {
        s.reset_stats();
        0
    })
}

// ============================================================================
// Managed-reconnect stats (shared across all three send shells)
// ============================================================================
//
// `ManagedStatsHandle` is not parameterized on the transport or the shell
// type, so this one body serves `tst_managed_{sender,mux_sender,raw_sender}
// _get_reconnect_stats` — the `T` parameter is just whatever shell type the
// caller's `CHandle<T, ManagedStatsHandle>` wraps (used only for the
// closed-check; the stats themselves come from the handle's snapshot, which
// on the three managed SEND classes IS the `ManagedStatsHandle`).

/// Generic body for `tst_managed_*_get_reconnect_stats`.
///
/// Runs the closed-check through `h` (so a closed handle reports
/// `TST_E_CLOSED` like every other managed getter) but reads the counters
/// from `sh`, which stays live independently of the shell's `CHandle` state.
///
/// **Not fully non-blocking in `Blocking` mode:** the closed-check
/// acquires `h`'s lock via `with_inner_ref`, the same lock a send stuck
/// in `Blocking` mode's inline reconnect loop holds via `with_inner_mut`
/// for the whole outage — so this getter can block for the outage's
/// duration too in that mode. Polling this getter without ever blocking
/// is a `Background`-mode property (the mode these stats primarily exist
/// to observe).
///
/// # Safety
/// `out` must be a valid writable `*mut TstManagedTransportStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_get_reconnect_stats<T>(
    h: &CHandle<T, tst_pipeline::ManagedStatsHandle>,
    out: *mut crate::stats::TstManagedTransportStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    // The observer IS the handle's construction-time snapshot: read it
    // lock-free, then run the closed-check through the slot.
    let sh = h.snapshot();
    h.with_inner_ref(|_| match sh.stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => {
            record_internal("managed transport stats gap-buffer lock poisoned");
            TstError::Internal as i32
        }
    })
}

// ============================================================================
// Sender<T> (raw TS) generic impls
// ============================================================================

/// Generic body for `tst_*_sender_send_ts`.
///
/// # Safety
/// `bytes` must be readable for `len` bytes.
#[cfg(any(feature = "rtp", feature = "udp", feature = "tcp", feature = "rist"))]
pub(crate) unsafe fn sender_send_ts<T: Transport, S>(
    h: &CHandle<Sender<T>, S>,
    bytes: *const u8,
    len: usize,
) -> i32 {
    let slice = match unsafe { crate::ffi_slice::ffi_slice(bytes, len, "bytes") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    h.with_inner_mut(|s| match s.send_ts(slice) {
        Ok(()) => 0,
        Err(e) => record_shell_error(&e),
    })
}

/// Generic body for `tst_*_sender_get_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSenderStats` when non-null.
pub(crate) unsafe fn sender_get_stats<T: Transport, S>(
    h: &CHandle<Sender<T>, S>,
    out: *mut crate::stats::TstSenderStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|s| {
        let stats = crate::stats::TstSenderStats::from(&s.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_*_sender_get_socket_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
pub(crate) unsafe fn sender_get_socket_stats<T: Transport, S>(
    h: &CHandle<Sender<T>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|s| match s.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_sender_reset_stats`.
pub(crate) fn sender_reset_stats<T: Transport, S>(h: &CHandle<Sender<T>, S>) -> i32 {
    h.with_inner_mut(|s| {
        s.reset_stats();
        0
    })
}

// ============================================================================
// RawSender<T> generic impls
// ============================================================================

/// Generic body for `tst_*_sender_get_stats` on a `RawSender<T>` handle.
///
/// # Safety
/// `out` must be a valid writable `*mut TstRawSendStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn raw_sender_get_stats<T: Transport, S>(
    h: &CHandle<RawSender<T>, S>,
    out: *mut crate::stats::TstRawSendStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|s| {
        let stats = crate::stats::TstRawSendStats::from(&s.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_*_sender_get_socket_stats` on a `RawSender<T>` handle.
///
/// Reaches through `RawSender::transport()` — unlike `Sender<T>`, `RawSender`
/// exposes no `socket_stats()` shell passthrough.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn raw_sender_get_socket_stats<T: Transport, S>(
    h: &CHandle<RawSender<T>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|s| match s.transport().socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_sender_reset_stats` on a `RawSender<T>` handle.
#[cfg(feature = "srt")]
pub(crate) fn raw_sender_reset_stats<T: Transport, S>(h: &CHandle<RawSender<T>, S>) -> i32 {
    h.with_inner_mut(|s| {
        s.reset_stats();
        0
    })
}

// ============================================================================
// Receiver<R> (raw TS recv) generic impls
// ============================================================================

/// Generic body for `tst_*_receiver_recv_ts` (blocking 188-byte packet read).
///
/// Validates that `buf` is non-null and `buf_len ≥ TS_PACKET_SIZE (188)`. The
/// stream-end outcomes route through `record_recv_error`: a handle the caller
/// cancelled reports `TST_E_CLOSED`, a peer close reports
/// `TST_E_END_OF_STREAM` (`broken_is_eos = true` — these are plain shells).
///
/// # Safety
/// `buf` must be writable for `buf_len` bytes; `out_n` must be non-null.
#[cfg(any(feature = "udp", feature = "tcp", feature = "rist"))]
pub(crate) unsafe fn receiver_recv_ts<R: RecvTransport, S>(
    h: &CHandle<Receiver<R>, S>,
    buf: *mut u8,
    buf_len: usize,
    out_n: *mut usize,
) -> i32 {
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
    // The cancel latch is read on both sides of the park. The pre-park read is
    // belt-and-braces only: the latch never resets, so the post-park read
    // already covers a cancel that lands while the call is blocked. Keeping
    // both makes the intent explicit at every recv site.
    let cancelled = h.is_cancelled();
    h.with_inner_mut(|rx| match rx.next_packet() {
        Ok(pkt) => {
            // SAFETY: buf non-null + writable for >= TS_PACKET_SIZE bytes per guard above.
            unsafe {
                std::ptr::copy_nonoverlapping(pkt.as_ptr(), buf, TS_PACKET_SIZE);
                *out_n = TS_PACKET_SIZE;
            }
            0
        }
        Err(e) => record_recv_error(&e, cancelled || h.is_cancelled(), true),
    })
}

/// Generic body for `tst_*_receiver_get_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstReceiverStats` when non-null.
pub(crate) unsafe fn receiver_get_stats<R: RecvTransport, S>(
    h: &CHandle<Receiver<R>, S>,
    out: *mut crate::stats::TstReceiverStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = crate::stats::TstReceiverStats::from(&rx.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_*_receiver_get_socket_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
pub(crate) unsafe fn receiver_get_socket_stats<R: RecvTransport, S>(
    h: &CHandle<Receiver<R>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|rx| match rx.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_receiver_reset_stats`.
pub(crate) fn receiver_reset_stats<R: RecvTransport, S>(h: &CHandle<Receiver<R>, S>) -> i32 {
    h.with_inner_mut(|rx| {
        rx.reset_stats();
        0
    })
}

// ============================================================================
// RawReceiver<R> generic impls
// ============================================================================

/// Generic body for `tst_*_receiver_get_stats` on a `RawReceiver<R>` handle.
///
/// # Safety
/// `out` must be a valid writable `*mut TstRawRecvStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn raw_receiver_get_stats<R: RecvTransport, S>(
    h: &CHandle<RawReceiver<R>, S>,
    out: *mut crate::stats::TstRawRecvStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = crate::stats::TstRawRecvStats::from(&rx.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_*_receiver_get_socket_stats` on a `RawReceiver<R>` handle.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn raw_receiver_get_socket_stats<R: RecvTransport, S>(
    h: &CHandle<RawReceiver<R>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|rx| match rx.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_receiver_reset_stats` on a `RawReceiver<R>` handle.
#[cfg(feature = "srt")]
pub(crate) fn raw_receiver_reset_stats<R: RecvTransport, S>(h: &CHandle<RawReceiver<R>, S>) -> i32 {
    h.with_inner_mut(|rx| {
        rx.reset_stats();
        0
    })
}

// ============================================================================
// DemuxReceiver<R> generic impls
// ============================================================================

/// Generic body for `tst_*_demux_receiver_next_event`.
///
/// `Ok(None)` from `recv_event` means the stream ended; so do the `Closed` /
/// `EndOfStream` / `TransportBroken` errors of a plain shell. All of them go
/// through `record_recv_closed` / `record_recv_error`, which read the
/// binding-shared cancel flag off the handle: a caller who cancelled sees
/// `TST_E_CLOSED`, a peer close sees `TST_E_END_OF_STREAM`.
///
/// # Safety
/// `out_event` must be a valid writable `*mut TstEvent` when non-null.
#[cfg(any(feature = "rtp", feature = "udp", feature = "tcp", feature = "rist"))]
pub(crate) unsafe fn demux_receiver_next_event<R: RecvTransport, S>(
    inner: &CHandle<DemuxReceiver<R>, S>,
    arena: &Mutex<EventArena>,
    out_event: *mut TstEvent,
) -> i32 {
    if out_event.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_event pointer");
        return TstError::InvalidConfig as i32;
    }
    // The cancel latch is read on both sides of the park. The pre-park read is
    // belt-and-braces only: the latch never resets, so the post-park read
    // already covers a cancel that lands while the call is blocked. Keeping
    // both makes the intent explicit at every recv site.
    let cancelled = inner.is_cancelled();
    inner.with_inner_mut(|rx| match rx.recv_event() {
        Ok(Some(ev)) => {
            let mut arena = arena.lock().expect("event arena Mutex poisoned");
            // SAFETY: out_event non-null per guard above. event::convert writes
            // through the pointer; pointer fields on the result alias arena Vecs
            // (held under the arena Mutex for this call — stable until next call).
            unsafe { crate::event::convert(&mut arena, &ev, &mut *out_event) };
            0
        }
        Ok(None) => record_recv_closed(cancelled || inner.is_cancelled()),
        Err(e) => record_recv_error(&e, cancelled || inner.is_cancelled(), true),
    })
}

/// Generic body for `tst_*_demux_receiver_get_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstDemuxReceiverStats` when non-null.
pub(crate) unsafe fn demux_receiver_get_stats<R: RecvTransport, S>(
    h: &CHandle<DemuxReceiver<R>, S>,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = crate::stats::TstDemuxReceiverStats::from(&rx.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_*_demux_receiver_get_socket_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
pub(crate) unsafe fn demux_receiver_get_socket_stats<R: RecvTransport, S>(
    h: &CHandle<DemuxReceiver<R>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|rx| match rx.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_*_demux_receiver_get_stream_codec_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstStreamCodecStats` when non-null.
pub(crate) unsafe fn demux_receiver_get_stream_codec_stats<R: RecvTransport, S>(
    h: &CHandle<DemuxReceiver<R>, S>,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
    not_found_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| match rx.stream_codec_stats(pid) {
        Some(stats) => {
            unsafe { *out = crate::stats::codec_stats_to_c(stats) };
            0
        }
        None => record_not_found(not_found_msg),
    })
}

/// Generic body for `tst_*_demux_receiver_get_stream_last_seen_micros`.
///
/// # Safety
/// `out_epoch_micros` must be a valid writable `*mut u64` when non-null.
pub(crate) unsafe fn demux_receiver_get_stream_last_seen_micros<R: RecvTransport, S>(
    h: &CHandle<DemuxReceiver<R>, S>,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> i32 {
    if out_epoch_micros.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_epoch_micros pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = rx.stats();
        let micros = stats
            .per_stream
            .get(&pid)
            .map(|ss| crate::stats::last_seen_epoch_micros(ss.last_seen))
            .unwrap_or(0);
        unsafe { *out_epoch_micros = micros };
        0
    })
}

/// Generic body for `tst_*_demux_receiver_reset_stats`.
///
/// Clears the borrowed `stream_stats_buf` snapshot before resetting to ensure
/// any pointer previously returned by `_get_stream_stats` is no longer valid.
pub(crate) fn demux_receiver_reset_stats<R: RecvTransport, S>(
    inner: &CHandle<DemuxReceiver<R>, S>,
    stream_stats_buf: &Mutex<Vec<crate::stats::TstStreamStats>>,
) -> i32 {
    if let Ok(mut buf) = stream_stats_buf.lock() {
        buf.clear();
    }
    inner.with_inner_mut(|rx| {
        rx.reset_stats();
        0
    })
}

/// Generic body for `tst_*_demux_receiver_get_stream_stats` (borrowed-buffer design §4.5).
///
/// # Safety
/// `out_array` and `out_count` must be valid non-null pointers.
pub(crate) unsafe fn demux_receiver_get_stream_stats<R: RecvTransport, S>(
    inner: &CHandle<DemuxReceiver<R>, S>,
    stream_stats_buf: &Mutex<Vec<crate::stats::TstStreamStats>>,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> i32 {
    if out_array.is_null() || out_count.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            "null out_array or out_count pointer",
        );
        return TstError::InvalidConfig as i32;
    }
    inner.with_inner_ref(|rx| {
        let stats = rx.stats();
        let mut buf = stream_stats_buf
            .lock()
            .expect("stream_stats_buf Mutex poisoned");
        buf.clear();
        let cap = crate::stats::TST_STATS_MAX_STREAMS;
        for (pid, ss) in stats.per_stream.iter().take(cap) {
            let mut c_ss = crate::stats::TstStreamStats {
                pid: *pid,
                ..Default::default()
            };
            crate::stats::fill_stream_stats(&mut c_ss, ss);
            buf.push(c_ss);
        }
        // SAFETY: out_array / out_count non-null per guard above. The returned
        // pointer borrows from buf which lives on the handle until the next
        // _get_stream_stats / _reset_stats / _close call.
        unsafe {
            *out_array = buf.as_ptr();
            *out_count = buf.len();
        }
        0
    })
}

// ============================================================================
// ManagedDemuxReceiver<R> generic impls
// ============================================================================
//
// `tst_pipeline::ManagedDemuxReceiver<R>` is a distinct shell struct from
// `DemuxReceiver<R>` (it owns its own `Receiver<ManagedRecvTransport<R>>` +
// `Demuxer` pair for per-reconnect discontinuity handling), not a type alias
// or a wrapper generic over `DemuxReceiver` — so it needs its own set of
// generic bodies even though the method surface (`stats`/`socket_stats`/
// `stream_codec_stats`/`reset_stats`) is identical in shape to the plain
// receiver's. `recv_event`/`cancel` stay family-local in `managed.rs` (same
// reasoning as the plain receiver's cancel-aware `_recv_*` bodies).

/// Generic body for `tst_managed_*_demux_receiver_get_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstDemuxReceiverStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_demux_receiver_get_stats<R: RecvTransport, S>(
    h: &CHandle<ManagedDemuxReceiver<R>, S>,
    out: *mut crate::stats::TstDemuxReceiverStats,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = crate::stats::TstDemuxReceiverStats::from(&rx.stats());
        unsafe { *out = stats };
        0
    })
}

/// Generic body for `tst_managed_*_demux_receiver_get_socket_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstSocketStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_demux_receiver_get_socket_stats<R: RecvTransport, S>(
    h: &CHandle<ManagedDemuxReceiver<R>, S>,
    out: *mut crate::stats::TstSocketStats,
    not_available_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    unsafe { *out = crate::stats::TstSocketStats::default() };
    h.with_inner_ref(|rx| match rx.socket_stats() {
        Some(stats) => {
            unsafe { *out = (&stats).into() };
            0
        }
        None => record_not_available(not_available_msg),
    })
}

/// Generic body for `tst_managed_*_demux_receiver_get_stream_codec_stats`.
///
/// # Safety
/// `out` must be a valid writable `*mut TstStreamCodecStats` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_demux_receiver_get_stream_codec_stats<R: RecvTransport, S>(
    h: &CHandle<ManagedDemuxReceiver<R>, S>,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
    not_found_msg: &str,
) -> i32 {
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| match rx.stream_codec_stats(pid) {
        Some(stats) => {
            unsafe { *out = crate::stats::codec_stats_to_c(stats) };
            0
        }
        None => record_not_found(not_found_msg),
    })
}

/// Generic body for `tst_managed_*_demux_receiver_get_stream_last_seen_micros`.
///
/// # Safety
/// `out_epoch_micros` must be a valid writable `*mut u64` when non-null.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_demux_receiver_get_stream_last_seen_micros<R: RecvTransport, S>(
    h: &CHandle<ManagedDemuxReceiver<R>, S>,
    pid: u16,
    out_epoch_micros: *mut u64,
) -> i32 {
    if out_epoch_micros.is_null() {
        set_last_error(TstError::InvalidConfig, "null out_epoch_micros pointer");
        return TstError::InvalidConfig as i32;
    }
    h.with_inner_ref(|rx| {
        let stats = rx.stats();
        let micros = stats
            .per_stream
            .get(&pid)
            .map(|ss| crate::stats::last_seen_epoch_micros(ss.last_seen))
            .unwrap_or(0);
        unsafe { *out_epoch_micros = micros };
        0
    })
}

/// Generic body for `tst_managed_*_demux_receiver_reset_stats`.
///
/// Clears the borrowed `stream_stats_buf` snapshot before resetting, same
/// invalidation contract as the plain receiver's `demux_receiver_reset_stats`.
#[cfg(feature = "srt")]
pub(crate) fn managed_demux_receiver_reset_stats<R: RecvTransport, S>(
    inner: &CHandle<ManagedDemuxReceiver<R>, S>,
    stream_stats_buf: &Mutex<Vec<crate::stats::TstStreamStats>>,
) -> i32 {
    if let Ok(mut buf) = stream_stats_buf.lock() {
        buf.clear();
    }
    inner.with_inner_mut(|rx| {
        rx.reset_stats();
        0
    })
}

/// Generic body for `tst_managed_*_demux_receiver_get_stream_stats`
/// (borrowed-buffer design §4.5).
///
/// # Safety
/// `out_array` and `out_count` must be valid non-null pointers.
#[cfg(feature = "srt")]
pub(crate) unsafe fn managed_demux_receiver_get_stream_stats<R: RecvTransport, S>(
    inner: &CHandle<ManagedDemuxReceiver<R>, S>,
    stream_stats_buf: &Mutex<Vec<crate::stats::TstStreamStats>>,
    out_array: *mut *const crate::stats::TstStreamStats,
    out_count: *mut libc::size_t,
) -> i32 {
    if out_array.is_null() || out_count.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            "null out_array or out_count pointer",
        );
        return TstError::InvalidConfig as i32;
    }
    inner.with_inner_ref(|rx| {
        let stats = rx.stats();
        let mut buf = stream_stats_buf
            .lock()
            .expect("stream_stats_buf Mutex poisoned");
        buf.clear();
        let cap = crate::stats::TST_STATS_MAX_STREAMS;
        for (pid, ss) in stats.per_stream.iter().take(cap) {
            let mut c_ss = crate::stats::TstStreamStats {
                pid: *pid,
                ..Default::default()
            };
            crate::stats::fill_stream_stats(&mut c_ss, ss);
            buf.push(c_ss);
        }
        // SAFETY: out_array / out_count non-null per guard above. The returned
        // pointer borrows from buf which lives on the handle until the next
        // _get_stream_stats / _reset_stats / _close call.
        unsafe {
            *out_array = buf.as_ptr();
            *out_count = buf.len();
        }
        0
    })
}
