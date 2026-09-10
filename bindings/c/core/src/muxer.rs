//! `tst_muxer_t` — standalone MPEG-TS muxer utility.
//!
//! Wraps `tst_core::mpegts::mux::Muxer`. No transport — push NALs and KLV,
//! pull TS bytes. The handle is internally synchronized; push_video,
//! push_klv, and pull may be called from different threads.

use crate::config::TstMuxConfig;
use crate::error::{TstError, record_mux_error, record_not_found, set_last_error};
use crate::handle::{
    Handle, TstAudioStreamHandle, TstDataStreamHandle, TstKlvStreamHandle, TstSubtitleStreamHandle,
    TstVideoStreamHandle,
};
use alloc::boxed::Box;
use alloc::format;
use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, Muxer, StreamKind, SubtitleStreamHandle,
    VideoStreamHandle,
};

pub struct TstMuxer {
    inner: Handle<Muxer>,
}

/// Open a standalone muxer. Builds the config from `cfg` so the caller may
/// free it immediately after this returns. Returns NULL on failure with
/// last-error set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_open(cfg: *mut TstMuxConfig) -> *mut TstMuxer {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return core::ptr::null_mut();
        };
        let built = match cfg.build_config() {
            Ok(c) => c,
            Err(e) => {
                record_mux_error(&e);
                return core::ptr::null_mut();
            }
        };
        let muxer = match Muxer::new(built) {
            Ok(m) => m,
            Err(e) => {
                record_mux_error(&e);
                return core::ptr::null_mut();
            }
        };
        Box::into_raw(Box::new(TstMuxer {
            inner: Handle::new(muxer),
        }))
    })
}

/// Push one Annex-B-framed video access unit. Returns 0 on success or a
/// negative TST_E_* code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video(
    p: *mut TstMuxer,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_mut(|m| match m.push_video(slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                // Find the matching TST_E_* code via the recorded last-error.
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one KLV blob onto the muxer's single KLV stream.
///
/// `klv` must point to **raw MISB Local Set bytes**. For streams configured
/// as `TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA`, the muxer prepends a
/// 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 §2.12.4.2 before
/// emitting. **Do not pre-wrap the AU cell on the caller side** —
/// double-wrapping produces metadata that receivers cannot parse. For
/// streams configured as `TST_KLV_STREAM_TYPE_PRIVATE_DATA`, the payload
/// is emitted as-is.
///
/// `pts_90khz` is the presentation timestamp in 90 kHz ticks, lives in
/// the PES header, and is the same value pulled by demux-side consumers.
///
/// Single-stream form: the muxer must have exactly one KLV stream
/// configured. Multi-stream callers use `tst_muxer_push_klv_to` with an
/// explicit `TstKlvStreamHandle`.
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — no KLV stream configured (`NoKlvStreamsConfigured`).
/// - `TST_E_INVALID_USAGE` — more than one KLV stream configured
///   (`AmbiguousTarget` — caller must use `_to` variant).
/// - `TST_E_KLV_TOO_LARGE` — payload exceeds the per-frame KLV size limit
///   (`KlvTooLarge`).
/// - `TST_E_INVALID_CONFIG` — `klv` is null with non-zero `len`.
///
/// # C ABI
///
/// `tst_muxer_push_klv` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_klv(
    p: *mut TstMuxer,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle.inner.with_inner_mut(|m| {
        match m.push_klv(
            slice, pts,
            // C ABI receiver-surface plan will expose metadata_service_id;
            // today defaults to 0x00 per ST 1402.2 App. B Table 2.
            0x00,
        ) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        }
    })
}

