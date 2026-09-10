//! Typed codec parameter-set parsers and audio frame iterators.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Stateless parsers for video codec parameter sets (SPS / VPS / PPS)
//! and audio frame iteration. Each codec lives in its own submodule
//! with consistent function shape. Consumers receive raw NAL units or
//! audio PES bytes from [`crate::mpegts::demux`] and call the parser
//! explicitly when typed fields are needed.
//!
//! Shipped: H.264 ([`h264`]), H.265 ([`h265`]), H.266 ([`h266`]),
//! AV1 ([`av1`]), MPEG audio Layer I/II/III ([`mpegaudio`]), AAC ADTS
//! ([`aac`]), AC-3 syncframe header ([`ac3`]). AAC LATM frame iterator
//! is deferred to a follow-up plan; AC-3 full-frame iteration (vs the
//! header-only parser shipped) is deferred to a follow-up plan.

use alloc::string::String;
pub mod aac;
pub mod ac3;
pub(crate) mod annexb;
pub mod av1;
pub(crate) mod bitreader;
pub(crate) mod framing;
pub mod h264;
pub mod h265;
pub mod h266;
pub mod misp_time;
pub mod mpegaudio;
pub mod nal_framing;
pub mod util;

/// Chroma subsampling format. From H.264 / H.265 `chroma_format_idc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaFormat {
    /// 4:0:0 — luma only.
    Monochrome,
    /// 4:2:0 — half horizontal + half vertical chroma.
    Yuv420,
    /// 4:2:2 — half horizontal chroma only.
    Yuv422,
    /// 4:4:4 — full chroma.
    Yuv444,
}

/// Numerator/denominator pair (no implicit reduction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

/// H.264 §E.2.1 / H.265 §E.3.1 / H.266 §7.7.3.1 `aspect_ratio_idc`→SAR
/// table (ITU-T T.832 Table E-1 / HEVC Table E-1). Returns `None` for
/// idc=0 (unspecified) and idc=255 (Extended_SAR; the extended-SAR width
/// and height are signalled separately by the caller).
pub(crate) fn aspect_ratio_idc_to_sar(idc: u8) -> Option<Rational> {
    Some(match idc {
        1 => Rational { num: 1, den: 1 },
        2 => Rational { num: 12, den: 11 },
        3 => Rational { num: 10, den: 11 },
        4 => Rational { num: 16, den: 11 },
        5 => Rational { num: 40, den: 33 },
        6 => Rational { num: 24, den: 11 },
        7 => Rational { num: 20, den: 11 },
        8 => Rational { num: 32, den: 11 },
        9 => Rational { num: 80, den: 33 },
        10 => Rational { num: 18, den: 11 },
        11 => Rational { num: 15, den: 11 },
        12 => Rational { num: 64, den: 33 },
        13 => Rational { num: 160, den: 99 },
        14 => Rational { num: 4, den: 3 },
        15 => Rational { num: 3, den: 2 },
        16 => Rational { num: 2, den: 1 },
        _ => return None,
    })
}

/// VUI / video signal type metadata. All fields decoded per
/// ITU-T H.273 / ISO/IEC 23091-2 (the codec-independent registry
/// referenced by both H.264 §E.2.1 and H.265 §E.2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorInfo {
    pub primaries: ColourPrimaries,
    pub transfer: TransferCharacteristics,
    pub matrix: MatrixCoefficients,
    /// `false` = limited range (16-235 for 8-bit luma); `true` = full range (0-255).
    pub full_range: bool,
    /// Chroma sample location type. `None` when the bitstream did not
    /// signal `chroma_sample_loc_info`. Per spec the value is constrained:
    /// 0..=5 for H.264 §E.2.1 / H.265 §E.2.1; 0..=6 for H.266 via H.274
    /// §7.3 (p. 20). For H.266 streams with `vui_chroma_loc_info_present_flag
    /// = 0` AND `ChromaFormatIdc == 1` (4:2:0), H.274 §7.3 (p. 20) infers
    /// `vui_chroma_sample_loc_type_frame = 6` ("unknown or unspecified") —
    /// callers needing the inferred value should substitute 6 when
    /// `chroma_loc.is_none()` in that case. The parser does not pre-populate
    /// the inference to keep "absent" and "absent and inferred" distinguishable.
    pub chroma_loc: Option<u8>,
    /// Pixel aspect ratio. `None` when `aspect_ratio_idc == 0` (unspecified).
    pub sample_aspect_ratio: Option<Rational>,
}

