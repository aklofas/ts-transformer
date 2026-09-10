//! `tst_annexb_to_length_prefixed` / `tst_param_sets_*` — Annex B ↔
//! length-prefixed NAL conversion and parameter-set extraction.
//!
//! Wraps `tst_core::codec::nal_framing::{annexb_to_length_prefixed_len,
//! annexb_to_length_prefixed_into, extract_parameter_sets}`. Apple's
//! VideoToolbox (and the ISO/IEC
//! 14496-15 AVCC/HVCC sample formats it expects) wants H.264/H.265 NALs
//! length-prefixed rather than Annex-B start-code-delimited, and wants
//! parameter sets (VPS/SPS/PPS) handed to
//! `CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets`
//! separately from the sample data — this module is the C-callable
//! surface for both conversions, needed by the Apple-PoC consumer.
//!
//! Unconditional module (no `srt`/`rtp`/... feature gate) — `tst-core`
//! is a non-optional dependency of `tst-c`, matching the existing
//! offline `tst_demuxer_*` / `tst_muxer_*` / `tst_st0601_*` surfaces.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;

use tst_core::codec::CodecParseError;
use tst_core::codec::nal_framing::{self, ParameterSets};

use crate::config::TstVideoCodec;
use crate::error::{TstError, set_last_error};

// ---------------------------------------------------------------------------
// Annex B -> length-prefixed conversion
// ---------------------------------------------------------------------------

/// Map a [`CodecParseError`] raised by
/// [`nal_framing::annexb_to_length_prefixed_len`] /
/// [`nal_framing::annexb_to_length_prefixed_into`] to a `TstError` and
/// record it. Only [`CodecParseError::InvalidLengthSize`] and
/// [`CodecParseError::NalLengthOverflow`] can ever reach it: those two
/// are all the sizing pass can raise (see its doc), and the one extra
/// error the writing pass has — [`CodecParseError::BufferTooSmall`] — is
/// pre-empted by the `out_cap` check that guards it. The wildcard arm
/// exists only to satisfy `#[non_exhaustive]` match exhaustiveness and
/// is not expected to fire.
fn record_nal_framing_error(e: &CodecParseError) -> i32 {
    let code = match e {
        CodecParseError::InvalidLengthSize { .. } => TstError::InvalidConfig,
        CodecParseError::NalLengthOverflow { .. } => TstError::TooLarge,
        _ => TstError::Internal,
    };
    set_last_error(code, &e.to_string());
    code as i32
}

/// Convert an Annex-B-framed NAL buffer into length-prefixed (AVCC/HVCC)
/// framing — see [`nal_framing::annexb_to_length_prefixed`].
///
/// **Two-call idiom.** Pass `out = NULL` (any `out_cap`) to learn the
/// required size: this always returns `TST_E_BUFFER_FULL` (-4) with the
/// required byte count in `*out_len`, without writing to `out`, even
/// when the required size happens to be 0 — call again with any
/// non-null `out` once `*out_len` reads back. With a non-null `out` and
/// `out_cap >= *out_len` from the query call, the conversion is written
/// into `out`, `*out_len` is set to the actual byte count written (equal
/// to the previously-queried size), and this returns 0.
///
/// Returns `TST_E_INVALID_CONFIG` (-1) without touching `*out_len` if
/// `out_len` is NULL, `annexb` is NULL with `annexb_len > 0`, or
/// `length_size` is not 1, 2, or 4. Returns `TST_E_TOO_LARGE` (-6)
/// without touching `*out_len` if a single NAL's byte length exceeds
/// what `length_size` bytes can encode.
///
/// # Safety
///
/// `annexb` must be valid for reads of `annexb_len` bytes (or NULL with
/// `annexb_len == 0`). `out_len` must be a valid writable `size_t`
/// pointer. When `out` is non-null, it must be valid for writes of
/// `out_cap` bytes.
///
/// # C ABI
///
/// `tst_annexb_to_length_prefixed` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_annexb_to_length_prefixed(
    annexb: *const u8,
    annexb_len: usize,
    length_size: u8,
    out: *mut u8,
    out_cap: usize,
    out_len: *mut usize,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(out_len_ref) = (unsafe { out_len.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null out_len pointer");
            return TstError::InvalidConfig as i32;
        };
        let annexb_slice =
            match unsafe { crate::ffi_slice::ffi_slice(annexb, annexb_len, "annexb") } {
                Ok(s) => s,
                Err(rc) => return rc,
            };
        // Sizing pass only — no output is built here, so the query call
        // of the two-call idiom costs one scan of `annexb` and no
        // allocation. Errors leave `*out_len` untouched, as documented.
        let needed = match nal_framing::annexb_to_length_prefixed_len(annexb_slice, length_size) {
            Ok(n) => n,
            Err(e) => return record_nal_framing_error(&e),
        };

        if out.is_null() || out_cap < needed {
            *out_len_ref = needed;
            return TstError::BufferFull as i32;
        }
        // Only the `needed` bytes this call will actually write are
        // materialized as a slice, even when the caller offers more.
        let out_slice = match unsafe { crate::ffi_slice::ffi_slice_mut(out, needed, "out") } {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        // Writes straight into the caller's buffer — no intermediate
        // `Vec` to fill and copy out of.
        if let Err(e) =
            nal_framing::annexb_to_length_prefixed_into(annexb_slice, length_size, out_slice)
        {
            return record_nal_framing_error(&e);
        }
        *out_len_ref = needed;
        0
    })
}

// ---------------------------------------------------------------------------
// Parameter-set extraction
// ---------------------------------------------------------------------------