/// Push one Annex-B NAL targeting a specific video elementary stream.
///
/// `handle` is obtained from `tst_mux_config_add_video_stream` at config
/// time and is stable across managed-sender reconnects. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream muxer, prefer `tst_muxer_push_video` — it has the
/// same effect and doesn't require a handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_to(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation: reject any caller-provided u32 that has
    // bits set outside the canonical 8-bit packed layout. Without this,
    // `valid.raw() | 0x100` would mask down to the valid low byte and
    // alias the wrong elementary stream — the push-time range check
    // sees only the masked indices and cannot distinguish.
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner
        .with_inner_mut(|m| match m.push_video_to(stream, slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push an already-carried on-wire video access unit onto the muxer's
/// single video stream (single-stream shorthand).
///
/// Emits `wire` verbatim — no Annex-B start-code validation, no AV1 OBU
/// re-wrapping. Intended for byte-faithful transmux: demux a sample
/// (`tst_demux_receiver_recv_event` / `tst_demuxer_next_event`), take
/// `ev.u.sample.payload`, read `ev.u.sample.av1_carriage`, configure the
/// destination muxer's carriage via `tst_mux_config_set_av1_carriage`, then
/// push through this function. For H.264/H.265/H.266 you may use
/// `tst_muxer_push_video` instead (Annex-B is structurally unchanged after
/// demux).
///
/// Resolves only when exactly one video stream is configured across all
/// programs. Otherwise rejects with `TST_E_INVALID_USAGE` (carrying
/// `AmbiguousTarget`).
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — zero or more than one video stream configured.
/// - `TST_E_BUFFER_FULL` — TS-packet output buffer would exceed capacity.
/// - `TST_E_INVALID_CONFIG` — `wire` is null with non-zero `len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_wire(
    p: *mut TstMuxer,
    wire: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(wire, len, "wire") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle.inner.with_inner_mut(|m| {
        // Single-stream resolution: same ambiguity contract as push_video.
        let handles = m.video_handles();
        let h = match handles.as_slice() {
            [single] => *single,
            _ => {
                let e = MuxError::AmbiguousTarget {
                    kind: StreamKind::Video,
                    count: handles.len(),
                };
                record_mux_error(&e);
                return unsafe { crate::error::tst_get_last_error() };
            }
        };
        match m.push_video_wire_to(h, slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        }
    })
}

/// Push an already-carried on-wire video access unit targeting a specific
/// video elementary stream.
///
/// Emits `wire` verbatim — no Annex-B start-code validation, no AV1 OBU
/// re-wrapping. See `tst_muxer_push_video_wire` for the byte-faithful
/// transmux workflow.
///
/// `handle` is obtained from `tst_mux_config_add_video_stream` at config
/// time and is stable across managed-sender reconnects. On a single-stream
/// muxer, prefer `tst_muxer_push_video_wire` — same effect, no handle
/// required.
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — `handle` index is out of range for this muxer.
/// - `TST_E_BUFFER_FULL` — TS-packet output buffer would exceed capacity.
/// - `TST_E_INVALID_CONFIG` — `wire` is null with non-zero `len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_wire_to(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    wire: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(wire, len, "wire") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner.with_inner_mut(
        |m| match m.push_video_wire_to(stream, slice, pts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        },
    )
}

/// Push one access unit with explicit composition (PTS) and decode (DTS)
/// timestamps, targeting a specific video elementary stream. Required for
/// codecs that emit reordered output (B-frames in H.264/H.265/H.266/AV1).
///
/// Emits PES with `PTS_DTS_flags = '11'` per ISO/IEC 13818-1 §2.4.3.6
/// (10-byte PES header carrying both PTS and DTS). `handle` is obtained
/// from `tst_mux_config_add_video_stream`. The muxer does not enforce
/// `dts <= pts`; receivers reject inverted timestamps. There is no
/// single-stream shorthand — resolve the lone stream's handle from
/// `tst_mux_config_add_video_stream` when only one video stream exists.
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — `handle` index is out of range for this muxer.
/// - `TST_E_INVALID_NAL` — `nal` is not Annex-B framed (H.264/H.265/H.266).
/// - `TST_E_BUFFER_FULL` — TS-packet output buffer would exceed capacity.
/// - `TST_E_INVALID_CONFIG` — `nal` is null with non-zero `len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_to_with_dts(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    dts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    let dts = Pts90khz::new(dts_90khz);
    h.inner.with_inner_mut(
        |m| match m.push_video_to_with_dts(stream, slice, pts, dts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        },
    )
}