/// ITU-T H.273 V4 (07/2024) §8.1 Table 2 — colour primaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourPrimaries {
    Bt709,
    Unspecified,
    Bt470M,
    Bt470Bg,
    Smpte170M,
    Smpte240M,
    Film,
    Bt2020,
    SmpteSt428,
    /// SMPTE RP 431-2 — DCI-P3 primaries (DCI white point). H.273 value 11.
    /// (RP, not ST — the H.273 informative name is "SMPTE RP 431-2".)
    SmpteRp431_2,
    /// SMPTE EG 432-1 — Display P3 primaries (D65 white point). H.273 value 12.
    /// (EG, not ST — the H.273 informative name is "SMPTE EG 432-1".)
    SmpteEg432_1,
    Ebu3213E,
    /// Spec-reserved or registry-extension value; preserved verbatim.
    Reserved(u8),
}

impl ColourPrimaries {
    /// Decode from H.273 `colour_primaries` byte.
    pub fn from_h273(v: u8) -> Self {
        match v {
            1 => Self::Bt709,
            2 => Self::Unspecified,
            4 => Self::Bt470M,
            5 => Self::Bt470Bg,
            6 => Self::Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::Film,
            9 => Self::Bt2020,
            10 => Self::SmpteSt428,
            11 => Self::SmpteRp431_2,
            12 => Self::SmpteEg432_1,
            22 => Self::Ebu3213E,
            other => Self::Reserved(other),
        }
    }
}

/// ITU-T H.273 V4 (07/2024) §8.2 Table 3 — transfer characteristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCharacteristics {
    Bt709,
    Unspecified,
    Gamma22,
    Gamma28,
    Smpte170M,
    Smpte240M,
    Linear,
    Log100,
    LogSqrt,
    Iec61966_2_4,
    Bt1361E,
    Iec61966_2_1,
    Bt2020Bits10,
    Bt2020Bits12,
    /// SMPTE ST 2084 — perceptual quantizer (HDR PQ).
    SmpteSt2084,
    SmpteSt428,
    /// ARIB STD-B67 — hybrid log-gamma (HDR HLG).
    AribStdB67,
    Reserved(u8),
}

impl TransferCharacteristics {
    pub fn from_h273(v: u8) -> Self {
        match v {
            1 => Self::Bt709,
            2 => Self::Unspecified,
            4 => Self::Gamma22,
            5 => Self::Gamma28,
            6 => Self::Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::Linear,
            9 => Self::Log100,
            10 => Self::LogSqrt,
            11 => Self::Iec61966_2_4,
            12 => Self::Bt1361E,
            13 => Self::Iec61966_2_1,
            14 => Self::Bt2020Bits10,
            15 => Self::Bt2020Bits12,
            16 => Self::SmpteSt2084,
            17 => Self::SmpteSt428,
            18 => Self::AribStdB67,
            other => Self::Reserved(other),
        }
    }
}

/// ITU-T H.273 V4 (07/2024) §8.3 Table 4 — matrix coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixCoefficients {
    Identity,
    Bt709,
    Unspecified,
    FccMc,
    Bt470Bg,
    Smpte170M,
    Smpte240M,
    YCgCo,
    Bt2020NonConstant,
    Bt2020Constant,
    SmpteSt2085,
    ChromaDerivedNonConstant,
    ChromaDerivedConstant,
    IctCp,
    /// `15`: IPT-C2 (SMPTE IPT-PQ-C2). Added in H.273 V4.
    IptC2,
    /// `16`: YCgCo-Re — YCgCo-R with even bit-depth offset. Added in H.273 V4.
    YCgCoRe,
    /// `17`: YCgCo-Ro — YCgCo-R with odd bit-depth offset. Added in H.273 V4.
    YCgCoRo,
    /// Codepoints 18..255 reserved per H.273 V4 Table 4. Preserved verbatim.
    Reserved(u8),
}

