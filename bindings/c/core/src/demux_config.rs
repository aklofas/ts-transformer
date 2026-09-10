//! `tst_demux_config_t` opaque builder + supporting C-ABI enums.
//!
//! Mirrors the `tst_mux_config_t` shape from plan #14: heap-allocated
//! opaque builder, mutating setters returning `i32` codes, `_free`
//! releases. The receiver clones what it needs at `_open_with_config`
//! time; the caller still owns the builder and must `_free` it.

use crate::c_types::c_int;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// `repr(i32)` mirror of `tst_core::mpegts::mux::Av1CarriageMode`.
///
/// Two-valued enum: `Mpeg2TsBinding=0` (default, spec-conformant
/// AV1-in-MPEG-2-TS binding — PES `stream_id=0xBD` and
/// `ts_open_bitstream_unit()` framing per AV1-in-MPEG-2-TS §3.x),
/// `InteropRawObu=1` (interop carriage matching ffmpeg / libaom /
/// hls.js / mediamtx — PES `stream_id=0xE0` and raw OBU payload).
///
/// Set on the demuxer side via `tst_demux_config_set_av1_carriage`
/// so the receiver matches the sender's carriage; mismatched modes
/// surface as `TST_NONCONFORMANT_CODE_AV1_WRONG_STREAM_ID` and
/// `TST_NONCONFORMANT_CODE_AV1_MISSING_TS_OBU_FRAMING` issues.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstAv1CarriageMode {
    Mpeg2TsBinding = 0,
    InteropRawObu = 1,
}

impl TstAv1CarriageMode {
    pub(crate) fn from_c_int(v: c_int) -> Option<Self> {
        match v {
            0 => Some(Self::Mpeg2TsBinding),
            1 => Some(Self::InteropRawObu),
            _ => None,
        }
    }

    pub(crate) fn to_rust(self) -> tst_core::mpegts::mux::Av1CarriageMode {
        use tst_core::mpegts::mux::Av1CarriageMode;
        match self {
            Self::Mpeg2TsBinding => Av1CarriageMode::Mpeg2TsBinding,
            Self::InteropRawObu => Av1CarriageMode::InteropRawObu,
        }
    }
}

/// `repr(i32)` mirror of `tst_core::mpegts::demux::StrictMode`.
///
/// Four-valued enum: `Off=0` (default, lenient), `TimingOnly=1`,
/// `DescriptorsOnly=2`, `Full=3`. cbindgen emits parallel `#define
/// TST_STRICT_MODE_*` blocks for C callers.
///
/// NOTE: this differs from the receiver-surface design doc §7.1, which
/// originally specified `0=Off, 1=KlvOnly, 2=All`. The actual Rust enum
/// is 4-valued; this mapping is the truth.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstStrictMode {
    Off = 0,
    TimingOnly = 1,
    DescriptorsOnly = 2,
    Full = 3,
}

impl TstStrictMode {
    pub(crate) fn from_c_int(v: c_int) -> Option<Self> {
        match v {
            0 => Some(Self::Off),
            1 => Some(Self::TimingOnly),
            2 => Some(Self::DescriptorsOnly),
            3 => Some(Self::Full),
            _ => None,
        }
    }

    pub(crate) fn to_rust(self) -> tst_core::mpegts::demux::StrictMode {
        use tst_core::mpegts::demux::StrictMode;
        match self {
            Self::Off => StrictMode::Off,
            Self::TimingOnly => StrictMode::TimingOnly,
            Self::DescriptorsOnly => StrictMode::DescriptorsOnly,
            Self::Full => StrictMode::Full,
        }
    }
}

// ------------------------------------------------------------------
// TstDemuxConfig opaque builder
// ------------------------------------------------------------------

use crate::c_types::size_t;
use alloc::collections::BTreeMap;
use tst_core::mpegts::demux::{DemuxerConfig, StreamKind, StrictMode};
use tst_core::mpegts::mux::Av1CarriageMode;