/// Push one access unit with an ST 0604 MISP timestamp SEI spliced in
/// before the first VCL NAL, targeting a specific video stream.
///
/// `misp_kind`: 0 = microsecond Precision Time Stamp (H.264 and H.265),
/// 1 = nanosecond (H.265-only per ST 0604.6 §12.2).
/// `time_status` is the MISB ST 0603 status byte; `value` is the
/// timestamp in the kind's unit.
///
/// # Errors
///
/// - `TST_E_MISP_TIME` — nano on H.264, AV1/H.266 stream, no VCL NAL,
///   or `misp_kind` out of range (not 0 or 1).
/// - plus everything `tst_muxer_push_video_to` raises.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_misp_to(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    key_frame: bool,
    misp_kind: u8,
    time_status: u8,
    value: u64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let misp = match misp_kind {
        0 => tst_core::codec::misp_time::MispTimestamp::micros(value, time_status),
        1 => tst_core::codec::misp_time::MispTimestamp::nanos(value, time_status),
        _ => {
            set_last_error(
                TstError::MispTime,
                "misp_kind must be 0 (micro) or 1 (nano)",
            );
            return TstError::MispTime as i32;
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner.with_inner_mut(
        |m| match m.push_video_misp_to(stream, slice, pts, key_frame, &misp) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        },
    )
}

/// Push one access unit with an ST 0604 MISP timestamp SEI and explicit
/// decode timestamp (DTS), targeting a specific video stream. Required for
/// codecs that emit reordered output (B-frames in H.264/H.265).
///
/// See `tst_muxer_push_video_misp_to` for the MISP parameter contract and
/// `tst_muxer_push_video_to_with_dts` for the DTS contract.
///
/// # Errors
///
/// - `TST_E_MISP_TIME` — nano on H.264, AV1/H.266 stream, no VCL NAL,
///   or `misp_kind` out of range.
/// - plus everything `tst_muxer_push_video_to_with_dts` raises.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_misp_to_with_dts(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    nal: *const u8,
    len: usize,
    pts_90khz: i64,
    dts_90khz: i64,
    key_frame: bool,
    misp_kind: u8,
    time_status: u8,
    value: u64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(nal, len, "nal") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let misp = match misp_kind {
        0 => tst_core::codec::misp_time::MispTimestamp::micros(value, time_status),
        1 => tst_core::codec::misp_time::MispTimestamp::nanos(value, time_status),
        _ => {
            set_last_error(
                TstError::MispTime,
                "misp_kind must be 0 (micro) or 1 (nano)",
            );
            return TstError::MispTime as i32;
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    let dts = Pts90khz::new(dts_90khz);
    h.inner.with_inner_mut(|m| {
        match m.push_video_misp_to_with_dts(stream, slice, pts, dts, key_frame, &misp) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        }
    })
}

/// PTS+DTS variant of `tst_muxer_push_video_wire_to` — pushes an
/// already-carried on-wire video AU (verbatim, no framing transform)
/// targeting a specific video stream, with explicit decode timestamp.
/// See `tst_muxer_push_video_wire_to` for the byte-faithful transmux
/// workflow and `tst_muxer_push_video_to_with_dts` for the DTS contract.
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — `handle` index is out of range for this muxer.
/// - `TST_E_BUFFER_FULL` — TS-packet output buffer would exceed capacity.
/// - `TST_E_INVALID_CONFIG` — `wire` is null with non-zero `len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_video_wire_to_with_dts(
    p: *mut TstMuxer,
    handle: TstVideoStreamHandle,
    wire: *const u8,
    len: usize,
    pts_90khz: i64,
    dts_90khz: i64,
    key_frame: bool,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(wire, len, "wire") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let stream = match VideoStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    let dts = Pts90khz::new(dts_90khz);
    h.inner.with_inner_mut(|m| {
        match m.push_video_wire_to_with_dts(stream, slice, pts, dts, key_frame) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        }
    })
}

