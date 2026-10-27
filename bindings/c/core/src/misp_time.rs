//! ST 0604 MISP-timestamp extraction helper (`tst_misp_time_extract`).

use crate::config::streams::TstVideoCodec;
use crate::error::{TstError, record_bad_enum, set_last_error};

/// Scan an Annex-B access unit for an ST 0604 MISP timestamp SEI.
///
/// Returns 0 and fills the out-params when found; 1 when no MISP SEI is
/// present (out-params untouched); negative `TST_E_*` on null arguments
/// or a malformed MISP payload. `out_kind`: 0 = micro, 1 = nano.
///
/// `au` must point to `len` bytes of an Annex-B access unit. The `codec`
/// argument is one of the `TST_VIDEO_CODEC_*` constants (any other value
/// returns `TST_E_INVALID_CONFIG`) and selects how SEI NAL units are
/// located (H.264 vs H.265 NAL structure); once an SEI payload is found,
/// extraction accepts any known ST 0604 MISP UUID (microsecond or
/// nanosecond) regardless of codec. AV1 and H.266 always return 1 (absent)
/// because ST 0604 defines no SEI carriage for them.
///
/// # Safety
///
/// - `au` must be valid for `len` bytes (or null with `len == 0`).
/// - `out_kind`, `out_time_status`, `out_value` must each be a valid
///   non-null writable pointer to the respective type when the call
///   may return 0 (found). They are untouched on any non-zero return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_misp_time_extract(
    au: *const u8,
    len: usize,
    codec: i32,
    out_kind: *mut u8,
    out_time_status: *mut u8,
    out_value: *mut u64,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(codec) = TstVideoCodec::from_c_int(codec) else {
            record_bad_enum("codec", codec, "TST_VIDEO_CODEC_*");
            return TstError::InvalidConfig as i32;
        };
        let slice = match unsafe { crate::ffi_slice::ffi_slice(au, len, "au") } {
            Ok(s) => s,
            Err(code) => return code,
        };
        if out_kind.is_null() || out_time_status.is_null() || out_value.is_null() {
            set_last_error(TstError::InvalidConfig, "null out-param");
            return TstError::InvalidConfig as i32;
        }
        let mux_codec = match codec {
            TstVideoCodec::H264 => tst_core::mpegts::mux::VideoCodec::H264,
            TstVideoCodec::H265 => tst_core::mpegts::mux::VideoCodec::H265,
            TstVideoCodec::H266 => tst_core::mpegts::mux::VideoCodec::H266,
            TstVideoCodec::Av1 => tst_core::mpegts::mux::VideoCodec::Av1,
        };
        match tst_core::codec::misp_time::extract(slice, mux_codec) {
            Ok(Some(ts)) => {
                unsafe {
                    *out_kind = match ts.kind {
                        tst_core::codec::misp_time::MispTimeKind::Micro => 0,
                        tst_core::codec::misp_time::MispTimeKind::Nano => 1,
                        // non_exhaustive future kinds report as micro
                        _ => 0,
                    };
                    *out_time_status = ts.time_status;
                    *out_value = ts.value;
                }
                0
            }
            Ok(None) => 1,
            Err(e) => {
                set_last_error(TstError::MispTimeMalformed, &alloc::format!("{e}"));
                TstError::MispTimeMalformed as i32
            }
        }
    })
}