/// Opaque demux-config builder. Heap-allocated via `_new`, mutated
/// in place via setters, released via `_free`. The receiver clones
/// what it needs at `_open_with_config` time; the caller still owns
/// the builder.
///
/// Lifecycle mirrors `tst_mux_config_t` from plan #14 exactly.
pub struct TstDemuxConfig {
    strict: StrictMode,
    pes_cap_per_pid: Option<usize>,
    pes_cap_total: Option<usize>,
    klv_link_overrides: Vec<(u16, u16)>,
    stream_kind_overrides: BTreeMap<u16, StreamKind>,
    cfi_tolerance: bool,
    // `None` = use Rust-side default (`Av1CarriageMode::Mpeg2TsBinding`).
    // Explicit `Some(_)` lets the C caller request the interop carriage
    // when the upstream sender ships ffmpeg/libaom/hls.js framing.
    av1_carriage: Option<Av1CarriageMode>,
    // `None` = use Rust-side default (1 MiB).
    au_cell_cap_per_pid: Option<usize>,
    lenient_psi_reassembly: bool,
    unwrap_timestamps: bool,
}

/// Build a `DemuxerConfig` from a C-side `TstDemuxConfig`. Exposed to
/// integration tests in this crate so they can verify the C-ABI
/// builder path end-to-end without going through a real SRT loopback.
/// Not part of the public C ABI (no `extern "C"`).
///
/// **Stability:** internal test-only API. The `test_` prefix combined
/// with `#[doc(hidden)]` follows the established convention in this
/// crate — see `lib.rs::test_clear_last_error` and siblings, whose
/// own comment explains the pattern. Not part of the supported Rust
/// API surface; downstream Rust consumers must not depend on it.
///
/// # Safety
///
/// `cfg` must be a valid non-null pointer returned by
/// `tst_demux_config_new` and not yet freed.
#[doc(hidden)]
pub unsafe fn test_build_options(cfg: *const TstDemuxConfig) -> DemuxerConfig {
    unsafe { (*cfg).build_options() }
}

impl TstDemuxConfig {
    pub(crate) fn build_options(&self) -> DemuxerConfig {
        let mut cfg = DemuxerConfig::default();
        cfg.strict = self.strict;
        cfg.pes_cap_per_pid = self.pes_cap_per_pid;
        cfg.pes_cap_total = self.pes_cap_total;
        cfg.klv_link_overrides = self.klv_link_overrides.clone();
        cfg.stream_kind_overrides = self.stream_kind_overrides.clone();
        cfg.lenient_psi_reassembly = self.lenient_psi_reassembly;
        cfg.unwrap_timestamps = self.unwrap_timestamps;
        cfg.cfi_tolerance = self.cfi_tolerance;
        if let Some(mode) = self.av1_carriage {
            cfg.av1_carriage = mode;
        }
        cfg.au_cell_cap_per_pid = self.au_cell_cap_per_pid;
        cfg
    }
}

/// Allocate a new `tst_demux_config_t` with default values
/// (strict mode = Off, no overrides, default PES caps).
///
/// Returns `NULL` on allocation failure or internal panic.
/// Free with `tst_demux_config_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_new() -> *mut TstDemuxConfig {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstDemuxConfig {
            strict: StrictMode::Off,
            pes_cap_per_pid: None,
            pes_cap_total: None,
            klv_link_overrides: Vec::new(),
            stream_kind_overrides: BTreeMap::new(),
            // Tolerance-by-default — see tst_core::DemuxerConfig rustdoc
            // and CHANGELOG entry on the 2026-05-24 default flip.
            cfi_tolerance: true,
            av1_carriage: None,
            au_cell_cap_per_pid: None,
            lenient_psi_reassembly: false,
            unwrap_timestamps: false,
        }))
    })
}

/// Release a `tst_demux_config_t`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_free(cfg: *mut TstDemuxConfig) {
    crate::panic::ffi_catch((), || {
        if !cfg.is_null() {
            drop(unsafe { Box::from_raw(cfg) });
        }
    })
}