/// Push one pre-built KLV blob targeting a specific KLV elementary stream.
///
/// `handle` is obtained from `tst_mux_config_add_klv_stream`. Same
/// semantics as `tst_muxer_push_video_to`.
///
/// For `KlvStreamType::SynchronousMetadata` streams, the muxer auto-wraps
/// the caller's bytes in a `Metadata_AU_cell` header per ITU-T H.222.0
/// V9 § 2.12.4.2 (5 bytes prepended; PTS surfaced in the PES header).
/// For `KlvStreamType::PrivateData` streams, the caller's bytes pass
/// through unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_klv_to(
    p: *mut TstMuxer,
    handle: TstKlvStreamHandle,
    klv: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(klv, len, "klv") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation — see VideoStreamHandle::try_from_raw rationale
    // in tst_muxer_push_video_to above.
    let stream = match KlvStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner.with_inner_mut(|m| {
        match m.push_klv_to(
            stream, slice, pts,
            // C ABI receiver-surface plan will expose metadata_service_id;
            // today defaults to 0x00 per ST 1402.2 App. B Table 2.
            0x00,
        ) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        }
    })
}

/// Push one data payload onto the muxer's single data stream.
///
/// **Pass-through contract:** the muxer applies no AU-cell wrap, no
/// framing, and no payload inspection — `data` lands verbatim as the
/// payload of exactly one PES packet on the configured PID, using
/// `stream_id` `0xBD` (`private_stream_1`). Record boundaries within
/// `data` (if any) are entirely the caller's convention.
///
/// `pts_90khz` is written into the PES header only when the stream was
/// configured with `carries_pts = true` in
/// `tst_mux_config_add_data_stream`; it is **always** used for PSI/PCR
/// pacing decisions regardless. For `carries_pts = false` streams the
/// PES omits the PTS field entirely; this library's demuxer surfaces
/// such samples with `pts == 0` (its no-PTS substitute).
///
/// Single-stream form: the muxer must have exactly one data stream
/// configured. Multi-stream callers use `tst_muxer_push_data_to` with an
/// explicit `tst_data_stream_handle_t`.
///
/// # Errors
///
/// - `TST_E_INVALID_USAGE` — no data stream configured
///   (`NoDataStreamsConfigured`).
/// - `TST_E_INVALID_USAGE` — more than one data stream configured
///   (`AmbiguousTarget` — caller must use the `_to` variant).
/// - `TST_E_INVALID_USAGE` — payload exceeds the `PES_packet_length`
///   ceiling (`DataTooLarge`: 65532 bytes without PTS, 65527 with).
/// - `TST_E_INVALID_CONFIG` — `data` is null with non-zero `len`.
///
/// # C ABI
///
/// `tst_muxer_push_data` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_data(
    p: *mut TstMuxer,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_mut(|m| match m.push_data(slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one data payload targeting a specific data elementary stream.
///
/// `handle` is obtained from `tst_mux_config_add_data_stream`. Same
/// semantics as `tst_muxer_push_video_to`; payload, PTS, and size-ceiling
/// contracts are those of `tst_muxer_push_data` (the single-stream form).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_data_to(
    p: *mut TstMuxer,
    handle: TstDataStreamHandle,
    data: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(data, len, "data") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation — see VideoStreamHandle::try_from_raw rationale
    // in tst_muxer_push_video_to above.
    let stream = match DataStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner
        .with_inner_mut(|m| match m.push_data_to(stream, slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one audio frame buffer (single-stream shorthand).
///
/// Resolves only when exactly one audio stream is configured across all
/// programs. Otherwise rejects with `TST_E_INVALID_USAGE` (carrying
/// `MuxError::AmbiguousTarget` or `MuxError::NoAudioStreamsConfigured`).
///
/// `frames` is one or more pre-framed audio frames concatenated by the
/// caller (e.g. one ADTS frame for AAC, one MP2 frame, one AC-3 frame).
/// PTS is required — audio always carries PTS.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_audio(
    p: *mut TstMuxer,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_mut(|m| match m.push_audio(slice, pts) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one audio frame buffer targeting a specific audio elementary stream.
///
/// `handle` is obtained from `tst_mux_config_add_audio_stream` /
/// `tst_mux_config_add_audio_stream_with_language` at config time and is
/// stable across the config→open boundary. Out-of-range handles surface
/// as `TST_E_INVALID_USAGE` (carrying `MuxError::InvalidStreamHandle`).
///
/// On a single-stream muxer, prefer `tst_muxer_push_audio` — it has the
/// same effect and doesn't require a handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_audio_to(
    p: *mut TstMuxer,
    handle: TstAudioStreamHandle,
    frames: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(frames, len, "frames") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation — see VideoStreamHandle::try_from_raw rationale
    // in tst_muxer_push_video_to above.
    let stream = match AudioStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner
        .with_inner_mut(|m| match m.push_audio_to(stream, pts, slice) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one subtitle PES unit (single-stream shorthand).
///
/// Resolves only when exactly one subtitle stream is configured.
/// Otherwise rejects with `TST_E_INVALID_USAGE` (carrying
/// `MuxError::AmbiguousTarget` or `MuxError::NoSubtitleStreamsConfigured`).
///
/// `payload` is one complete logical subtitle unit (DVB-sub composition
/// page, teletext data field, CEA-708 service block, or WebVTT cue);
/// fragmentation across PES is not used. PTS is required — subtitles
/// are rendered at presentation time and never reordered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_subtitle(
    p: *mut TstMuxer,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let pts = Pts90khz::new(pts_90khz);
    handle
        .inner
        .with_inner_mut(|m| match m.push_subtitle(pts, slice) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Push one subtitle PES unit targeting a specific subtitle elementary stream.
///
/// `handle` is obtained from one of the four
/// `tst_mux_config_add_subtitle_stream_*` constructors. Out-of-range
/// handles surface as `TST_E_INVALID_USAGE` (carrying
/// `MuxError::InvalidStreamHandle`).
///
/// On a single-stream muxer, prefer `tst_muxer_push_subtitle` — same
/// effect, no handle required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_push_subtitle_to(
    p: *mut TstMuxer,
    handle: TstSubtitleStreamHandle,
    payload: *const u8,
    len: usize,
    pts_90khz: i64,
) -> crate::c_types::c_int {
    let Some(h) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    let slice = match unsafe { crate::ffi_slice::ffi_slice(payload, len, "payload") } {
        Ok(s) => s,
        Err(code) => return code,
    };
    // Trust-boundary validation — see VideoStreamHandle::try_from_raw rationale
    // in tst_muxer_push_video_to above.
    let stream = match SubtitleStreamHandle::try_from_raw(handle) {
        Ok(h) => h,
        Err(e) => {
            record_mux_error(&e);
            return unsafe { crate::error::tst_get_last_error() };
        }
    };
    let pts = Pts90khz::new(pts_90khz);
    h.inner
        .with_inner_mut(|m| match m.push_subtitle_to(stream, pts, slice) {
            Ok(()) => 0,
            Err(e) => {
                record_mux_error(&e);
                unsafe { crate::error::tst_get_last_error() }
            }
        })
}

/// Drain TS bytes into `out_buf` (capacity `out_cap`). Returns the number
/// of bytes written; 0 means nothing was ready or the buffer was too
/// small for the next chunk. Returns 0 on invalid buffer arguments (null
/// pointer, zero capacity, or capacity exceeding `isize::MAX`); the
/// absurd-length case additionally records a last-error for diagnosis.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_pull(
    p: *mut TstMuxer,
    out_buf: *mut u8,
    out_cap: usize,
) -> usize {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    if out_buf.is_null() || out_cap == 0 {
        return 0;
    }
    let slice = match unsafe { crate::ffi_slice::ffi_slice_mut(out_buf, out_cap, "out_buf") } {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let mut n = 0usize;
    let _rc = handle.inner.with_inner_mut(|m| {
        n = m.pull(slice);
        0
    });
    n
}

/// Snapshot stats for a `tst_muxer_t` into `*out`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if either pointer is
/// null, or `TST_E_CLOSED` if the muxer has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_get_stats(
    p: *mut TstMuxer,
    out: *mut crate::stats::TstMuxerStats,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    handle.inner.with_inner_ref(|m| {
        let stats = m.stats();
        let mut per_stream =
            [crate::stats::TstStreamStats::default(); crate::stats::TST_STATS_MAX_STREAMS];
        let (per_stream_count, truncated) =
            crate::stats::fill_per_stream(&mut per_stream, &stats.per_stream);
        let dst = crate::stats::TstMuxerStats {
            ts_packets_emitted: stats.ts_packets_emitted,
            ts_bytes_emitted: stats.ts_bytes_emitted,
            programs_configured: stats.programs_configured,
            per_stream_count,
            per_stream_truncated: if truncated { 1 } else { 0 },
            per_stream,
        };
        unsafe { *out = dst };
        0
    })
}

/// Snapshot codec-specific stats for one PID on a `tst_muxer_t` into `*out`.
///
/// The returned struct is a tagged union — read `out->kind` first, then
/// the matching `out->u.<arm>` field. See `tst_stream_codec_stats_t` in
/// `tstrans.h` for the discriminator constants (`TST_CODEC_KIND_*`).
///
/// # Errors
///
/// * `TST_E_INVALID_CONFIG` — `p` or `out` is null
/// * `TST_E_CLOSED` — handle was closed via `tst_muxer_close`
/// * `TST_E_NOT_FOUND` — `pid` has never been observed on this handle
/// * `TST_E_INTERNAL` — internal panic caught at the FFI boundary
///
/// # Safety
///
/// `p` must be a valid pointer obtained from `tst_muxer_open`; `out`
/// must be a writable `tst_stream_codec_stats_t` of size at least
/// `sizeof(tst_stream_codec_stats_t)`. The pointee is fully written on
/// `TST_OK` and untouched on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_get_stream_codec_stats(
    p: *mut TstMuxer,
    pid: u16,
    out: *mut crate::stats::TstStreamCodecStats,
) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    if out.is_null() {
        set_last_error(TstError::InvalidConfig, "null out pointer");
        return TstError::InvalidConfig as i32;
    }
    handle
        .inner
        .with_inner_ref(|m| match m.stream_codec_stats(pid) {
            Some(stats) => {
                unsafe { *out = crate::stats::codec_stats_to_c(stats) };
                0
            }
            None => record_not_found(&format!(
                "codec stats not available for pid 0x{pid:04x} (pid has never been observed on this muxer)"
            )),
        })
}

/// Reset stats counters for a `tst_muxer_t` to zero.
///
/// Per-stream entries are preserved (identity fields remain); only flow
/// counters (`items`, `bytes`, `discontinuities`) are zeroed.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if the pointer is
/// null, or `TST_E_CLOSED` if the muxer has been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_reset_stats(p: *mut TstMuxer) -> crate::c_types::c_int {
    let Some(handle) = (unsafe { p.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null muxer pointer");
        return TstError::InvalidConfig as i32;
    };
    handle.inner.with_inner_mut(|m| {
        m.reset_stats();
        0
    })
}

/// Close and free the muxer.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_muxer_close(p: *mut TstMuxer) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.close();
        drop(boxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use alloc::vec;

    #[test]
    fn open_with_default_config_succeeds() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
            let m = tst_muxer_open(cfg);
            assert!(!m.is_null());
            tst_muxer_close(m);
            tst_mux_config_free(cfg);
        }
    }

    #[test]
    fn push_then_pull_emits_bytes() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            // Single-stream muxer: push_video (no handle) is unambiguous.
            let hv = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
            let m = tst_muxer_open(cfg);
            tst_mux_config_free(cfg);

            // Annex-B IDR NAL.
            let nal: [u8; 9] = [0, 0, 0, 1, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];
            let rc = tst_muxer_push_video_to(m, hv, nal.as_ptr(), nal.len(), 0, true);
            assert_eq!(rc, 0);

            let mut buf = vec![0u8; 4096];
            let n = tst_muxer_pull(m, buf.as_mut_ptr(), buf.len());
            assert!(n > 0, "expected TS bytes to be pulled");
            assert_eq!(buf[0], 0x47, "first byte of TS packet must be 0x47");

            tst_muxer_close(m);
        }
    }

    #[test]
    fn push_video_with_invalid_nal_returns_invalid_nal() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            tst_mux_config_add_klv_stream(cfg, prog, 0x1031, TstKlvStreamType::PrivateData, false);
            let m = tst_muxer_open(cfg);
            tst_mux_config_free(cfg);

            let bad = [0xAB, 0xCD];
            let rc = tst_muxer_push_video(m, bad.as_ptr(), bad.len(), 0, false);
            assert_eq!(rc, TstError::InvalidNal as i32);

            tst_muxer_close(m);
        }
    }

    #[test]
    fn null_pointer_close_is_safe() {
        unsafe { tst_muxer_close(core::ptr::null_mut()) };
    }
}