impl MatrixCoefficients {
    pub fn from_h273(v: u8) -> Self {
        match v {
            0 => Self::Identity,
            1 => Self::Bt709,
            2 => Self::Unspecified,
            4 => Self::FccMc,
            5 => Self::Bt470Bg,
            6 => Self::Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::YCgCo,
            9 => Self::Bt2020NonConstant,
            10 => Self::Bt2020Constant,
            11 => Self::SmpteSt2085,
            12 => Self::ChromaDerivedNonConstant,
            13 => Self::ChromaDerivedConstant,
            14 => Self::IctCp,
            15 => Self::IptC2,
            16 => Self::YCgCoRe,
            17 => Self::YCgCoRo,
            other => Self::Reserved(other),
        }
    }
}

/// Errors returned by codec parameter-set parsers.
///
/// All variants are non-panicking: bitstream walks are bounded, Golomb
/// decoders are capped at 32 leading zeros, enum casts use try-from with
/// `Err → ReservedValue`. See [crate root](crate::codec) for partial-success
/// behavioral rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CodecParseError {
    /// Bitstream cursor walked past end of input. `needed_bits` is the
    /// shortfall in bits at the position where parsing failed.
    #[error("truncated RBSP at bit {offset_bits} (needed {needed_bits} more bits)")]
    TruncatedRbsp { offset_bits: u32, needed_bits: u32 },

    /// Malformed Exp-Golomb code (leading zeros run > 32, or missing
    /// terminator bit).
    #[error("invalid Exp-Golomb code at bit {offset_bits}")]
    InvalidGolomb { offset_bits: u32 },

    /// Field carries a spec-reserved value that affects framing or
    /// downstream parsing semantics. `field` is a `&'static str` for
    /// cheap formatting; consumers should not pattern-match on it.
    #[error("reserved value {value} in field '{field}'")]
    ReservedValue { field: &'static str, value: u32 },

    /// Profile we cannot VUI-extract for. Fires only for profiles whose
    /// downstream framing semantics differ enough to produce wrong
    /// fields. Most uncommon profiles are supported as opaque integers.
    #[error("unsupported profile_idc {profile_idc}")]
    UnsupportedProfile { profile_idc: u8 },

    /// `parse_pps` standalone references an SPS id that wasn't seen.
    /// Does not fire from `parse_parameter_sets` (which collects SPSes
    /// before resolving PPSes).
    #[error("PPS references SPS id {sps_id} which was not in the input")]
    DanglingSpsReference { sps_id: u8 },

    /// H.265 only: `parse_sps` standalone references a VPS id that
    /// wasn't seen. Does not fire from `parse_parameter_sets`.
    #[error("SPS references VPS id {vps_id} which was not in the input")]
    DanglingVpsReference { vps_id: u8 },

    /// Underlying engine returned an error that doesn't map cleanly to
    /// our enum. The string is for diagnostics only — consumers should
    /// not pattern-match on it.
    #[error("parser engine: {0}")]
    EngineError(String),

    /// LEB128-encoded value (AV1 OBU size, uvlc) walked past 8 bytes
    /// or had the continuation bit set on the 8th byte (per AV1 spec
    /// `Leb128()` algorithm).
    #[error("invalid LEB128 at byte {offset_bytes}")]
    InvalidLeb128 { offset_bytes: u32 },

    /// Audio frame parser found a buffer that does not start with the
    /// codec's expected sync word. `expected` and `found` are the actual
    /// sync-word values (12-bit values for MPEG audio / AAC ADTS).
    #[error("bad sync word: expected 0x{expected:03X}, found 0x{found:03X}")]
    BadSyncWord { expected: u16, found: u16 },

    /// Byte-oriented truncation. Frame parser ran short of bytes while
    /// reading a header or frame body. Distinct from `TruncatedRbsp`,
    /// which is bit-oriented for video RBSP walks.
    #[error("truncated: needed {needed} bytes, had {had}")]
    Truncated { needed: u32, had: u32 },

    /// Field carries an explicitly forbidden bit pattern per spec.
    /// Distinct from `ReservedValue` — `Forbidden` means the spec marks
    /// the value as never-valid; `ReservedValue` means the spec leaves
    /// it for future use.
    #[error("forbidden value in field '{field}'")]
    Forbidden { field: &'static str },

    /// MPEG audio header carries `bitrate_index == 0`, signaling
    /// "free-format" mode per ISO 11172-3 §2.4.2.3 Table 8 / ISO 13818-3
    /// Table 5: the frame length must be discovered by scanning for the
    /// next syncword rather than computed from the bitrate table. This
    /// parser does not implement next-syncword frame-length discovery;
    /// free-format streams are rare in modern encoders. Distinct from
    /// [`Self::ReservedValue`] so callers can distinguish "spec leaves
    /// this for future use" from "spec defines this but we don't decode
    /// it".
    ///
    /// `layer` is the decoded MPEG audio layer (1, 2, or 3) for diagnostics.
    #[error("unsupported free-format MPEG audio (layer {layer})")]
    UnsupportedFreeFormat { layer: u8 },

    /// `length_size` argument to Annex B ↔ length-prefixed NAL conversion
    /// ([`crate::codec::nal_framing`]) must be 1, 2, or 4 (the widths
    /// ISO/IEC 14496-15 AVCC/HVCC allow for `NALUnitLength`). Any other
    /// value is a caller programming error.
    #[error("invalid NAL length-prefix size {got} (must be 1, 2, or 4)")]
    InvalidLengthSize { got: u8 },

    /// A single NAL's byte length exceeds what `length_size` bytes can
    /// encode during Annex B → length-prefixed conversion
    /// ([`crate::codec::nal_framing`]). `nal_len` is saturated to
    /// `u32::MAX` for display if the true length somehow exceeds it.
    #[error("NAL length {nal_len} exceeds what a {length_size}-byte length prefix can encode")]
    NalLengthOverflow { nal_len: u32, length_size: u8 },

    /// A caller-supplied output buffer is shorter than the conversion
    /// needs. Raised only by the write-into-a-caller-buffer entry points
    /// ([`crate::codec::nal_framing::annexb_to_length_prefixed_into`]);
    /// the buffer is left untouched, and `needed` is the exact size that
    /// would have succeeded — the same number
    /// [`crate::codec::nal_framing::annexb_to_length_prefixed_len`]
    /// reports.
    ///
    /// `usize` rather than the byte counts elsewhere in this enum: these
    /// are Rust slice lengths, not bitstream fields, and this variant is
    /// Rust-only — the C ABI's two-call idiom signals the same condition
    /// as `TST_E_BUFFER_FULL` plus a `size_t` out-parameter.
    #[error("output buffer too small: needed {needed} bytes, have {have}")]
    BufferTooSmall { needed: usize, have: usize },
}

/// Read the ITU-T H.273 colour_primaries / transfer_characteristics /
/// matrix_coefficients triplet common to H.264 §E.2.1, H.265 §E.2.1,
/// and H.266 §7.3.2.5 / H.274 §7.2 VUI colour-description blocks.
///
/// All three fields are one byte wide (`u(8)`) and decoded via the shared
/// H.273 registry tables in this module. Callers read this helper inside
/// their `colour_description_present_flag` / `vui_colour_description_present_flag`
/// guard, then destructure the returned tuple into their local variables.
pub(crate) fn read_h273_colour(
    br: &mut bitreader::BitReader<'_>,
) -> Result<(ColourPrimaries, TransferCharacteristics, MatrixCoefficients), CodecParseError> {
    let primaries = ColourPrimaries::from_h273(br.read_u(8)? as u8);
    let transfer = TransferCharacteristics::from_h273(br.read_u(8)? as u8);
    let matrix = MatrixCoefficients::from_h273(br.read_u(8)? as u8);
    Ok((primaries, transfer, matrix))
}

/// Validate `bit_depth_*_minus8` per H.264 / H.265 / H.266: the normative
/// syntax range is `0..=8` (bit_depth ∈ 8..=16). A value greater than 8 is
/// out-of-spec (a malformed or fuzzed parameter set), not a real codec.
///
/// Returns `8 + value as u8` on success, [`CodecParseError::ReservedValue`]
/// otherwise. All paths (H.264 / H.265 / H.266) use the same hand-rolled check here.
// NOTE: `crates/tst-core/tests/tools/trace_h265_sps.rs` keeps an inlined
// copy of this helper for diagnostic-tool purposes. Keep in sync.
pub(crate) fn validate_bit_depth_minus8(
    field: &'static str,
    value: u32,
) -> Result<u8, CodecParseError> {
    if value > 8 {
        return Err(CodecParseError::ReservedValue { field, value });
    }
    Ok(8 + value as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_format_is_copy_eq() {
        let a = ChromaFormat::Yuv420;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn rational_basic_construction() {
        let r = Rational {
            num: 30000,
            den: 1001,
        };
        assert_eq!(r.num, 30000);
        assert_eq!(r.den, 1001);
    }

    #[test]
    fn colour_primaries_known_arms_round_trip() {
        assert_eq!(ColourPrimaries::from_h273(1), ColourPrimaries::Bt709);
        assert_eq!(ColourPrimaries::from_h273(9), ColourPrimaries::Bt2020);
        assert_eq!(ColourPrimaries::from_h273(2), ColourPrimaries::Unspecified);
    }

    #[test]
    fn colour_primaries_unknown_arm_preserves_byte() {
        assert_eq!(
            ColourPrimaries::from_h273(99),
            ColourPrimaries::Reserved(99)
        );
    }

    #[test]
    fn transfer_characteristics_pq_and_hlg() {
        assert_eq!(
            TransferCharacteristics::from_h273(16),
            TransferCharacteristics::SmpteSt2084
        );
        assert_eq!(
            TransferCharacteristics::from_h273(18),
            TransferCharacteristics::AribStdB67
        );
    }

    #[test]
    fn matrix_coefficients_bt2020() {
        assert_eq!(
            MatrixCoefficients::from_h273(9),
            MatrixCoefficients::Bt2020NonConstant
        );
        assert_eq!(
            MatrixCoefficients::from_h273(10),
            MatrixCoefficients::Bt2020Constant
        );
    }

    /// Per ITU-T H.273 V4 (07/2024) §8.3 Table 4 (PDF p.13), three matrix
    /// codepoints were added beyond what V3 covered:
    ///   15 — IPT-C2 (SMPTE IPT-PQ-C2)
    ///   16 — YCgCo-Re (YCgCo-R with even bit-depth offset)
    ///   17 — YCgCo-Ro (YCgCo-R with odd bit-depth offset)
    /// Codepoints 18-255 remain Reserved.
    #[test]
    fn matrix_coefficients_h273_v4_codepoints_15_16_17() {
        assert_eq!(MatrixCoefficients::from_h273(15), MatrixCoefficients::IptC2);
        assert_eq!(
            MatrixCoefficients::from_h273(16),
            MatrixCoefficients::YCgCoRe
        );
        assert_eq!(
            MatrixCoefficients::from_h273(17),
            MatrixCoefficients::YCgCoRo
        );
        // Boundary: 18 remains Reserved per V4 Table 4.
        assert_eq!(
            MatrixCoefficients::from_h273(18),
            MatrixCoefficients::Reserved(18)
        );
    }

    #[test]
    fn parse_error_displays_helpfully() {
        let e = CodecParseError::TruncatedRbsp {
            offset_bits: 80,
            needed_bits: 5,
        };
        let s = format!("{e}");
        assert!(s.contains("truncated"));
        assert!(s.contains("80"));
    }

    #[test]
    fn parse_error_is_std_error() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        let e = CodecParseError::EngineError("test".into());
        assert_error(&e);
    }

    #[test]
    fn parse_error_reserved_value_carries_field_name() {
        let e = CodecParseError::ReservedValue {
            field: "chroma_format_idc",
            value: 4,
        };
        let s = format!("{e}");
        assert!(s.contains("chroma_format_idc"));
    }

    #[test]
    fn parse_error_audio_variants_format() {
        let bad_sync = CodecParseError::BadSyncWord {
            expected: 0xFFF,
            found: 0xABC,
        };
        assert!(format!("{:?}", bad_sync).contains("BadSyncWord"));

        let trunc = CodecParseError::Truncated { needed: 7, had: 4 };
        assert!(format!("{:?}", trunc).contains("Truncated"));

        let forbidden = CodecParseError::Forbidden { field: "layer" };
        assert!(format!("{:?}", forbidden).contains("Forbidden"));
    }

    #[test]
    fn codec_parse_error_display_unchanged() {
        assert_eq!(
            CodecParseError::TruncatedRbsp {
                offset_bits: 80,
                needed_bits: 5,
            }
            .to_string(),
            "truncated RBSP at bit 80 (needed 5 more bits)"
        );
        assert_eq!(
            CodecParseError::InvalidGolomb { offset_bits: 32 }.to_string(),
            "invalid Exp-Golomb code at bit 32"
        );
        assert_eq!(
            CodecParseError::ReservedValue {
                field: "chroma_format_idc",
                value: 4,
            }
            .to_string(),
            "reserved value 4 in field 'chroma_format_idc'"
        );
        assert_eq!(
            CodecParseError::UnsupportedProfile { profile_idc: 200 }.to_string(),
            "unsupported profile_idc 200"
        );
        assert_eq!(
            CodecParseError::DanglingSpsReference { sps_id: 3 }.to_string(),
            "PPS references SPS id 3 which was not in the input"
        );
        assert_eq!(
            CodecParseError::DanglingVpsReference { vps_id: 1 }.to_string(),
            "SPS references VPS id 1 which was not in the input"
        );
        assert_eq!(
            CodecParseError::EngineError("test error".into()).to_string(),
            "parser engine: test error"
        );
        assert_eq!(
            CodecParseError::InvalidLeb128 { offset_bytes: 7 }.to_string(),
            "invalid LEB128 at byte 7"
        );
        assert_eq!(
            CodecParseError::BadSyncWord {
                expected: 0xFFF,
                found: 0xABC,
            }
            .to_string(),
            "bad sync word: expected 0xFFF, found 0xABC"
        );
        assert_eq!(
            CodecParseError::Truncated { needed: 7, had: 4 }.to_string(),
            "truncated: needed 7 bytes, had 4"
        );
        assert_eq!(
            CodecParseError::Forbidden { field: "layer" }.to_string(),
            "forbidden value in field 'layer'"
        );
        assert_eq!(
            CodecParseError::InvalidLengthSize { got: 3 }.to_string(),
            "invalid NAL length-prefix size 3 (must be 1, 2, or 4)"
        );
        assert_eq!(
            CodecParseError::NalLengthOverflow {
                nal_len: 257,
                length_size: 1,
            }
            .to_string(),
            "NAL length 257 exceeds what a 1-byte length prefix can encode"
        );
    }
}

/// Shared bit-building helpers for codec parser tests.
///
/// `pub(crate)` so that in-crate `#[cfg(test)]` modules in
/// `h264`, `h265`, `h266`, and `av1` submodules can share one
/// definition without each carrying a private copy.
#[cfg(test)]
pub(crate) mod test_util {
    extern crate alloc;
    use alloc::vec::Vec;

    /// MSB-first bit writer for constructing synthetic codec bitstreams
    /// in parser tests. Writes up to 64 bits per call; all `bw.write(N, k)`
    /// calls where `N` is an integer literal compile without cast since
    /// literals infer to `u64`.
    pub(crate) struct BitWriter {
        pub bytes: Vec<u8>,
        pub pos: u32,
    }

    impl BitWriter {
        pub(crate) fn new() -> Self {
            Self {
                bytes: Vec::new(),
                pos: 0,
            }
        }

        /// Write the lowest `n` bits of `value`, MSB first.
        pub(crate) fn write(&mut self, value: u64, n: u32) {
            for i in (0..n).rev() {
                let bit = ((value >> i) & 1) as u8;
                let byte_idx = (self.pos / 8) as usize;
                let bit_in_byte = 7 - (self.pos % 8);
                if byte_idx == self.bytes.len() {
                    self.bytes.push(0);
                }
                self.bytes[byte_idx] |= bit << bit_in_byte;
                self.pos += 1;
            }
        }

        /// Exp-Golomb ue(v) per H.264/H.265/H.266 §9.x.
        pub(crate) fn write_ue(&mut self, value: u32) {
            let v = value + 1;
            let leading_zeros = 31 - v.leading_zeros();
            for _ in 0..leading_zeros {
                self.write(0, 1);
            }
            self.write(v as u64, leading_zeros + 1);
        }

        /// rbsp_trailing_bits(): one '1' bit + zero-pad to byte boundary.
        pub(crate) fn end_rbsp(&mut self) {
            self.write(1, 1);
            while self.pos % 8 != 0 {
                self.write(0, 1);
            }
        }
    }
}