/// Set the demuxer's strict mode. `mode` is one of
/// `TST_STRICT_MODE_OFF` (0, default), `TST_STRICT_MODE_TIMING_ONLY` (1),
/// `TST_STRICT_MODE_DESCRIPTORS_ONLY` (2), or `TST_STRICT_MODE_FULL` (3).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg` or
/// unrecognized `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_strict_mode(
    cfg: *mut TstDemuxConfig,
    mode: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        let Some(parsed) = TstStrictMode::from_c_int(mode) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "unrecognized strict mode (valid: 0..=3)",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.strict = parsed.to_rust();
        0
    })
}

/// Add a `klv_pid` → `video_pid` KLV-link override. Bypasses PMT-descriptor
/// inference. Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_add_link_klv(
    cfg: *mut TstDemuxConfig,
    klv_pid: u16,
    video_pid: u16,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.klv_link_overrides.push((klv_pid, video_pid));
        0
    })
}

/// Force a PID's stream-kind classification. `stream_kind` is one of
/// `TST_STREAM_KIND_VIDEO_H264`, `TST_STREAM_KIND_VIDEO_H265`,
/// `TST_STREAM_KIND_AUDIO_MP2`, ... — see the header for the full table.
///
/// NOTE: The C-side mapping flattens (TstStreamKindTag × codec) pairs
/// into single integers. The implementation table below covers the
/// common cases — extend if a consumer asks for a kind not listed.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg` or
/// unrecognized `stream_kind`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_add_treat_as(
    cfg: *mut TstDemuxConfig,
    pid: u16,
    stream_kind: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        use tst_core::mpegts::demux::{AudioCodec, StreamKind, SubtitleCodec, VideoCodec};
        // Flat mapping: 100..=103 = Video H264/H265/H266/AV1;
        //               200..=203 = Audio MP2/AAC/AacLatm/AC3;
        //               300..=303 = Subtitle DvbSub/DvbTeletext/Cea708/WebVtt;
        //               400 = KlvSync (declared_link=None);
        //               401 = KlvAsync.
        let kind = match stream_kind {
            100 => StreamKind::Video(VideoCodec::H264),
            101 => StreamKind::Video(VideoCodec::H265),
            102 => StreamKind::Video(VideoCodec::H266),
            103 => StreamKind::Video(VideoCodec::Av1),
            200 => StreamKind::Audio(AudioCodec::Mp2),
            201 => StreamKind::Audio(AudioCodec::Aac),
            202 => StreamKind::Audio(AudioCodec::AacLatm),
            203 => StreamKind::Audio(AudioCodec::Ac3),
            300 => StreamKind::Subtitle(SubtitleCodec::DvbSubtitling),
            301 => StreamKind::Subtitle(SubtitleCodec::DvbTeletext),
            302 => StreamKind::Subtitle(SubtitleCodec::Cea708Standalone),
            303 => StreamKind::Subtitle(SubtitleCodec::WebVttInTs),
            400 => StreamKind::KlvSync {
                declared_link: None,
            },
            401 => StreamKind::KlvAsync,
            _ => {
                crate::error::set_last_error(
                    crate::error::TstError::InvalidConfig,
                    "unrecognized stream_kind integer",
                );
                return crate::error::TstError::InvalidConfig as i32;
            }
        };
        cfg.stream_kind_overrides.insert(pid, kind);
        0
    })
}

/// Set PES reassembly caps. `0` means use the Rust-side default.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_pes_cap(
    cfg: *mut TstDemuxConfig,
    per_pid: size_t,
    total: size_t,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.pes_cap_per_pid = if per_pid == 0 { None } else { Some(per_pid) };
        cfg.pes_cap_total = if total == 0 { None } else { Some(total) };
        0
    })
}

