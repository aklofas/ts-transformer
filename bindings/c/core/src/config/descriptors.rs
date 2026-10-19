//! PMT descriptor configuration entry points.
//!
//! Houses the program- and per-stream descriptor setters/adders. The
//! private helpers:
//!
//! - `resolve_stream_descriptor_slot` — shared resolver for all 8 per-stream
//!   descriptor fns (5 `add_*` + 3 `set_stream_descriptors_for_*`). Routes
//!   **both** families through the `try_from_raw` trust-boundary guard.
//! - `DescriptorStream`, `resolve`, `add_descriptor`, `set_descriptors` — the
//!   per-kind part of those 8 entry points (handle unpacking, `StreamSpec`
//!   variant, error texts) as a trait, and the two bodies generic over it.
//! - `desc_to_tlv_blob` — single-descriptor copy.
//! - `parse_tlv_list` — multi-descriptor byte-stream parse. The
//!   `with_capacity` reservation is clamped to `tlv_count.min(tlv_total_len
//!   / 2 + 1)` to prevent a caller-controlled `tlv_count = 2e9` from
//!   triggering an uncatchable `handle_alloc_error` abort.

use super::{TstMuxConfig, TstProgramHandle};
use crate::error::{TstError, set_last_error};
use crate::handle::{
    TstAudioStreamHandle, TstDataStreamHandle, TstKlvStreamHandle, TstSubtitleStreamHandle,
    TstVideoStreamHandle,
};
use crate::panic::ffi_catch;
use alloc::vec::Vec;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, StreamSpec, SubtitleStreamHandle,
    VideoStreamHandle,
};

// ---------------------------------------------------------------------------
// Shared slot resolver
// ---------------------------------------------------------------------------

/// Resolve a per-stream descriptor slot, shared by all 8 per-stream
/// descriptor entry points (5 `add_*` + 3 `set_stream_descriptors_for_*`).
///
/// `raw` is the packed `u32` stream handle received from the C caller.
///
/// `try_unpack` must call the kind-specific `*StreamHandle::try_from_raw(raw)`
/// and then `h.unpack()`, returning `Ok((prog_idx, within_idx))` on a
/// canonical handle or `Err(error_code)` (with the last-error already set)
/// on a forged one. Routing both the `add_*` and `set_*` families through
/// this path closes the bypass that let a raw bit-twiddled handle escape
/// the anti-forgery check in the `add_*` family.
///
/// `kind_filter` selects the matching `StreamSpec` variant.
///
/// `prog_oor_msg` / `stream_oor_msg` are the static error strings emitted
/// when the programme or within-kind index is out of range.
///
/// Returns `Ok((prog_idx, stream_idx))` where `stream_idx` is the index
/// into both `prog.streams` and `prog.stream_descriptors`.
fn resolve_stream_descriptor_slot<U, F>(
    cfg: &TstMuxConfig,
    raw: u32,
    try_unpack: U,
    kind_filter: F,
    prog_oor_msg: &str,
    stream_oor_msg: &str,
) -> Result<(usize, usize), i32>
where
    U: FnOnce(u32) -> Result<(usize, usize), i32>,
    F: Fn(&StreamSpec) -> bool,
{
    let (prog_idx, within_idx) = try_unpack(raw)?;
    if prog_idx >= cfg.programs.len() {
        set_last_error(TstError::InvalidUsage, prog_oor_msg);
        return Err(TstError::InvalidUsage as i32);
    }
    let prog = &cfg.programs[prog_idx];
    match prog
        .streams
        .iter()
        .enumerate()
        .filter(|(_, s)| kind_filter(s))
        .nth(within_idx)
        .map(|(i, _)| i)
    {
        Some(stream_idx) => Ok((prog_idx, stream_idx)),
        None => {
            set_last_error(TstError::InvalidUsage, stream_oor_msg);
            Err(TstError::InvalidUsage as i32)
        }
    }
}