/// Opaque handle wrapping the parameter-set NALs extracted by
/// [`tst_param_sets_extract`] — see [`nal_framing::extract_parameter_sets`].
/// Immutable once built; no separate `_close`, only
/// [`tst_param_sets_free`].
///
/// Deliberately a bare `Box<ParameterSets>`, *not* the crate's
/// mutex-backed `Handle<T>` that [`crate::klv_st0601::TstSt0601`] uses —
/// `ParameterSets` has no mutation surface and no `_close`, so there is
/// nothing for a `Handle`'s lock to protect; a plain `Box` is the
/// simpler choice, equally sound for a handle that is only ever read
/// then freed.
pub struct TstParamSets {
    inner: ParameterSets,
}

/// `which` bucket selector shared by [`tst_param_sets_count`] and
/// [`tst_param_sets_get`]: `0` = VPS, `1` = SPS, `2` = PPS. Returns
/// `None` for any other value.
fn bucket(sets: &ParameterSets, which: crate::c_types::c_int) -> Option<&Vec<Vec<u8>>> {
    match which {
        0 => Some(&sets.vps),
        1 => Some(&sets.sps),
        2 => Some(&sets.pps),
        _ => None,
    }
}

/// Extract VPS/SPS/PPS NALs from an Annex-B access unit — see
/// [`nal_framing::extract_parameter_sets`]. Each returned NAL (via
/// [`tst_param_sets_get`]) is complete: header byte(s) included, no
/// start code, no length prefix — ready for
/// `CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets`.
///
/// Non-fallible on the Rust side: an `annexb` buffer with no parameter
/// sets (or a codec with no parameter-set NALs at all — H.266 and AV1
/// today, see [`nal_framing::extract_parameter_sets`]'s doc) still
/// returns a non-null handle with every bucket count at 0. NULL is
/// returned only for a caller-side argument error: an unrecognized
/// `codec` value (anything outside the four `TST_VIDEO_CODEC_*`
/// constants in the generated header), with `TST_E_INVALID_CONFIG`
/// recorded on the last-error channel.
///
/// # Safety
///
/// `annexb` must be valid for reads of `len` bytes (or NULL with
/// `len == 0`).
///
/// # C ABI
///
/// `tst_param_sets_extract` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_param_sets_extract(
    annexb: *const u8,
    len: usize,
    codec: crate::c_types::c_int,
) -> *mut TstParamSets {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        let Some(codec) = TstVideoCodec::from_c_int(codec) else {
            set_last_error(
                TstError::InvalidConfig,
                "unrecognized video codec (valid: 0..=3)",
            );
            return core::ptr::null_mut();
        };
        let slice = match unsafe { crate::ffi_slice::ffi_slice(annexb, len, "annexb") } {
            Ok(s) => s,
            Err(_) => return core::ptr::null_mut(),
        };
        let inner = nal_framing::extract_parameter_sets(slice, codec.to_core());
        Box::into_raw(Box::new(TstParamSets { inner }))
    })
}

/// Number of NALs in the `which` bucket (`0` = VPS, `1` = SPS,
/// `2` = PPS). Returns 0 for a NULL `p` or an out-of-range `which` — a
/// side-channel query with no failure signal beyond the count itself
/// (matches [`crate::klv_st0601::tst_st0601_state`]'s convention); it
/// never touches the last-error channel.
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstParamSets` from
/// [`tst_param_sets_extract`], or NULL.
///
/// # C ABI
///
/// `tst_param_sets_count` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_param_sets_count(
    p: *const TstParamSets,
    which: crate::c_types::c_int,
) -> usize {
    crate::panic::ffi_catch(0, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            return 0;
        };
        bucket(&handle.inner, which).map_or(0, Vec::len)
    })
}

/// Write a view `(*out_ptr, *out_len)` onto the `idx`-th NAL in the
/// `which` bucket (`0` = VPS, `1` = SPS, `2` = PPS) — a complete NAL,
/// header byte(s) included, no start code, no length prefix. The view
/// aliases memory owned by `p`; it is valid only until
/// [`tst_param_sets_free`] is called on the same pointer — do not
/// retain it past that call.
///
/// Returns 0 on success. Returns `TST_E_NOT_FOUND` (-14) without
/// writing either output if `idx` is out of range for `which`. Returns
/// `TST_E_INVALID_CONFIG` (-1) without writing either output if `p`,
/// `out_ptr`, or `out_len` is NULL, or if `which` is out of range.
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstParamSets`. `out_ptr` and
/// `out_len` must be valid writable pointers.
///
/// # C ABI
///
/// `tst_param_sets_get` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_param_sets_get(
    p: *const TstParamSets,
    which: crate::c_types::c_int,
    idx: usize,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null param-sets pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(out_ptr_ref) = (unsafe { out_ptr.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null out_ptr pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(out_len_ref) = (unsafe { out_len.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null out_len pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(list) = bucket(&handle.inner, which) else {
            set_last_error(
                TstError::InvalidConfig,
                "which out of range (valid: 0=vps, 1=sps, 2=pps)",
            );
            return TstError::InvalidConfig as i32;
        };
        let Some(nal) = list.get(idx) else {
            return crate::error::record_not_found("idx out of range for this bucket");
        };
        *out_ptr_ref = nal.as_ptr();
        *out_len_ref = nal.len();
        0
    })
}

/// Free a handle obtained from [`tst_param_sets_extract`]. Invalidates
/// every view previously returned by [`tst_param_sets_get`] on this
/// pointer. NULL-safe; freeing twice is undefined behavior (matches
/// every other `tst_*_free` in this crate).
///
/// # Safety
///
/// `p` must be a valid `*mut TstParamSets` from
/// [`tst_param_sets_extract`], or NULL. Must not be called more than
/// once on the same pointer.
///
/// # C ABI
///
/// `tst_param_sets_free` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_param_sets_free(p: *mut TstParamSets) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(p) });
    })
}