/// Toggle tolerance for sync-metadata AU cells whose
/// `cell_fragment_indication` bits are set to `0b00` (Middle) or
/// `0b01` (Last) without a prior `First` cell. When tolerance is on
/// AND the orphan cell's inner payload independently validates as a
/// complete KLV record (SMPTE 336M UL prefix + BER length match), the
/// demuxer emits a `KlvSyncAuCell` event with
/// `cell_fragment_indication` substituted to `Complete` AND a
/// `TST_NONCONFORMANT_CODE_CFI_TOLERATED` (= 32) diagnostic carrying
/// the observed and substituted CFI bytes on `cc_expected` and
/// `cc_observed`.
///
/// **Default `true`** — corpus-wide validation showed the
/// producer-side CFI=00-on-single-cell-AU bug is dominant in real
/// STANAG 4609 traffic (~99% of NonConformant events), and no other
/// public reference decoder enforces CFI either. Set to `false` for
/// spec-strict conformance testing: orphan cells then surface only
/// as `TST_NONCONFORMANT_CODE_MULTI_CELL_AU` with reason
/// `TST_MULTI_CELL_AU_REASON_ORPHAN` and no metadata event.
///
/// `enable` is read as a C `bool` (any non-zero value enables).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_cfi_tolerance(
    cfg: *mut TstDemuxConfig,
    enable: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.cfi_tolerance = enable != 0;
        0
    })
}

/// Set the demuxer's expected AV1 PES carriage mode. `mode` is one of
/// `TST_AV1_CARRIAGE_MODE_MPEG2_TS_BINDING` (0, default — spec-conformant
/// per the AV1-in-MPEG-2-TS binding) or `TST_AV1_CARRIAGE_MODE_INTEROP_RAW_OBU`
/// (1 — matches ffmpeg / libaom / hls.js / mediamtx senders).
///
/// In binding mode the demuxer expects PES `stream_id=0xBD` and
/// `ts_open_bitstream_unit()` framing on each OBU; violations surface
/// as `TST_NONCONFORMANT_CODE_AV1_WRONG_STREAM_ID` and
/// `TST_NONCONFORMANT_CODE_AV1_MISSING_TS_OBU_FRAMING` issues. In
/// interop mode the demuxer accepts raw OBUs without that framing.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg` or
/// unrecognized `mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_av1_carriage(
    cfg: *mut TstDemuxConfig,
    mode: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        let Some(parsed) = TstAv1CarriageMode::from_c_int(mode) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "unrecognized av1 carriage mode (valid: 0..=1)",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.av1_carriage = Some(parsed.to_rust());
        0
    })
}

/// Set the per-PID cap on the in-flight sync-metadata AU cell reassembly
/// buffer. `cap_bytes` of `0` means use the Rust-side default (1 MiB).
///
/// When the buffered inner-byte total would exceed this cap, the demuxer
/// drops the in-flight buffer and emits a
/// `TST_NONCONFORMANT_CODE_MULTI_CELL_AU` with
/// `multi_cell_au_reason = TST_MULTI_CELL_AU_REASON_OVERFLOW`. Tune up
/// for streams with unusually large sync-metadata AUs; tune down for
/// adversarial-input scenarios where faster failure (and a tighter
/// memory bound) is preferable.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_au_cell_cap_per_pid(
    cfg: *mut TstDemuxConfig,
    cap_bytes: size_t,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.au_cell_cap_per_pid = if cap_bytes == 0 {
            None
        } else {
            Some(cap_bytes)
        };
        0
    })
}

/// Enable lenient PSI section reassembly across continuity-counter jumps.
/// `enable` is read as a C `bool` (any non-zero value enables). Default
/// is `false` (strict).
///
/// In strict (default) mode, a continuity-counter jump on a PSI PID drops
/// the in-flight partial section and emits a
/// `TST_NONCONFORMANT_CODE_PSI_CC_DISCONTINUITY` diagnostic (matches
/// ffmpeg `mpegts.c:3118-3142`). In lenient mode, the continuation
/// packets are accepted across the jump (today's permissive behavior —
/// the section either passes by luck or fails its CRC at the end).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_lenient_psi_reassembly(
    cfg: *mut TstDemuxConfig,
    enable: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.lenient_psi_reassembly = enable != 0;
        0
    })
}