/// What the eight per-stream descriptor entry points differ in: the handle
/// kind's unpacking, its `StreamSpec` variant, and its three error texts.
trait DescriptorStream {
    const NON_CANONICAL: &'static str;
    const PROGRAM_OOR: &'static str;
    const STREAM_OOR: &'static str;
    fn unpack_raw(raw: u32) -> Option<(usize, usize)>;
    fn is_kind(spec: &StreamSpec) -> bool;
}

macro_rules! descriptor_stream {
    ($handle:ty, $variant:ident, $name:literal) => {
        impl DescriptorStream for $handle {
            const NON_CANONICAL: &'static str = concat!(
                "invalid ",
                $name,
                " stream handle (non-canonical raw bits set)"
            );
            const PROGRAM_OOR: &'static str =
                concat!("invalid ", $name, " stream handle (program out of range)");
            const STREAM_OOR: &'static str =
                concat!("invalid ", $name, " stream handle (stream out of range)");
            fn unpack_raw(raw: u32) -> Option<(usize, usize)> {
                <$handle>::try_from_raw(raw).ok().map(|h| h.unpack())
            }
            fn is_kind(spec: &StreamSpec) -> bool {
                matches!(spec, StreamSpec::$variant { .. })
            }
        }
    };
}

descriptor_stream!(VideoStreamHandle, Video, "video");
descriptor_stream!(KlvStreamHandle, Klv, "klv");
descriptor_stream!(AudioStreamHandle, Audio, "audio");
descriptor_stream!(SubtitleStreamHandle, Subtitle, "subtitle");
descriptor_stream!(DataStreamHandle, Data, "data");

/// Resolve `raw` to `(program, stream)` through the trust-boundary check:
/// `try_from_raw` rejects handles with bits set above the canonical packed
/// layout. A forged `valid.raw() | 0x100` would otherwise mask to the same
/// low byte and write descriptors to the wrong elementary stream.
fn resolve<K: DescriptorStream>(cfg: &TstMuxConfig, raw: u32) -> Result<(usize, usize), i32> {
    resolve_stream_descriptor_slot(
        cfg,
        raw,
        |raw| {
            K::unpack_raw(raw).ok_or_else(|| {
                set_last_error(TstError::InvalidUsage, K::NON_CANONICAL);
                TstError::InvalidUsage as i32
            })
        },
        K::is_kind,
        K::PROGRAM_OOR,
        K::STREAM_OOR,
    )
}

/// Body of the five `tst_mux_config_add_<kind>_descriptor` entry points
/// (each wraps it in `ffi_catch`).
///
/// # Safety
/// `cfg` is NULL or a live `TstMuxConfig`; `desc` is NULL or a live
/// `TstDescriptor` whose `data` is readable for `data_len` bytes.
unsafe fn add_descriptor<K: DescriptorStream>(
    cfg: *mut TstMuxConfig,
    raw: u32,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    let Some(cfg) = (unsafe { cfg.as_mut() }) else {
        set_last_error(TstError::InvalidConfig, "null config pointer");
        return TstError::InvalidConfig as i32;
    };
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        set_last_error(TstError::InvalidConfig, "null descriptor pointer");
        return TstError::InvalidConfig as i32;
    };
    let tlv = match unsafe { desc_to_tlv_blob(desc) } {
        Ok(b) => b,
        Err(rc) => return rc,
    };
    let (prog_idx, stream_idx) = match resolve::<K>(cfg, raw) {
        Ok(v) => v,
        Err(rc) => return rc,
    };
    cfg.programs[prog_idx].stream_descriptors[stream_idx].push(tlv);
    0
}

/// Body of the three `tst_mux_config_set_stream_descriptors_for_<kind>`
/// entry points (each wraps it in `ffi_catch`).
///
/// # Safety
/// `cfg` is NULL or a live `TstMuxConfig`; `tlv_bytes` is readable for
/// `tlv_total_len` bytes when non-null.
unsafe fn set_descriptors<K: DescriptorStream>(
    cfg: *mut TstMuxConfig,
    raw: u32,
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> crate::c_types::c_int {
    let Some(cfg) = (unsafe { cfg.as_mut() }) else {
        set_last_error(TstError::InvalidConfig, "null config pointer");
        return TstError::InvalidConfig as i32;
    };
    let (prog_idx, stream_idx) = match resolve::<K>(cfg, raw) {
        Ok(v) => v,
        Err(rc) => return rc,
    };
    let descs = match unsafe { parse_tlv_list(tlv_bytes, tlv_total_len, tlv_count) } {
        Ok(d) => d,
        Err(rc) => return rc,
    };
    cfg.programs[prog_idx].stream_descriptors[stream_idx] = descs;
    0
}

// ---------------------------------------------------------------------------
// Program-level descriptor setter
// ---------------------------------------------------------------------------

/// Set program-level PMT descriptors for the specified program.
///
/// `tlv_bytes` points to the concatenation of `tlv_count` TLV triples,
/// totalling `tlv_total_len` bytes. Each TLV has the layout:
///   byte 0: tag
///   byte 1: length of body (N)
///   bytes 2..2+N: body
///
/// Calling with `tlv_total_len == 0` or `tlv_count == 0` clears any
/// previously set program descriptors for this program.
///
/// Returns 0 on success or a negative `TST_E_*` code on: null `cfg`,
/// null `tlv_bytes` with non-zero count, invalid program handle, or a
/// malformed TLV byte stream (truncated length or body).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_program_descriptors(
    cfg: *mut TstMuxConfig,
    program: TstProgramHandle,
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        let prog_idx = program.0 as usize;
        if prog_idx >= cfg.programs.len() {
            set_last_error(TstError::InvalidUsage, "invalid program handle");
            return TstError::InvalidUsage as i32;
        }
        if tlv_total_len == 0 || tlv_count == 0 {
            cfg.programs[prog_idx].program_descriptors = Vec::new();
            return 0;
        }
        let descs = unsafe {
            match parse_tlv_list(tlv_bytes, tlv_total_len, tlv_count) {
                Ok(d) => d,
                Err(rc) => return rc,
            }
        };
        cfg.programs[prog_idx].program_descriptors = descs;
        0
    })
}

// ---------------------------------------------------------------------------
// Per-stream descriptor setters (set_stream_descriptors_for_*)
// ---------------------------------------------------------------------------

/// Set per-stream PMT descriptors for the specified video stream.
///
/// `video` is a handle previously returned by `tst_mux_config_add_video_stream`.
/// The TLV byte format is the same as `tst_mux_config_set_program_descriptors`.
///
/// Calling with `tlv_total_len == 0` or `tlv_count == 0` clears any
/// previously set stream descriptors for this stream.
///
/// Returns 0 on success or a negative `TST_E_*` code on: null `cfg`,
/// invalid handle, null `tlv_bytes` with non-zero count, or malformed TLV.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_stream_descriptors_for_video(
    cfg: *mut TstMuxConfig,
    video: TstVideoStreamHandle,
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        set_descriptors::<VideoStreamHandle>(cfg, video, tlv_bytes, tlv_total_len, tlv_count)
    })
}

/// Set per-stream PMT descriptors for the specified KLV stream.
///
/// `klv` is a handle previously returned by `tst_mux_config_add_klv_stream`.
/// The TLV byte format is the same as `tst_mux_config_set_program_descriptors`.
///
/// Returns 0 on success or a negative `TST_E_*` code on the same conditions
/// as `tst_mux_config_set_stream_descriptors_for_video`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_stream_descriptors_for_klv(
    cfg: *mut TstMuxConfig,
    klv: TstKlvStreamHandle,
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        set_descriptors::<KlvStreamHandle>(cfg, klv, tlv_bytes, tlv_total_len, tlv_count)
    })
}

/// Set per-stream PMT descriptors for the specified data stream.
///
/// `data` is a handle previously returned by `tst_mux_config_add_data_stream`.
/// The TLV byte format is the same as `tst_mux_config_set_program_descriptors`.
///
/// Returns 0 on success or a negative `TST_E_*` code on the same conditions
/// as `tst_mux_config_set_stream_descriptors_for_video`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_set_stream_descriptors_for_data(
    cfg: *mut TstMuxConfig,
    data: TstDataStreamHandle,
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        set_descriptors::<DataStreamHandle>(cfg, data, tlv_bytes, tlv_total_len, tlv_count)
    })
}