/// Enable the opt-in PTS/DTS unwrap. `enable` is read as a C `bool`
/// (any non-zero value enables). Default is `false` (raw wire PTS/DTS,
/// matching today's behavior). See
/// `tst_core::mpegts::demux::DemuxerConfig::unwrap_timestamps` for the
/// full unwrap semantics: a per-PID accumulator adds each sample's
/// signed wrap-aware delta onto the previous unwrapped value, so a
/// genuine reorder steps back by its true distance instead of being
/// read as another wrap; PIDs of the same program stay comparable
/// because a PID starting late anchors onto its program's running
/// clock; DTS unwraps against its own PES's PTS; and the state resets
/// on reconnect and on PID/program topology changes.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` on null `cfg`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_demux_config_set_unwrap_timestamps(
    cfg: *mut TstDemuxConfig,
    enable: c_int,
) -> c_int {
    crate::panic::ffi_catch(crate::error::TstError::PanicCaught as i32, || {
        let Some(cfg) = (unsafe { cfg.as_mut() }) else {
            crate::error::set_last_error(
                crate::error::TstError::InvalidConfig,
                "null config pointer",
            );
            return crate::error::TstError::InvalidConfig as i32;
        };
        cfg.unwrap_timestamps = enable != 0;
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mode_round_trip() {
        for v in 0..=3 {
            let m = TstStrictMode::from_c_int(v).expect("recognized");
            assert_eq!(m as i32, v);
        }
        assert!(TstStrictMode::from_c_int(-1).is_none());
        assert!(TstStrictMode::from_c_int(4).is_none());
    }

    #[test]
    fn new_then_free_smoke() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert!(!cfg.is_null());
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn double_free_safe() {
        unsafe {
            let cfg = tst_demux_config_new();
            tst_demux_config_free(cfg);
            // Calling _free again on the same pointer is documented as
            // undefined behavior; this test only validates _free(NULL).
            tst_demux_config_free(core::ptr::null_mut());
        }
    }

    #[test]
    fn set_strict_mode_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_strict_mode(core::ptr::null_mut(), 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn set_strict_mode_invalid_value_returns_invalid_config() {
        unsafe {
            let cfg = tst_demux_config_new();
            let rc = tst_demux_config_set_strict_mode(cfg, 999);
            assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn add_link_klv_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_add_link_klv(core::ptr::null_mut(), 0x101, 0x102) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn add_treat_as_unrecognized_kind_returns_invalid_config() {
        unsafe {
            let cfg = tst_demux_config_new();
            let rc = tst_demux_config_add_treat_as(cfg, 0x101, 9999);
            assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_pes_cap_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_pes_cap(core::ptr::null_mut(), 1024, 65536) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn cfi_tolerance_default_is_true() {
        unsafe {
            let cfg = tst_demux_config_new();
            let opts = (*cfg).build_options();
            assert!(opts.cfi_tolerance);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_cfi_tolerance_toggles() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert_eq!(tst_demux_config_set_cfi_tolerance(cfg, 1), 0);
            assert!((*cfg).build_options().cfi_tolerance);
            assert_eq!(tst_demux_config_set_cfi_tolerance(cfg, 0), 0);
            assert!(!(*cfg).build_options().cfi_tolerance);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_cfi_tolerance_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_cfi_tolerance(core::ptr::null_mut(), 1) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn av1_carriage_mode_round_trip() {
        for v in 0..=1 {
            let m = TstAv1CarriageMode::from_c_int(v).expect("recognized");
            assert_eq!(m as i32, v);
        }
        assert!(TstAv1CarriageMode::from_c_int(-1).is_none());
        assert!(TstAv1CarriageMode::from_c_int(2).is_none());
    }

    #[test]
    fn av1_carriage_default_is_rust_default() {
        // Absent any setter call, build_options() must produce the
        // Rust-side default (Mpeg2TsBinding) — the C wrapper must NOT
        // silently force a different mode.
        unsafe {
            let cfg = tst_demux_config_new();
            let opts = (*cfg).build_options();
            assert_eq!(
                opts.av1_carriage,
                tst_core::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding
            );
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_av1_carriage_toggles() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert_eq!(
                tst_demux_config_set_av1_carriage(cfg, TstAv1CarriageMode::InteropRawObu as i32),
                0,
            );
            assert_eq!(
                (*cfg).build_options().av1_carriage,
                tst_core::mpegts::mux::Av1CarriageMode::InteropRawObu
            );
            assert_eq!(
                tst_demux_config_set_av1_carriage(cfg, TstAv1CarriageMode::Mpeg2TsBinding as i32),
                0,
            );
            assert_eq!(
                (*cfg).build_options().av1_carriage,
                tst_core::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding
            );
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_av1_carriage_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_av1_carriage(core::ptr::null_mut(), 0) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn set_av1_carriage_invalid_value_returns_invalid_config() {
        unsafe {
            let cfg = tst_demux_config_new();
            let rc = tst_demux_config_set_av1_carriage(cfg, 999);
            assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn au_cell_cap_default_is_none() {
        unsafe {
            let cfg = tst_demux_config_new();
            // `None` lets the Rust side use DEFAULT_AU_CELL_CAP_PER_PID
            // (1 MiB). We assert by checking the option is None at the
            // C wrapper layer — Rust-side default replacement happens in
            // the demuxer itself.
            let opts = (*cfg).build_options();
            assert_eq!(opts.au_cell_cap_per_pid, None);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_au_cell_cap_per_pid_sets_value() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert_eq!(
                tst_demux_config_set_au_cell_cap_per_pid(cfg, 2 * 1024 * 1024),
                0
            );
            assert_eq!(
                (*cfg).build_options().au_cell_cap_per_pid,
                Some(2 * 1024 * 1024)
            );
            // Zero resets back to default (None).
            assert_eq!(tst_demux_config_set_au_cell_cap_per_pid(cfg, 0), 0);
            assert_eq!((*cfg).build_options().au_cell_cap_per_pid, None);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_au_cell_cap_per_pid_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_au_cell_cap_per_pid(core::ptr::null_mut(), 1024) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn lenient_psi_reassembly_default_is_false() {
        unsafe {
            let cfg = tst_demux_config_new();
            let opts = (*cfg).build_options();
            assert!(!opts.lenient_psi_reassembly);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_lenient_psi_reassembly_toggles() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert_eq!(tst_demux_config_set_lenient_psi_reassembly(cfg, 1), 0);
            assert!((*cfg).build_options().lenient_psi_reassembly);
            assert_eq!(tst_demux_config_set_lenient_psi_reassembly(cfg, 0), 0);
            assert!(!(*cfg).build_options().lenient_psi_reassembly);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_lenient_psi_reassembly_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_lenient_psi_reassembly(core::ptr::null_mut(), 1) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }

    #[test]
    fn unwrap_timestamps_default_is_false() {
        unsafe {
            let cfg = tst_demux_config_new();
            let opts = (*cfg).build_options();
            assert!(!opts.unwrap_timestamps);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_unwrap_timestamps_toggles() {
        unsafe {
            let cfg = tst_demux_config_new();
            assert_eq!(tst_demux_config_set_unwrap_timestamps(cfg, 1), 0);
            assert!((*cfg).build_options().unwrap_timestamps);
            assert_eq!(tst_demux_config_set_unwrap_timestamps(cfg, 0), 0);
            assert!(!(*cfg).build_options().unwrap_timestamps);
            tst_demux_config_free(cfg);
        }
    }

    #[test]
    fn set_unwrap_timestamps_null_cfg_returns_invalid_config() {
        let rc = unsafe { tst_demux_config_set_unwrap_timestamps(core::ptr::null_mut(), 1) };
        assert_eq!(rc, crate::error::TstError::InvalidConfig as i32);
    }
}