// ---------------------------------------------------------------------------
// Internal helper: single-descriptor copy
// ---------------------------------------------------------------------------

// Internal helper: copy one `TstDescriptor` into a full TLV blob
// `[tag, body_len, body...]` ready to push onto a `stream_descriptors[i]` slot.
//
// Returns `Err(TST_E_*)` on null data with non-zero length or body > 255 bytes.
//
// # Safety
// Caller must validate that `desc` is non-null before calling.
unsafe fn desc_to_tlv_blob(
    desc: &crate::event::TstDescriptor,
) -> Result<Vec<u8>, crate::c_types::c_int> {
    if desc.data_len > 255 {
        set_last_error(
            TstError::InvalidConfig,
            "descriptor body length exceeds 255 (MPEG-TS descriptor limit)",
        );
        return Err(TstError::InvalidConfig as i32);
    }
    if desc.data.is_null() && desc.data_len != 0 {
        set_last_error(
            TstError::InvalidConfig,
            "descriptor data pointer is null with non-zero data_len",
        );
        return Err(TstError::InvalidConfig as i32);
    }
    let body = unsafe { crate::ffi_slice::ffi_slice(desc.data, desc.data_len, "data") }?;
    let mut tlv = Vec::with_capacity(2 + body.len());
    tlv.push(desc.tag);
    tlv.push(desc.data_len as u8);
    tlv.extend_from_slice(body);
    Ok(tlv)
}

// ---------------------------------------------------------------------------
// Per-stream descriptor adders (add_*_descriptor)
// ---------------------------------------------------------------------------

/// Append one PMT descriptor to a video stream's per-PID descriptor list.
///
/// `stream` is the handle returned by `tst_mux_config_add_video_stream`.
/// `desc` must be non-null with `desc.data` pointing to `desc.data_len`
/// bytes (stripped length — does not include the tag/length header bytes).
/// Bytes are copied; the caller's buffer is not retained after this call.
/// Multiple calls accumulate; descriptors appear in the PMT in add-order.
///
/// Returns 0 on success, or a negative `TST_E_*` code on: null `cfg` or
/// `desc`, stale handle, null `desc.data` with non-zero `desc.data_len`,
/// or `desc.data_len > 255` (MPEG-TS descriptor body limit).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_video_descriptor(
    cfg: *mut TstMuxConfig,
    stream: TstVideoStreamHandle,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        add_descriptor::<VideoStreamHandle>(cfg, stream, desc)
    })
}

/// Append one PMT descriptor to a KLV stream's per-PID descriptor list.
/// Same contract as `tst_mux_config_add_video_descriptor`.
///
/// `stream` is the handle returned by `tst_mux_config_add_klv_stream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_klv_descriptor(
    cfg: *mut TstMuxConfig,
    stream: TstKlvStreamHandle,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        add_descriptor::<KlvStreamHandle>(cfg, stream, desc)
    })
}

/// Append one PMT descriptor to an audio stream's per-PID descriptor list.
/// Same contract as `tst_mux_config_add_video_descriptor`.
///
/// `stream` is the handle returned by `tst_mux_config_add_audio_stream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_audio_descriptor(
    cfg: *mut TstMuxConfig,
    stream: TstAudioStreamHandle,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        add_descriptor::<AudioStreamHandle>(cfg, stream, desc)
    })
}

/// Append one PMT descriptor to a subtitle stream's per-PID descriptor list.
/// Same contract as `tst_mux_config_add_video_descriptor`.
///
/// `stream` is the handle returned by one of
/// `tst_mux_config_add_subtitle_stream_dvb_subtitling`,
/// `tst_mux_config_add_subtitle_stream_dvb_teletext`,
/// `tst_mux_config_add_subtitle_stream_cea708`, or
/// `tst_mux_config_add_subtitle_stream_webvtt`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_subtitle_descriptor(
    cfg: *mut TstMuxConfig,
    stream: TstSubtitleStreamHandle,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        add_descriptor::<SubtitleStreamHandle>(cfg, stream, desc)
    })
}

/// Append one PMT descriptor to a data stream's per-PID descriptor list.
/// Same contract as `tst_mux_config_add_video_descriptor`.
///
/// `stream` is the handle returned by `tst_mux_config_add_data_stream`.
/// Note the muxer never auto-emits a descriptor on a data stream, and the
/// accumulated `(stream_type, descriptors)` pair must still classify as
/// Unknown under the demux cascade — enforced at `_open` time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_mux_config_add_data_descriptor(
    cfg: *mut TstMuxConfig,
    stream: TstDataStreamHandle,
    desc: *const crate::event::TstDescriptor,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || unsafe {
        add_descriptor::<DataStreamHandle>(cfg, stream, desc)
    })
}

// ---------------------------------------------------------------------------
// Internal helper: TLV byte-stream parser (with_capacity clamp)
// ---------------------------------------------------------------------------

// Internal helper: parse a concatenated TLV byte stream (tag + length + body
// per descriptor) into a Vec<Vec<u8>>. Returns Err(TST_E_*) on malformed input.
//
// `with_capacity` is clamped to `tlv_count.min(tlv_total_len / 2 + 1)`.
// Each TLV occupies at least 2 bytes (tag + length), so this is a tight upper
// bound on the number of TLVs that can fit in `tlv_total_len` bytes. Without
// the clamp, a caller-supplied `tlv_count = 2_000_000_000` with a 4-byte
// `tlv_total_len` would trigger an uncatchable `handle_alloc_error` abort via
// a ~48 GB `Vec::with_capacity`.
//
// # Safety
// Caller must ensure `tlv_bytes` is valid for `tlv_total_len` bytes when
// `tlv_total_len > 0` and `tlv_count > 0`.
unsafe fn parse_tlv_list(
    tlv_bytes: *const u8,
    tlv_total_len: usize,
    tlv_count: usize,
) -> Result<Vec<Vec<u8>>, crate::c_types::c_int> {
    if tlv_total_len == 0 || tlv_count == 0 {
        return Ok(Vec::new());
    }
    if tlv_bytes.is_null() {
        set_last_error(
            TstError::InvalidUsage,
            "null tlv_bytes pointer with non-zero count",
        );
        return Err(TstError::InvalidUsage as i32);
    }
    let bytes = unsafe { crate::ffi_slice::ffi_slice(tlv_bytes, tlv_total_len, "tlv_bytes") }?;
    // Clamp capacity: each TLV is at minimum 2 bytes (tag + body-length).
    // `tlv_count.min(tlv_total_len / 2 + 1)` bounds the reservation to a
    // value proportional to the actual byte budget, preventing caller-controlled
    // reservation exhaustion via an absurd `tlv_count`.
    let capacity = tlv_count.min(tlv_total_len / 2 + 1);
    let mut descs = Vec::with_capacity(capacity);
    let mut offset = 0usize;
    for _ in 0..tlv_count {
        if offset + 2 > bytes.len() {
            set_last_error(
                TstError::InvalidConfig,
                "TLV byte stream truncated (no tag+length)",
            );
            return Err(TstError::InvalidConfig as i32);
        }
        let body_len = bytes[offset + 1] as usize;
        let total = 2 + body_len;
        if offset + total > bytes.len() {
            set_last_error(
                TstError::InvalidConfig,
                "TLV byte stream truncated (body too short)",
            );
            return Err(TstError::InvalidConfig as i32);
        }
        descs.push(bytes[offset..offset + total].to_vec());
        offset += total;
    }
    Ok(descs)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use crate::event::TstDescriptor;
    use crate::test_support::null_entry_tests;

    null_entry_tests! {
        /// Null config pointer → `TST_E_INVALID_CONFIG`.
        null_cfg_returns_invalid_config => invalid_config {
            let desc = TstDescriptor {
                tag: 0x02,
                _reserved: [0; 7],
                data: core::ptr::null(),
                data_len: 0,
            };
            tst_mux_config_add_video_descriptor(core::ptr::null_mut(), 0, &desc as *const _)
        };
    }

    /// Helper: a zero-body TLV (2 bytes: tag + zero body_len).
    fn zero_body_tlv(tag: u8) -> [u8; 2] {
        [tag, 0x00]
    }

    // -----------------------------------------------------------------------
    // with_capacity clamp
    // -----------------------------------------------------------------------

    /// Confirms that a caller-supplied `tlv_count = 2_000_000_000` paired with
    /// a tiny `tlv_total_len` returns an error code rather than aborting the
    /// process via an uncatchable `handle_alloc_error`.
    ///
    /// Unclamped, `Vec::with_capacity(2_000_000_000)` would attempt a ~48 GB
    /// reservation and abort. The capacity is clamped to
    /// `min(2e9, tlv_total_len / 2 + 1)` — a small number — and the subsequent
    /// loop immediately detects the TLV count / byte-stream mismatch and returns
    /// `TST_E_INVALID_CONFIG`.
    #[test]
    fn absurd_tlv_count_tiny_total_len_returns_error_not_abort() {
        let byte = zero_body_tlv(0xAB); // one valid 2-byte TLV
        // tlv_count = 2e9, tlv_total_len = 2: only one TLV fits.
        // with_capacity → min(2_000_000_000, 2/2+1=2) = 2 (safe).
        // Loop iteration 2 will try offset=2, offset+2=4 > 2 → error.
        let rc = unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let result = tst_mux_config_set_program_descriptors(
                cfg,
                prog,
                byte.as_ptr(),
                2,             // tlv_total_len = 2
                2_000_000_000, // tlv_count = absurd
            );
            tst_mux_config_free(cfg);
            result
        };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    // -----------------------------------------------------------------------
    // forged handle rejection (add- and set-family parity)
    // -----------------------------------------------------------------------

    /// Confirms that both the `add_*` and `set_stream_descriptors_for_*`
    /// families reject a forged video stream handle (high bits set) with
    /// `TST_E_INVALID_USAGE`; raw bit-twiddling in the `add_*` family would
    /// silently mask the high bits instead.
    #[test]
    fn forged_video_handle_rejected_by_add_and_set_families() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let real_handle =
                tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            // Forge a handle by setting a high bit that try_from_raw rejects.
            let forged = real_handle | 0x100;

            // add-family
            let null_desc = TstDescriptor {
                tag: 0x02,
                _reserved: [0; 7],
                data: core::ptr::null(),
                data_len: 0,
            };
            let rc_add = tst_mux_config_add_video_descriptor(cfg, forged, &null_desc as *const _);
            assert_eq!(
                rc_add,
                TstError::InvalidUsage as i32,
                "add-family must reject forged handle"
            );

            // set-family
            let byte = zero_body_tlv(0x28);
            let rc_set =
                tst_mux_config_set_stream_descriptors_for_video(cfg, forged, byte.as_ptr(), 2, 1);
            assert_eq!(
                rc_set,
                TstError::InvalidUsage as i32,
                "set-family must reject forged handle"
            );

            tst_mux_config_free(cfg);
        }
    }

    /// Sanity check: a valid (canonical) handle succeeds for the add-family.
    #[test]
    fn valid_handle_add_video_descriptor_succeeds() {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let handle = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            let body = [0xDE, 0xAD];
            let desc = TstDescriptor {
                tag: 0x52,
                _reserved: [0; 7],
                data: body.as_ptr(),
                data_len: 2,
            };
            let rc = tst_mux_config_add_video_descriptor(cfg, handle, &desc as *const _);
            assert_eq!(rc, 0, "valid add-video-descriptor must return 0");
            tst_mux_config_free(cfg);
        }
    }

    /// Every per-stream entry point names its kind in each of its three
    /// handle errors: a forged handle, a program out of range, a stream out
    /// of range.
    #[test]
    fn per_stream_descriptor_errors_name_their_kind() {
        fn last_msg() -> alloc::string::String {
            unsafe { core::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) }
                .to_string_lossy()
                .into_owned()
        }
        const NO_BODY: TstDescriptor = TstDescriptor {
            tag: 0x02,
            _reserved: [0; 7],
            data: core::ptr::null(),
            data_len: 0,
        };
        type Entry = fn(*mut TstMuxConfig, u32) -> i32;
        let entries: [(&str, Entry); 8] = [
            ("video", |c, h| unsafe {
                tst_mux_config_add_video_descriptor(c, h, &NO_BODY)
            }),
            ("klv", |c, h| unsafe {
                tst_mux_config_add_klv_descriptor(c, h, &NO_BODY)
            }),
            ("audio", |c, h| unsafe {
                tst_mux_config_add_audio_descriptor(c, h, &NO_BODY)
            }),
            ("subtitle", |c, h| unsafe {
                tst_mux_config_add_subtitle_descriptor(c, h, &NO_BODY)
            }),
            ("data", |c, h| unsafe {
                tst_mux_config_add_data_descriptor(c, h, &NO_BODY)
            }),
            ("video", |c, h| unsafe {
                tst_mux_config_set_stream_descriptors_for_video(c, h, [0x28u8, 0].as_ptr(), 2, 1)
            }),
            ("klv", |c, h| unsafe {
                tst_mux_config_set_stream_descriptors_for_klv(c, h, [0x28u8, 0].as_ptr(), 2, 1)
            }),
            ("data", |c, h| unsafe {
                tst_mux_config_set_stream_descriptors_for_data(c, h, [0x28u8, 0].as_ptr(), 2, 1)
            }),
        ];
        let usage = TstError::InvalidUsage as i32;
        unsafe {
            let cfg = tst_mux_config_new();
            let _ = tst_mux_config_add_program(cfg, 1, 0x1000); // one program, no streams
            for (kind, entry) in entries {
                assert_eq!(entry(cfg, 0x100), usage, "{kind}");
                assert_eq!(
                    last_msg(),
                    alloc::format!("invalid {kind} stream handle (non-canonical raw bits set)")
                );
                assert_eq!(entry(cfg, 0xFF), usage, "{kind}");
                assert_eq!(
                    last_msg(),
                    alloc::format!("invalid {kind} stream handle (program out of range)")
                );
                assert_eq!(entry(cfg, 0x00), usage, "{kind}");
                assert_eq!(
                    last_msg(),
                    alloc::format!("invalid {kind} stream handle (stream out of range)")
                );
            }
            tst_mux_config_free(cfg);
        }
    }

    /// Valid handles: each of the eight entry points writes its TLV bytes
    /// into its own stream's descriptor list and into no other.
    #[test]
    fn per_stream_descriptor_entry_points_write_their_stream_slot() {
        use crate::config::streams::{
            tst_mux_config_add_audio_stream, tst_mux_config_add_data_stream,
            tst_mux_config_add_klv_stream, tst_mux_config_add_subtitle_stream_webvtt,
        };
        use crate::config::{TstAudioCodec, TstKlvStreamType};
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let v = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264);
            let k = tst_mux_config_add_klv_stream(
                cfg,
                prog,
                0x101,
                TstKlvStreamType::PrivateData,
                false,
            );
            let a = tst_mux_config_add_audio_stream(cfg, prog, 0x102, TstAudioCodec::Aac);
            let s = tst_mux_config_add_subtitle_stream_webvtt(cfg, prog, 0x103);
            let d = tst_mux_config_add_data_stream(cfg, prog, 0x104, 0xF0, true);
            let body = [0xAAu8, 0xBB];
            let desc = |tag| TstDescriptor {
                tag,
                _reserved: [0; 7],
                data: body.as_ptr(),
                data_len: 2,
            };
            assert_eq!(tst_mux_config_add_video_descriptor(cfg, v, &desc(0x11)), 0);
            assert_eq!(tst_mux_config_add_klv_descriptor(cfg, k, &desc(0x12)), 0);
            assert_eq!(tst_mux_config_add_audio_descriptor(cfg, a, &desc(0x13)), 0);
            assert_eq!(
                tst_mux_config_add_subtitle_descriptor(cfg, s, &desc(0x14)),
                0
            );
            assert_eq!(tst_mux_config_add_data_descriptor(cfg, d, &desc(0x15)), 0);
            // A second add accumulates; the set family replaces.
            assert_eq!(tst_mux_config_add_video_descriptor(cfg, v, &desc(0x16)), 0);
            let tlv = [0x28u8, 0x01, 0x77];
            assert_eq!(
                tst_mux_config_set_stream_descriptors_for_klv(cfg, k, tlv.as_ptr(), 3, 1),
                0
            );
            assert_eq!(
                tst_mux_config_set_stream_descriptors_for_data(cfg, d, tlv.as_ptr(), 3, 1),
                0
            );
            let two = [0x30u8, 0x00, 0x31, 0x01, 0x09];
            assert_eq!(
                tst_mux_config_set_stream_descriptors_for_video(cfg, v, two.as_ptr(), 5, 2),
                0
            );
            let got = &(&*cfg).programs[0].stream_descriptors;
            let b = |tag: u8| alloc::vec![tag, 2, 0xAA, 0xBB];
            assert_eq!(got.len(), 5);
            assert_eq!(
                got[0],
                alloc::vec![alloc::vec![0x30u8, 0], alloc::vec![0x31u8, 1, 9]]
            );
            assert_eq!(got[1], alloc::vec![tlv.to_vec()]);
            assert_eq!(got[2], alloc::vec![b(0x13)]);
            assert_eq!(got[3], alloc::vec![b(0x14)]);
            assert_eq!(got[4], alloc::vec![tlv.to_vec()]);
            tst_mux_config_free(cfg);
        }
    }

    /// A handle is an index within its kind: handing a KLV stream's handle to
    /// the video setter selects the n-th VIDEO stream (the kind filter), or
    /// fails with the video "stream out of range" text when there is none.
    #[test]
    fn wrong_kind_handle_resolves_within_the_setters_kind() {
        use crate::config::TstKlvStreamType;
        use crate::config::streams::tst_mux_config_add_klv_stream;
        fn last_msg() -> alloc::string::String {
            unsafe { core::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) }
                .to_string_lossy()
                .into_owned()
        }
        let tlv = [0x28u8, 0x01, 0x77];
        unsafe {
            // KLV only: the KLV handle is canonical, but no video stream exists.
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let k = tst_mux_config_add_klv_stream(
                cfg,
                prog,
                0x101,
                TstKlvStreamType::PrivateData,
                false,
            );
            let rc = tst_mux_config_set_stream_descriptors_for_video(cfg, k, tlv.as_ptr(), 3, 1);
            assert_eq!(rc, TstError::InvalidUsage as i32);
            assert_eq!(
                last_msg(),
                "invalid video stream handle (stream out of range)"
            );
            tst_mux_config_free(cfg);

            // KLV added before video: the video slot is stream 1, not 0.
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            let k = tst_mux_config_add_klv_stream(
                cfg,
                prog,
                0x101,
                TstKlvStreamType::PrivateData,
                false,
            );
            let v = tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264);
            assert_eq!(
                tst_mux_config_set_stream_descriptors_for_video(cfg, k, tlv.as_ptr(), 3, 1),
                0
            );
            let got = &(&*cfg).programs[0].stream_descriptors;
            assert!(got[0].is_empty());
            assert_eq!(got[1], alloc::vec![tlv.to_vec()]);
            assert_eq!(
                tst_mux_config_set_stream_descriptors_for_klv(cfg, v, tlv.as_ptr(), 3, 1),
                0
            );
            assert_eq!(
                (&*cfg).programs[0].stream_descriptors[0],
                alloc::vec![tlv.to_vec()]
            );
            tst_mux_config_free(cfg);
        }
    }
}
