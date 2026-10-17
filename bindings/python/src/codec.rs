//! tst-py — codec module.
//!
//! All codec PyO3 wraps for H.264/H.265/H.266/AV1/AAC/MPEG-2 audio,
//! plus shared types (ChromaFormat, Rational, ColorInfo, primaries/transfer/
//! matrix enums), plus NalUnit / Obu / ObuExtension typed wrappers used by
//! `Sample.payload`.

#![expect(
    clippy::disallowed_methods,
    reason = "in-memory parse/mux calls finish in bounded time and never park"
)]
// PyO3 0.22 + Rust 2024 edition: the #[pymethods] macro generates calls to
// internal unsafe functions inside unsafe fn bodies. The `unsafe_op_in_unsafe_fn`
// lint (now a warning in edition 2024) fires on macro-generated code; suppress
// here exactly as in the sibling modules (errors.rs, klv.rs, mpegts.rs, mux.rs).
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::codec::{
    ChromaFormat as RustChromaFormat, ColourPrimaries as RustColourPrimaries,
    MatrixCoefficients as RustMatrixCoefficients,
    TransferCharacteristics as RustTransferCharacteristics,
};

use crate::generated::codec::{
    AdtsFrame, Av1FrameHeaderLight, Av1ObuStream, Av1SequenceHeader, H264ParameterSets, H264Pps,
    H264SliceHeaderLight, H264Sps, H265ParameterSets, H265Pps, H265SliceHeaderLight, H265Sps,
    H265Vps, H266ParameterSets, H266Pps, H266SliceHeaderLight, H266Sps, H266Vps, MispTimestamp,
    Mpeg2AudioFrame, Obu,
};

// === Shared enums ===

/// Chroma subsampling format. Mirrors `tst_core::codec::ChromaFormat`.
#[pyclass(eq, eq_int, name = "ChromaFormat", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaFormatPy {
    #[pyo3(name = "MONOCHROME")]
    Monochrome,
    #[pyo3(name = "YUV420")]
    Yuv420,
    #[pyo3(name = "YUV422")]
    Yuv422,
    #[pyo3(name = "YUV444")]
    Yuv444,
    /// Catch-all for spec-reserved / future extension values.
    #[pyo3(name = "INVALID")]
    Invalid,
}

impl From<RustChromaFormat> for ChromaFormatPy {
    fn from(v: RustChromaFormat) -> Self {
        match v {
            RustChromaFormat::Monochrome => Self::Monochrome,
            RustChromaFormat::Yuv420 => Self::Yuv420,
            RustChromaFormat::Yuv422 => Self::Yuv422,
            RustChromaFormat::Yuv444 => Self::Yuv444,
        }
    }
}

/// ITU-T H.273 V4 §8.1 Table 2 — colour primaries.
/// Mirrors `tst_core::codec::ColourPrimaries`.
/// `Reserved(u8)` is collapsed to `RESERVED` (raw value not preserved in v1).
#[pyclass(eq, eq_int, name = "ColourPrimaries", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourPrimariesPy {
    #[pyo3(name = "BT709")]
    Bt709,
    #[pyo3(name = "UNSPECIFIED")]
    Unspecified,
    #[pyo3(name = "BT470M")]
    Bt470M,
    #[pyo3(name = "BT470BG")]
    Bt470Bg,
    #[pyo3(name = "SMPTE170M")]
    Smpte170M,
    #[pyo3(name = "SMPTE240M")]
    Smpte240M,
    #[pyo3(name = "FILM")]
    Film,
    #[pyo3(name = "BT2020")]
    Bt2020,
    #[pyo3(name = "SMPTE_ST428")]
    SmpteSt428,
    #[pyo3(name = "SMPTE_RP431_2")]
    SmpteRp431_2,
    #[pyo3(name = "SMPTE_EG432_1")]
    SmpteEg432_1,
    #[pyo3(name = "EBU3213E")]
    Ebu3213E,
    /// Spec-reserved or registry-extension value.
    #[pyo3(name = "RESERVED")]
    Reserved,
}

impl From<RustColourPrimaries> for ColourPrimariesPy {
    fn from(v: RustColourPrimaries) -> Self {
        match v {
            RustColourPrimaries::Bt709 => Self::Bt709,
            RustColourPrimaries::Unspecified => Self::Unspecified,
            RustColourPrimaries::Bt470M => Self::Bt470M,
            RustColourPrimaries::Bt470Bg => Self::Bt470Bg,
            RustColourPrimaries::Smpte170M => Self::Smpte170M,
            RustColourPrimaries::Smpte240M => Self::Smpte240M,
            RustColourPrimaries::Film => Self::Film,
            RustColourPrimaries::Bt2020 => Self::Bt2020,
            RustColourPrimaries::SmpteSt428 => Self::SmpteSt428,
            RustColourPrimaries::SmpteRp431_2 => Self::SmpteRp431_2,
            RustColourPrimaries::SmpteEg432_1 => Self::SmpteEg432_1,
            RustColourPrimaries::Ebu3213E => Self::Ebu3213E,
            // Reserved(u8) + any future #[non_exhaustive] variants.
            _ => Self::Reserved,
        }
    }
}

/// ITU-T H.273 V4 §8.2 Table 3 — transfer characteristics.
/// Mirrors `tst_core::codec::TransferCharacteristics`.
#[pyclass(eq, eq_int, name = "TransferCharacteristics", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCharacteristicsPy {
    #[pyo3(name = "BT709")]
    Bt709,
    #[pyo3(name = "UNSPECIFIED")]
    Unspecified,
    #[pyo3(name = "GAMMA22")]
    Gamma22,
    #[pyo3(name = "GAMMA28")]
    Gamma28,
    #[pyo3(name = "SMPTE170M")]
    Smpte170M,
    #[pyo3(name = "SMPTE240M")]
    Smpte240M,
    #[pyo3(name = "LINEAR")]
    Linear,
    #[pyo3(name = "LOG100")]
    Log100,
    #[pyo3(name = "LOG_SQRT")]
    LogSqrt,
    #[pyo3(name = "IEC61966_2_4")]
    Iec61966_2_4,
    #[pyo3(name = "BT1361E")]
    Bt1361E,
    #[pyo3(name = "IEC61966_2_1")]
    Iec61966_2_1,
    #[pyo3(name = "BT2020_BITS10")]
    Bt2020Bits10,
    #[pyo3(name = "BT2020_BITS12")]
    Bt2020Bits12,
    /// SMPTE ST 2084 — perceptual quantizer (HDR PQ).
    #[pyo3(name = "SMPTE_ST2084")]
    SmpteSt2084,
    #[pyo3(name = "SMPTE_ST428")]
    SmpteSt428,
    /// ARIB STD-B67 — hybrid log-gamma (HDR HLG).
    #[pyo3(name = "ARIB_STD_B67")]
    AribStdB67,
    /// Spec-reserved value.
    #[pyo3(name = "RESERVED")]
    Reserved,
}

impl From<RustTransferCharacteristics> for TransferCharacteristicsPy {
    fn from(v: RustTransferCharacteristics) -> Self {
        match v {
            RustTransferCharacteristics::Bt709 => Self::Bt709,
            RustTransferCharacteristics::Unspecified => Self::Unspecified,
            RustTransferCharacteristics::Gamma22 => Self::Gamma22,
            RustTransferCharacteristics::Gamma28 => Self::Gamma28,
            RustTransferCharacteristics::Smpte170M => Self::Smpte170M,
            RustTransferCharacteristics::Smpte240M => Self::Smpte240M,
            RustTransferCharacteristics::Linear => Self::Linear,
            RustTransferCharacteristics::Log100 => Self::Log100,
            RustTransferCharacteristics::LogSqrt => Self::LogSqrt,
            RustTransferCharacteristics::Iec61966_2_4 => Self::Iec61966_2_4,
            RustTransferCharacteristics::Bt1361E => Self::Bt1361E,
            RustTransferCharacteristics::Iec61966_2_1 => Self::Iec61966_2_1,
            RustTransferCharacteristics::Bt2020Bits10 => Self::Bt2020Bits10,
            RustTransferCharacteristics::Bt2020Bits12 => Self::Bt2020Bits12,
            RustTransferCharacteristics::SmpteSt2084 => Self::SmpteSt2084,
            RustTransferCharacteristics::SmpteSt428 => Self::SmpteSt428,
            RustTransferCharacteristics::AribStdB67 => Self::AribStdB67,
            // Reserved(u8) + any future #[non_exhaustive] variants.
            _ => Self::Reserved,
        }
    }
}

/// ITU-T H.273 V4 §8.3 Table 4 — matrix coefficients.
/// Mirrors `tst_core::codec::MatrixCoefficients`.
#[pyclass(eq, eq_int, name = "MatrixCoefficients", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixCoefficientsPy {
    #[pyo3(name = "IDENTITY")]
    Identity,
    #[pyo3(name = "BT709")]
    Bt709,
    #[pyo3(name = "UNSPECIFIED")]
    Unspecified,
    #[pyo3(name = "FCC_MC")]
    FccMc,
    #[pyo3(name = "BT470BG")]
    Bt470Bg,
    #[pyo3(name = "SMPTE170M")]
    Smpte170M,
    #[pyo3(name = "SMPTE240M")]
    Smpte240M,
    #[pyo3(name = "YCGCO")]
    YCgCo,
    #[pyo3(name = "BT2020_NON_CONSTANT")]
    Bt2020NonConstant,
    #[pyo3(name = "BT2020_CONSTANT")]
    Bt2020Constant,
    #[pyo3(name = "SMPTE_ST2085")]
    SmpteSt2085,
    #[pyo3(name = "CHROMA_DERIVED_NON_CONSTANT")]
    ChromaDerivedNonConstant,
    #[pyo3(name = "CHROMA_DERIVED_CONSTANT")]
    ChromaDerivedConstant,
    #[pyo3(name = "ICTCP")]
    IctCp,
    /// IPT-C2 (SMPTE IPT-PQ-C2). Added in H.273 V4.
    #[pyo3(name = "IPT_C2")]
    IptC2,
    /// YCgCo-Re — YCgCo-R with even bit-depth offset. Added in H.273 V4.
    #[pyo3(name = "YCGCO_RE")]
    YCgCoRe,
    /// YCgCo-Ro — YCgCo-R with odd bit-depth offset. Added in H.273 V4.
    #[pyo3(name = "YCGCO_RO")]
    YCgCoRo,
    /// Spec-reserved value.
    #[pyo3(name = "RESERVED")]
    Reserved,
}

impl From<RustMatrixCoefficients> for MatrixCoefficientsPy {
    fn from(v: RustMatrixCoefficients) -> Self {
        match v {
            RustMatrixCoefficients::Identity => Self::Identity,
            RustMatrixCoefficients::Bt709 => Self::Bt709,
            RustMatrixCoefficients::Unspecified => Self::Unspecified,
            RustMatrixCoefficients::FccMc => Self::FccMc,
            RustMatrixCoefficients::Bt470Bg => Self::Bt470Bg,
            RustMatrixCoefficients::Smpte170M => Self::Smpte170M,
            RustMatrixCoefficients::Smpte240M => Self::Smpte240M,
            RustMatrixCoefficients::YCgCo => Self::YCgCo,
            RustMatrixCoefficients::Bt2020NonConstant => Self::Bt2020NonConstant,
            RustMatrixCoefficients::Bt2020Constant => Self::Bt2020Constant,
            RustMatrixCoefficients::SmpteSt2085 => Self::SmpteSt2085,
            RustMatrixCoefficients::ChromaDerivedNonConstant => Self::ChromaDerivedNonConstant,
            RustMatrixCoefficients::ChromaDerivedConstant => Self::ChromaDerivedConstant,
            RustMatrixCoefficients::IctCp => Self::IctCp,
            RustMatrixCoefficients::IptC2 => Self::IptC2,
            RustMatrixCoefficients::YCgCoRe => Self::YCgCoRe,
            RustMatrixCoefficients::YCgCoRo => Self::YCgCoRo,
            // Reserved(u8) + any future #[non_exhaustive] variants.
            _ => Self::Reserved,
        }
    }
}

// === NalUnit (tagged union) ===

/// One H.264 / H.265 / H.266 NAL unit. Tagged with `kind` so Python
/// callers can pattern-match on the discriminant without needing a Rust
/// enum hierarchy.
///
/// Construct via the codec-specific static methods:
/// - `NalUnit.h264(nal_type, ref_idc, payload)`
/// - `NalUnit.h265(nal_type, layer_id, temporal_id_plus1, payload)`
/// - `NalUnit.h266(nal_type, layer_id, temporal_id_plus1, payload)`
#[pyclass(name = "NalUnit", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone)]
pub struct NalUnitPy {
    /// Codec discriminant — one of `"H264"`, `"H265"`, `"H266"`.
    pub kind: String,
    /// NAL unit type integer. Semantics depend on `kind`:
    /// H.264 §7.3.1 (5-bit), H.265 §7.3.1.2 (6-bit), H.266 §7.3.1.2 (5-bit).
    pub nal_type: u8,
    /// H.264 only — 2-bit `nal_ref_idc`. `None` for H.265 / H.266.
    pub ref_idc: Option<u8>,
    /// H.265 / H.266 only — `nuh_layer_id`. `None` for H.264.
    pub layer_id: Option<u8>,
    /// H.265 / H.266 only — `nuh_temporal_id_plus1`. `None` for H.264.
    pub temporal_id_plus1: Option<u8>,
    // RBSP payload bytes; exposed via getter returning PyBytes.
    pub payload: Vec<u8>,
}

#[pymethods]
impl NalUnitPy {
    /// Construct an H.264 NAL unit.
    ///
    /// `nal_type` is the 5-bit `nal_unit_type` from H.264 §7.3.1.
    /// `ref_idc` is the 2-bit `nal_ref_idc` from H.264 §7.3.1.
    /// `payload` carries RBSP bytes (Annex-B start codes stripped;
    /// emulation-prevention bytes preserved — consumer's decoder removes them).
    #[staticmethod]
    fn h264(nal_type: u8, ref_idc: u8, payload: Vec<u8>) -> Self {
        Self {
            kind: "H264".into(),
            nal_type,
            ref_idc: Some(ref_idc),
            layer_id: None,
            temporal_id_plus1: None,
            payload,
        }
    }

    /// Construct an H.265 NAL unit.
    ///
    /// `nal_type` is the 6-bit `nal_unit_type` from H.265 §7.3.1.2.
    /// `layer_id` is the 6-bit `nuh_layer_id`.
    /// `temporal_id_plus1` is the 3-bit `nuh_temporal_id_plus1`.
    #[staticmethod]
    fn h265(nal_type: u8, layer_id: u8, temporal_id_plus1: u8, payload: Vec<u8>) -> Self {
        Self {
            kind: "H265".into(),
            nal_type,
            ref_idc: None,
            layer_id: Some(layer_id),
            temporal_id_plus1: Some(temporal_id_plus1),
            payload,
        }
    }

    /// Construct an H.266 / VVC NAL unit.
    ///
    /// `nal_type` is the 5-bit `nal_unit_type` from H.266 V4 §7.3.1.2.
    /// `layer_id` is the 6-bit `nuh_layer_id`.
    /// `temporal_id_plus1` is the 3-bit `nuh_temporal_id_plus1`.
    #[staticmethod]
    fn h266(nal_type: u8, layer_id: u8, temporal_id_plus1: u8, payload: Vec<u8>) -> Self {
        Self {
            kind: "H266".into(),
            nal_type,
            ref_idc: None,
            layer_id: Some(layer_id),
            temporal_id_plus1: Some(temporal_id_plus1),
            payload,
        }
    }

    #[getter]
    fn kind(&self) -> &str {
        &self.kind
    }

    #[getter]
    fn nal_type(&self) -> u8 {
        self.nal_type
    }

    #[getter]
    fn ref_idc(&self) -> Option<u8> {
        self.ref_idc
    }

    #[getter]
    fn layer_id(&self) -> Option<u8> {
        self.layer_id
    }

    #[getter]
    fn temporal_id_plus1(&self) -> Option<u8> {
        self.temporal_id_plus1
    }

    /// RBSP payload bytes. Annex-B start codes stripped; emulation-prevention
    /// bytes preserved (consumer's decoder removes 0x03 escapes).
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.payload)
    }

    fn __repr__(&self) -> String {
        format!(
            "NalUnit(kind='{}', nal_type={}, payload_len={})",
            self.kind,
            self.nal_type,
            self.payload.len()
        )
    }
}

// Crate-internal Rust constructors for NalUnitPy — separate impl block
// so they don't appear as #[pymethods] (which would make them Python-callable).
impl NalUnitPy {
    pub(crate) fn make_h264(nal_type: u8, ref_idc: u8, payload: Vec<u8>) -> Self {
        Self {
            kind: "H264".into(),
            nal_type,
            ref_idc: Some(ref_idc),
            layer_id: None,
            temporal_id_plus1: None,
            payload,
        }
    }

    pub(crate) fn make_h265(
        nal_type: u8,
        layer_id: u8,
        temporal_id_plus1: u8,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind: "H265".into(),
            nal_type,
            ref_idc: None,
            layer_id: Some(layer_id),
            temporal_id_plus1: Some(temporal_id_plus1),
            payload,
        }
    }

    pub(crate) fn make_h266(
        nal_type: u8,
        layer_id: u8,
        temporal_id_plus1: u8,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind: "H266".into(),
            nal_type,
            ref_idc: None,
            layer_id: Some(layer_id),
            temporal_id_plus1: Some(temporal_id_plus1),
            payload,
        }
    }
}

// === H.264 ===

use tst_core::codec::h264::{
    EntropyCodingMode as RustEntropyCodingMode, H264SliceType as RustH264SliceType,
    parse_parameter_sets as rust_parse_h264_parameter_sets, parse_pps as rust_parse_h264_pps,
    parse_slice_header_light as rust_parse_h264_slice_header_light,
    parse_sps as rust_parse_h264_sps,
};

/// H.264 entropy coding mode signalled in the PPS.
/// Mirrors `tst_core::codec::h264::EntropyCodingMode`.
#[pyclass(eq, eq_int, name = "EntropyCodingMode", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyCodingModePy {
    /// Context-Adaptive Variable Length Coding (Baseline/Main profiles).
    #[pyo3(name = "CAVLC")]
    Cavlc,
    /// Context-Adaptive Binary Arithmetic Coding (Main/High profiles).
    #[pyo3(name = "CABAC")]
    Cabac,
}

impl From<RustEntropyCodingMode> for EntropyCodingModePy {
    fn from(v: RustEntropyCodingMode) -> Self {
        match v {
            RustEntropyCodingMode::Cavlc => Self::Cavlc,
            RustEntropyCodingMode::Cabac => Self::Cabac,
        }
    }
}

/// H.264 slice type, normalised via `slice_type % 5` per H.264 §7.4.3.
/// Mirrors `tst_core::codec::h264::H264SliceType`.
#[pyclass(eq, eq_int, name = "H264SliceType", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264SliceTypePy {
    /// P slice — predicted.
    #[pyo3(name = "P")]
    P,
    /// B slice — bidirectionally predicted.
    #[pyo3(name = "B")]
    B,
    /// I slice — intra-coded.
    #[pyo3(name = "I")]
    I,
    /// SP slice — switching P.
    #[pyo3(name = "Sp")]
    Sp,
    /// SI slice — switching I.
    #[pyo3(name = "Si")]
    Si,
    /// Unknown slice type — returned when the Rust parser produces a
    /// `#[non_exhaustive]` variant not yet mapped to a Python constant.
    #[pyo3(name = "Unknown")]
    Unknown,
}

impl From<RustH264SliceType> for H264SliceTypePy {
    fn from(v: RustH264SliceType) -> Self {
        match v {
            RustH264SliceType::P => Self::P,
            RustH264SliceType::B => Self::B,
            RustH264SliceType::I => Self::I,
            RustH264SliceType::Sp => Self::Sp,
            RustH264SliceType::Si => Self::Si,
            // #[non_exhaustive] catch-all — maps any future variant to Unknown
            // rather than mis-classifying it as intra (I) which would cause callers
            // to treat an unrecognised slice type as a keyframe indicator.
            _ => Self::Unknown,
        }
    }
}

/// Parse a single H.264 SPS RBSP.
///
/// `rbsp` must be the raw RBSP body — Annex B start code stripped, NAL header
/// byte stripped, emulation-prevention bytes preserved (matches
/// ``NalUnit.h264(...).payload``).
///
/// Raises `CodecError` with ``kind=CodecErrorKind.TRUNCATED_RBSP`` for empty
/// input; ``kind=CodecErrorKind.ENGINE_ERROR`` for unparseable bitstreams.
#[pyfunction]
#[pyo3(name = "parse_h264_sps")]
fn parse_h264_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H264Sps> {
    rust_parse_h264_sps(rbsp)
        .map(|r| H264Sps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

/// Parse a single H.264 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h264_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h264_pps")]
fn parse_h264_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H264Pps> {
    rust_parse_h264_pps(rbsp)
        .map(|r| H264Pps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

/// Parse all H.264 SPS and PPS NAL units from a list of `NalUnit` objects.
///
/// Non-H.264 NAL units in the list are silently skipped. Non-SPS/PPS H.264
/// NAL units are also skipped. Parse is partial-success-tolerant: bad
/// individual parameter-set NALs emit a warning and are skipped.
///
/// Raises `CodecError` only when every parameter-set NAL in the input
/// failed to parse.
#[pyfunction]
#[pyo3(name = "parse_h264_parameter_sets")]
fn parse_h264_parameter_sets_py(
    py: Python<'_>,
    nals: Vec<PyRef<'_, NalUnitPy>>,
) -> PyResult<H264ParameterSets> {
    use tst_core::codec::NalUnit as RustNalUnit;
    // Convert each NalUnitPy to the Rust NalUnit::H264 variant.
    // Non-H.264 entries (H265, H266) are silently filtered out — the Rust
    // parse_parameter_sets fn already ignores non-H264 variants, but filtering
    // here avoids allocating dummy payloads for non-H264 discriminants.
    let rust_nals: Vec<RustNalUnit> = nals
        .iter()
        .filter_map(|n| {
            if n.kind != "H264" {
                return None;
            }
            Some(RustNalUnit::H264 {
                nal_type: n.nal_type,
                ref_idc: n.ref_idc.unwrap_or(3),
                payload: n.payload.clone().into(),
            })
        })
        .collect();
    rust_parse_h264_parameter_sets(&rust_nals)
        .map(|r| H264ParameterSets::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

/// Parse a light H.264 slice header from a RBSP byte slice.
///
/// `rbsp` carries the RBSP body of a slice NAL — Annex B start code and NAL
/// header byte stripped, emulation-prevention bytes preserved.
///
/// `sps` is optional SPS context — when supplied, `frame_num` is read from
/// the bitstream using the bit width `log2_max_frame_num_minus4 + 4`. When
/// `None`, `H264SliceHeaderLight.frame_num` is `None`.
///
/// `nal_unit_type` is the 5-bit NAL type from the NAL header (`& 0x1F`) —
/// used to derive `H264SliceHeaderLight.idr` (`== 5`) without re-parsing.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h264_slice_header_light", signature = (rbsp, sps, nal_unit_type))]
fn parse_h264_slice_header_light_py(
    py: Python<'_>,
    rbsp: &[u8],
    sps: Option<&H264Sps>,
    nal_unit_type: u8,
) -> PyResult<H264SliceHeaderLight> {
    // The generated SPS keeps its raw bytes, not the Rust record: parse them
    // again for the context (the parse is deterministic).
    let ctx = sps
        .map(|s| rust_parse_h264_sps(&s.raw_rbsp.0))
        .transpose()
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))?;
    rust_parse_h264_slice_header_light(rbsp, ctx.as_ref(), nal_unit_type)
        .map(|r| H264SliceHeaderLight::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

// === H.265 ===

use tst_core::codec::h265::{
    H265SliceType as RustH265SliceType, parse_parameter_sets as rust_parse_h265_parameter_sets,
    parse_pps as rust_parse_h265_pps,
    parse_slice_header_light as rust_parse_h265_slice_header_light,
    parse_sps as rust_parse_h265_sps, parse_vps as rust_parse_h265_vps,
};

/// H.265 slice type (B / P / I). Only three values are defined by H.265
/// §7.4.7.1 Table 7-7.
/// Mirrors `tst_core::codec::h265::H265SliceType`.
#[pyclass(eq, eq_int, name = "H265SliceType", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H265SliceTypePy {
    /// B slice — bidirectionally predicted.
    #[pyo3(name = "B")]
    B,
    /// P slice — predicted.
    #[pyo3(name = "P")]
    P,
    /// I slice — intra-coded.
    #[pyo3(name = "I")]
    I,
    /// Unknown slice type — returned when the Rust parser produces a
    /// `#[non_exhaustive]` variant not yet mapped to a Python constant.
    #[pyo3(name = "Unknown")]
    Unknown,
}

impl From<RustH265SliceType> for H265SliceTypePy {
    fn from(v: RustH265SliceType) -> Self {
        match v {
            RustH265SliceType::B => Self::B,
            RustH265SliceType::P => Self::P,
            RustH265SliceType::I => Self::I,
            // #[non_exhaustive] catch-all — maps any future variant to Unknown
            // rather than mis-classifying it as intra (I) which would cause callers
            // to treat an unrecognised slice type as a keyframe indicator.
            _ => Self::Unknown,
        }
    }
}

/// Parse a single H.265 SPS RBSP.
///
/// `rbsp` must be the raw RBSP body — Annex B start code stripped, NAL header
/// (2 bytes for H.265) stripped, emulation-prevention bytes preserved (matches
/// ``NalUnit.h265(...).payload``).
///
/// Raises `CodecError` with ``kind=CodecErrorKind.TRUNCATED_RBSP`` for empty
/// input; ``kind=CodecErrorKind.ENGINE_ERROR`` for unparseable bitstreams.
#[pyfunction]
#[pyo3(name = "parse_h265_sps")]
fn parse_h265_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265Sps> {
    rust_parse_h265_sps(rbsp)
        .map(|r| H265Sps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse a single H.265 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h265_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h265_pps")]
fn parse_h265_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265Pps> {
    rust_parse_h265_pps(rbsp)
        .map(|r| H265Pps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse a single H.265 VPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h265_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h265_vps")]
fn parse_h265_vps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265Vps> {
    rust_parse_h265_vps(rbsp)
        .map(|r| H265Vps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse all H.265 VPS, SPS, and PPS NAL units from a list of `NalUnit` objects.
///
/// Non-H.265 NAL units in the list are silently skipped. Parse is
/// partial-success-tolerant: bad individual parameter-set NALs emit a warning
/// and are skipped.
///
/// Raises `CodecError` only when every parameter-set NAL in the input
/// failed to parse.
#[pyfunction]
#[pyo3(name = "parse_h265_parameter_sets")]
fn parse_h265_parameter_sets_py(
    py: Python<'_>,
    nals: Vec<PyRef<'_, NalUnitPy>>,
) -> PyResult<H265ParameterSets> {
    use tst_core::codec::NalUnit as RustNalUnit;
    // Convert each NalUnitPy to the Rust NalUnit::H265 variant.
    // Non-H265 entries are silently filtered out.
    let rust_nals: Vec<RustNalUnit> = nals
        .iter()
        .filter_map(|n| {
            if n.kind != "H265" {
                return None;
            }
            Some(RustNalUnit::H265 {
                nal_type: n.nal_type,
                layer_id: n.layer_id.unwrap_or(0),
                temporal_id_plus1: n.temporal_id_plus1.unwrap_or(1),
                payload: n.payload.clone().into(),
            })
        })
        .collect();
    rust_parse_h265_parameter_sets(&rust_nals)
        .map(|r| H265ParameterSets::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse a light H.265 slice segment header from a RBSP byte slice.
///
/// `rbsp` carries the RBSP body of a slice NAL — Annex B start code and NAL
/// header (2 bytes) stripped, emulation-prevention bytes preserved.
///
/// `sps` is optional SPS context — when supplied, `pic_order_cnt_lsb` is
/// read from the bitstream using the bit width
/// `log2_max_pic_order_cnt_lsb_minus4 + 4`. When `None`,
/// `H265SliceHeaderLight.pic_order_cnt_lsb` is `None`.
///
/// `nal_unit_type` is the 6-bit NAL type from the NAL header
/// — used to derive `idr` (IDR_W_RADL=19 or IDR_N_LP=20) and to gate
/// IRAP-specific fields.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h265_slice_header_light", signature = (rbsp, sps, nal_unit_type))]
fn parse_h265_slice_header_light_py(
    py: Python<'_>,
    rbsp: &[u8],
    sps: Option<&H265Sps>,
    nal_unit_type: u8,
) -> PyResult<H265SliceHeaderLight> {
    // The generated SPS keeps its raw bytes, not the Rust record: parse them
    // again for the context (the parse is deterministic).
    let ctx = sps
        .map(|s| rust_parse_h265_sps(&s.raw_rbsp.0))
        .transpose()
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))?;
    rust_parse_h265_slice_header_light(rbsp, ctx.as_ref(), nal_unit_type)
        .map(|r| H265SliceHeaderLight::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

// === H.266 ===

use tst_core::codec::h266::{
    H266SliceType as RustH266SliceType, parse_parameter_sets as rust_parse_h266_parameter_sets,
    parse_pps as rust_parse_h266_pps,
    parse_slice_header_light as rust_parse_h266_slice_header_light,
    parse_sps as rust_parse_h266_sps, parse_vps as rust_parse_h266_vps,
};

/// H.266 slice type (B / P / I). Only three values are defined by H.266 V4
/// §7.4.8 Table 9.
/// Mirrors `tst_core::codec::h266::H266SliceType`.
#[pyclass(eq, eq_int, name = "H266SliceType", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H266SliceTypePy {
    /// B slice — bidirectionally predicted.
    #[pyo3(name = "B")]
    B,
    /// P slice — predicted.
    #[pyo3(name = "P")]
    P,
    /// I slice — intra-coded.
    #[pyo3(name = "I")]
    I,
    /// Unknown slice type — returned when the Rust parser produces a
    /// `#[non_exhaustive]` variant not yet mapped to a Python constant.
    #[pyo3(name = "Unknown")]
    Unknown,
}

impl From<RustH266SliceType> for H266SliceTypePy {
    fn from(v: RustH266SliceType) -> Self {
        match v {
            RustH266SliceType::B => Self::B,
            RustH266SliceType::P => Self::P,
            RustH266SliceType::I => Self::I,
            // #[non_exhaustive] catch-all — maps any future variant to Unknown
            // rather than mis-classifying it as intra (I) which would cause callers
            // to treat an unrecognised slice type as a keyframe indicator.
            _ => Self::Unknown,
        }
    }
}

/// Parse a single H.266 SPS RBSP.
///
/// `rbsp` must be the raw RBSP body — Annex B start code stripped, NAL header
/// (2 bytes for H.266) stripped, emulation-prevention bytes preserved (matches
/// ``NalUnit.h266(...).payload``).
///
/// Raises `CodecError` with ``kind=CodecErrorKind.TRUNCATED_RBSP`` for empty
/// input; ``kind=CodecErrorKind.ENGINE_ERROR`` for unparseable bitstreams.
#[pyfunction]
#[pyo3(name = "parse_h266_sps")]
fn parse_h266_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266Sps> {
    rust_parse_h266_sps(rbsp)
        .map(|r| H266Sps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse a single H.266 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h266_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h266_pps")]
fn parse_h266_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266Pps> {
    rust_parse_h266_pps(rbsp)
        .map(|r| H266Pps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse a single H.266 VPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h266_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h266_vps")]
fn parse_h266_vps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266Vps> {
    rust_parse_h266_vps(rbsp)
        .map(|r| H266Vps::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse all H.266 VPS, SPS, and PPS NAL units from a list of `NalUnit` objects.
///
/// Non-H.266 NAL units in the list are silently skipped. Parse is
/// partial-success-tolerant: bad individual parameter-set NALs emit a warning
/// and are skipped.
///
/// Raises `CodecError` only when every parameter-set NAL in the input
/// failed to parse.
#[pyfunction]
#[pyo3(name = "parse_h266_parameter_sets")]
fn parse_h266_parameter_sets_py(
    py: Python<'_>,
    nals: Vec<PyRef<'_, NalUnitPy>>,
) -> PyResult<H266ParameterSets> {
    use tst_core::codec::NalUnit as RustNalUnit;
    // Convert each NalUnitPy to the Rust NalUnit::H266 variant.
    // Non-H266 entries (H264, H265) are silently filtered out.
    let rust_nals: Vec<RustNalUnit> = nals
        .iter()
        .filter_map(|n| {
            if n.kind != "H266" {
                return None;
            }
            Some(RustNalUnit::H266 {
                nal_type: n.nal_type,
                layer_id: n.layer_id.unwrap_or(0),
                temporal_id_plus1: n.temporal_id_plus1.unwrap_or(1),
                payload: n.payload.clone().into(),
            })
        })
        .collect();
    rust_parse_h266_parameter_sets(&rust_nals)
        .map(|r| H266ParameterSets::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse a light H.266 slice header from a RBSP byte slice.
///
/// `rbsp` carries the RBSP body of a slice NAL — Annex B start code and NAL
/// header (2 bytes) stripped, emulation-prevention bytes preserved.
///
/// `sps` is optional SPS context — accepted for API symmetry with the H.264 /
/// H.265 counterparts, but currently unused. Pass ``None`` in all cases.
///
/// `nal_unit_type` is the 5-bit NAL type from the H.266 NAL header — used to
/// derive `idr` (IDR_W_RADL=7 or IDR_N_LP=8 per H.266 V4 Table 5) and
/// `pic_order_cnt_lsb` for IDR slices.
///
/// # Sentinel values
///
/// ``H266SliceHeaderLight.slice_type`` always returns ``H266SliceType.I``
/// and ``H266SliceHeaderLight.pps_id`` always returns ``0`` regardless of
/// the bitstream content. Accurate extraction requires walking
/// ``picture_header_rbsp()``, whose length is governed by SPS / PPS context
/// fields that this light parser does not carry. Only ``idr``,
/// ``first_in_pic``, and ``pic_order_cnt_lsb`` (for IDR slices) are accurate.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h266_slice_header_light", signature = (rbsp, sps, nal_unit_type))]
fn parse_h266_slice_header_light_py(
    py: Python<'_>,
    rbsp: &[u8],
    sps: Option<&H266Sps>,
    nal_unit_type: u8,
) -> PyResult<H266SliceHeaderLight> {
    // The generated SPS keeps its raw bytes, not the Rust record: parse them
    // again for the context (the parse is deterministic).
    let ctx = sps
        .map(|s| rust_parse_h266_sps(&s.raw_rbsp.0))
        .transpose()
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))?;
    rust_parse_h266_slice_header_light(rbsp, ctx.as_ref(), nal_unit_type)
        .map(|r| H266SliceHeaderLight::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

// === AV1 ===

use tst_core::codec::av1::{
    parse_frame_header_light as rust_parse_av1_frame_header_light,
    parse_obu_stream as rust_parse_av1_obu_stream,
    parse_sequence_header as rust_parse_av1_sequence_header,
};
use tst_core::codec::{Obu as RustObu, ObuExtension as RustObuExtension};

/// Parse an AV1 Sequence Header OBU body.
///
/// `payload` carries the OBU body bytes — the OBU header byte and any
/// LEB128 `obu_size` prefix are stripped before calling this function
/// (as `Obu.payload` provides from a demuxed stream).
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_av1_sequence_header")]
fn parse_av1_sequence_header_py(py: Python<'_>, payload: &[u8]) -> PyResult<Av1SequenceHeader> {
    rust_parse_av1_sequence_header(payload)
        .map(|r| Av1SequenceHeader::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "av1"))
}

/// Parse a light AV1 Frame Header OBU body.
///
/// `payload` carries the OBU body bytes. `seq` is the Sequence Header
/// context required by the AV1 parser — it must correspond to the
/// Sequence Header that precedes this Frame Header in the bitstream.
/// Use `parse_av1_sequence_header` to obtain `seq`.
///
/// Light scope: extracts `frame_type` + `show_frame` +
/// `show_existing_frame` only. `frame_size` is always `None`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_av1_frame_header_light")]
fn parse_av1_frame_header_light_py(
    py: Python<'_>,
    payload: &[u8],
    seq: &Av1SequenceHeader,
) -> PyResult<Av1FrameHeaderLight> {
    // The generated sequence header keeps its raw bytes, not the Rust record:
    // parse them again for the context (the parse is deterministic).
    let ctx = rust_parse_av1_sequence_header(&seq.raw.0)
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "av1"))?;
    rust_parse_av1_frame_header_light(payload, &ctx)
        .map(|r| Av1FrameHeaderLight::from(&r))
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "av1"))
}

/// Walk a list of `Obu` objects and collect typed AV1 structs.
///
/// Partial-success-tolerant: OBUs that fail to parse accumulate in
/// `Av1ObuStream.unparseable` rather than aborting the walk. Skips
/// TemporalDelimiter, TileGroup, Metadata, TileList, and Padding OBUs
/// silently (they carry no metadata for this parser).
///
/// This function never raises — errors appear in `Av1ObuStream.unparseable`.
#[pyfunction]
#[pyo3(name = "parse_av1_obu_stream")]
fn parse_av1_obu_stream_py(obus: Vec<PyRef<'_, Obu>>) -> Av1ObuStream {
    let rust_obus: Vec<RustObu> = obus
        .iter()
        .map(|o| RustObu {
            obu_type: o.obu_type,
            extension: o.extension.map(|ext| RustObuExtension {
                temporal_id: ext.temporal_id,
                spatial_id: ext.spatial_id,
            }),
            payload: o.payload.0.clone().into(),
        })
        .collect();
    Av1ObuStream::from(&rust_parse_av1_obu_stream(&rust_obus))
}

// === AAC ===

use tst_core::codec::aac::{
    AacChannelLayout as RustAacChannelLayout, AacProfile as RustAacProfile,
    AdtsFrameOwned as RustAdtsFrameOwned, MpegVersion as RustMpegVersion,
    frames as rust_aac_frames, frames_with_resync as rust_aac_frames_with_resync,
};

/// AAC profile per ADTS ISO/IEC 13818-7 §1.A.
/// Mirrors `tst_core::codec::aac::AacProfile` (exhaustive — 4 spec-defined values).
#[pyclass(eq, eq_int, name = "AacProfile", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AacProfilePy {
    #[pyo3(name = "MAIN")]
    Main,
    #[pyo3(name = "LC")]
    Lc,
    #[pyo3(name = "SSR")]
    Ssr,
    #[pyo3(name = "LTP")]
    Ltp,
}

impl From<RustAacProfile> for AacProfilePy {
    fn from(v: RustAacProfile) -> Self {
        match v {
            RustAacProfile::Main => Self::Main,
            RustAacProfile::Lc => Self::Lc,
            RustAacProfile::Ssr => Self::Ssr,
            RustAacProfile::LongTermPrediction => Self::Ltp,
        }
    }
}

/// ADTS MPEG version bit.
/// Mirrors `tst_core::codec::aac::MpegVersion` (exhaustive — 2 values).
#[pyclass(eq, eq_int, name = "MpegVersion", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersionPy {
    #[pyo3(name = "MPEG2")]
    Mpeg2,
    #[pyo3(name = "MPEG4")]
    Mpeg4,
}

impl From<RustMpegVersion> for MpegVersionPy {
    fn from(v: RustMpegVersion) -> Self {
        match v {
            RustMpegVersion::Mpeg2 => Self::Mpeg2,
            RustMpegVersion::Mpeg4 => Self::Mpeg4,
        }
    }
}

/// AAC channel layout decoded from the ADTS `channel_configuration` field.
///
/// Mirrors `tst_core::codec::aac::AacChannelLayout` (`#[non_exhaustive]`).
/// Two cases:
/// - `is_pce_defined == True` — channel layout is in a PCE inside the raw
///   data block; `channels` is `None`.
/// - `is_pce_defined == False` — canonical channel count; `channels` is `Some(n)`.
#[pyclass(name = "AacChannelLayout", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct AacChannelLayoutPy {
    is_pce_defined: bool,
    channels: Option<u8>,
}

#[pymethods]
impl AacChannelLayoutPy {
    /// True when the channel layout is defined by a Program Config Element
    /// (PCE) inside the raw_data_block — not derivable from the ADTS header.
    #[getter]
    fn is_pce_defined(&self) -> bool {
        self.is_pce_defined
    }

    /// Canonical channel count, or `None` when PCE-defined.
    #[getter]
    fn channels(&self) -> Option<u8> {
        self.channels
    }

    fn __repr__(&self) -> String {
        if self.is_pce_defined {
            "AacChannelLayout(pce_defined)".to_owned()
        } else {
            format!("AacChannelLayout(channels={})", self.channels.unwrap_or(0))
        }
    }
}

impl From<RustAacChannelLayout> for AacChannelLayoutPy {
    fn from(v: RustAacChannelLayout) -> Self {
        match v {
            RustAacChannelLayout::PceDefined => Self {
                is_pce_defined: true,
                channels: None,
            },
            RustAacChannelLayout::Channels(n) => Self {
                is_pce_defined: false,
                channels: Some(n),
            },
            // #[non_exhaustive] catch-all — forward-compat for future variants.
            _ => Self {
                is_pce_defined: false,
                channels: None,
            },
        }
    }
}

/// Lazy ADTS frame iterator returned by `iter_aac_frames` /
/// `iter_aac_frames_with_resync`.
///
/// Implementation: frames are collected eagerly at construction time into a
/// `Vec<AdtsFrameOwned>` — the lifetime-borrowed `AdtsFrames<'_>` iterator
/// cannot cross the PyO3 boundary. From Python's perspective the type has
/// the standard iterator protocol (`__iter__` + `__next__`).
#[pyclass(name = "AdtsFrameIter", module = "tstrans.codec")]
pub struct AdtsFrameIterPy {
    frames: Vec<RustAdtsFrameOwned>,
    index: usize,
}

#[pymethods]
impl AdtsFrameIterPy {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<AdtsFrame> {
        if self.index < self.frames.len() {
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Some(AdtsFrame::from(&frame))
        } else {
            None
        }
    }
}

/// Returns an iterator over ADTS frames parsed from `bytes_buf` (strict —
/// raises `CodecError` on first parse failure). **Eager:** all frames are
/// collected upfront into a `Vec`; the returned object iterates that `Vec`,
/// not a true streaming parser. Memory usage is O(num_frames); peak
/// allocation occurs at construction. For very large buffers, prefer
/// chunked input or process in segments.
///
/// Internally drives the Rust `tst_core::codec::aac::frames` iterator at
/// construction time. The returned `AdtsFrameIter` yields `AdtsFrame`
/// objects one at a time from the pre-built `Vec`.
///
/// Raises `CodecError` immediately if any frame fails to parse.
#[pyfunction]
#[pyo3(name = "iter_aac_frames")]
fn iter_aac_frames_py(py: Python<'_>, bytes_buf: &[u8]) -> PyResult<AdtsFrameIterPy> {
    // GIL release: the eager collect is the heavy
    // work — it scans `bytes_buf` and copies every frame into an owned
    // Vec. `bytes_buf` borrows from a `Py<PyBytes>` held by the caller's
    // frame, safe to access without the GIL. Per-iter `__next__` stays
    // inside the GIL (overhead of release per element exceeds benefit).
    let frames = py.allow_threads(|| {
        rust_aac_frames(bytes_buf)
            .map(|res| res.map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()
    });
    let frames = frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "aac"))?;
    Ok(AdtsFrameIterPy { frames, index: 0 })
}

/// Returns an iterator over ADTS frames parsed from `bytes_buf`
/// (best-effort — never raises, silently skips frames that fail to parse).
/// **Eager:** all frames are collected upfront into a `Vec`; the returned
/// object iterates that `Vec`, not a true streaming parser. Memory usage
/// is O(num_frames); peak allocation occurs at construction. For very
/// large buffers, prefer chunked input or process in segments.
///
/// Uses `tst_core::codec::aac::frames_with_resync` which scans forward for
/// the next plausible ADTS syncword after each parse error. Errors are
/// filtered out; only successfully decoded frames are yielded.
#[pyfunction]
#[pyo3(name = "iter_aac_frames_with_resync")]
fn iter_aac_frames_with_resync_py(py: Python<'_>, bytes_buf: &[u8]) -> AdtsFrameIterPy {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_aac_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    AdtsFrameIterPy { frames, index: 0 }
}

/// Eagerly parse all ADTS frames from `bytes_buf` (strict — raises
/// `CodecError` on first parse failure).
///
/// Returns a `list[AdtsFrame]` on success. Equivalent to
/// `list(iter_aac_frames(bytes_buf))` but avoids the iterator object.
#[pyfunction]
#[pyo3(name = "parse_aac_frames")]
fn parse_aac_frames_py(py: Python<'_>, bytes_buf: &[u8]) -> PyResult<Vec<AdtsFrame>> {
    // GIL-release rationale: see `iter_aac_frames_py`. Parse + own-copy
    // off-thread, then map errors to PyErr inside the GIL (cheap).
    let frames = py.allow_threads(|| {
        rust_aac_frames(bytes_buf)
            .map(|res| res.map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()
    });
    let frames = frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "aac"))?;
    Ok(frames.into_iter().map(|r| AdtsFrame::from(&r)).collect())
}

/// Eagerly parse all ADTS frames from `bytes_buf`, skipping parse errors
/// (best-effort — never raises).
///
/// Uses `frames_with_resync` internally; only successfully decoded frames
/// appear in the returned list. Suitable for stats / telemetry use where
/// dropping a frame on corruption is preferable to aborting the parse.
#[pyfunction]
#[pyo3(name = "parse_aac_frames_with_resync")]
fn parse_aac_frames_with_resync_py(py: Python<'_>, bytes_buf: &[u8]) -> Vec<AdtsFrame> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_aac_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    frames.into_iter().map(|r| AdtsFrame::from(&r)).collect()
}

// === MPEG-2 audio ===

use tst_core::codec::mpegaudio::{
    ChannelMode as RustChannelMode, FrameOwned as RustMpeg2AudioFrameOwned, Layer as RustLayer,
    Version as RustVersion, frames as rust_mpeg2audio_frames,
    frames_with_resync as rust_mpeg2audio_frames_with_resync,
};

/// MPEG audio layer (I, II, or III).
/// Mirrors `tst_core::codec::mpegaudio::Layer` (exhaustive — 3 spec-defined values).
#[pyclass(eq, eq_int, name = "Layer", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerPy {
    #[pyo3(name = "I")]
    I,
    #[pyo3(name = "II")]
    Ii,
    #[pyo3(name = "III")]
    Iii,
}

impl From<RustLayer> for LayerPy {
    fn from(v: RustLayer) -> Self {
        match v {
            RustLayer::I => Self::I,
            RustLayer::II => Self::Ii,
            RustLayer::III => Self::Iii,
        }
    }
}

/// MPEG audio version (MPEG-1, MPEG-2, or MPEG-2.5).
///
/// MPEG-2.5 is the de-facto half-rate extension (8/11.025/12 kHz Layer III);
/// not part of any ratified ISO spec but ubiquitous in consumer MP3 streams.
/// Mirrors `tst_core::codec::mpegaudio::Version` (exhaustive — 3 values).
#[pyclass(eq, eq_int, name = "Version", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionPy {
    #[pyo3(name = "MPEG1")]
    Mpeg1,
    #[pyo3(name = "MPEG2")]
    Mpeg2,
    #[pyo3(name = "MPEG2_5")]
    Mpeg2_5,
}

impl From<RustVersion> for VersionPy {
    fn from(v: RustVersion) -> Self {
        match v {
            RustVersion::Mpeg1 => Self::Mpeg1,
            RustVersion::Mpeg2 => Self::Mpeg2,
            RustVersion::Mpeg2_5 => Self::Mpeg2_5,
        }
    }
}

/// MPEG audio channel mode (header bits 25-26).
/// Mirrors `tst_core::codec::mpegaudio::ChannelMode` (exhaustive — 4 values).
#[pyclass(eq, eq_int, name = "ChannelMode", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelModePy {
    #[pyo3(name = "STEREO")]
    Stereo,
    #[pyo3(name = "JOINT_STEREO")]
    JointStereo,
    #[pyo3(name = "DUAL_CHANNEL")]
    DualChannel,
    #[pyo3(name = "MONO")]
    Mono,
}

impl From<RustChannelMode> for ChannelModePy {
    fn from(v: RustChannelMode) -> Self {
        match v {
            RustChannelMode::Stereo => Self::Stereo,
            RustChannelMode::JointStereo => Self::JointStereo,
            RustChannelMode::DualChannel => Self::DualChannel,
            RustChannelMode::Mono => Self::Mono,
        }
    }
}

/// Lazy MPEG audio frame iterator returned by `iter_mpeg2_audio_frames` /
/// `iter_mpeg2_audio_frames_with_resync`.
///
/// Implementation: frames are collected eagerly at construction time into a
/// `Vec<FrameOwned>` — the lifetime-borrowed `Frames<'_>` iterator cannot
/// cross the PyO3 boundary. From Python's perspective the type has the
/// standard iterator protocol (`__iter__` + `__next__`).
#[pyclass(name = "Mpeg2AudioFrameIter", module = "tstrans.codec")]
pub struct Mpeg2AudioFrameIterPy {
    frames: Vec<RustMpeg2AudioFrameOwned>,
    index: usize,
}

#[pymethods]
impl Mpeg2AudioFrameIterPy {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<Mpeg2AudioFrame> {
        if self.index < self.frames.len() {
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Some(Mpeg2AudioFrame::from(&frame))
        } else {
            None
        }
    }
}

/// Returns an iterator over MPEG audio frames parsed from `bytes_buf`
/// (strict — raises `CodecError` on first parse failure). **Eager:** all
/// frames are collected upfront into a `Vec`; the returned object iterates
/// that `Vec`, not a true streaming parser. Memory usage is O(num_frames);
/// peak allocation occurs at construction. For very large buffers, prefer
/// chunked input or process in segments.
///
/// Internally drives the Rust `tst_core::codec::mpegaudio::frames`
/// iterator at construction time. The returned `Mpeg2AudioFrameIter`
/// yields `Mpeg2AudioFrame` objects one at a time from the pre-built
/// `Vec`.
///
/// Raises `CodecError` immediately if any frame fails to parse.
#[pyfunction]
#[pyo3(name = "iter_mpeg2_audio_frames")]
fn iter_mpeg2_audio_frames_py(py: Python<'_>, bytes_buf: &[u8]) -> PyResult<Mpeg2AudioFrameIterPy> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames = py.allow_threads(|| {
        rust_mpeg2audio_frames(bytes_buf)
            .map(|res| res.map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()
    });
    let frames =
        frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "mpeg2audio"))?;
    Ok(Mpeg2AudioFrameIterPy { frames, index: 0 })
}

/// Returns an iterator over MPEG audio frames parsed from `bytes_buf`
/// (best-effort — never raises, silently skips frames that fail to parse).
/// **Eager:** all frames are collected upfront into a `Vec`; the returned
/// object iterates that `Vec`, not a true streaming parser. Memory usage
/// is O(num_frames); peak allocation occurs at construction. For very
/// large buffers, prefer chunked input or process in segments.
///
/// Uses `tst_core::codec::mpegaudio::frames_with_resync` which scans forward
/// for the next plausible 11-bit syncword after each parse error. Errors are
/// filtered out; only successfully decoded frames are yielded.
#[pyfunction]
#[pyo3(name = "iter_mpeg2_audio_frames_with_resync")]
fn iter_mpeg2_audio_frames_with_resync_py(
    py: Python<'_>,
    bytes_buf: &[u8],
) -> Mpeg2AudioFrameIterPy {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_mpeg2audio_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    Mpeg2AudioFrameIterPy { frames, index: 0 }
}

/// Eagerly parse all MPEG audio frames from `bytes_buf` (strict — raises
/// `CodecError` on first parse failure).
///
/// Returns a `list[Mpeg2AudioFrame]` on success. Equivalent to
/// `list(iter_mpeg2_audio_frames(bytes_buf))` but avoids the iterator object.
#[pyfunction]
#[pyo3(name = "parse_mpeg2_audio_frames")]
fn parse_mpeg2_audio_frames_py(py: Python<'_>, bytes_buf: &[u8]) -> PyResult<Vec<Mpeg2AudioFrame>> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames = py.allow_threads(|| {
        rust_mpeg2audio_frames(bytes_buf)
            .map(|res| res.map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()
    });
    let frames =
        frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "mpeg2audio"))?;
    Ok(frames
        .into_iter()
        .map(|r| Mpeg2AudioFrame::from(&r))
        .collect())
}

/// Eagerly parse all MPEG audio frames from `bytes_buf`, skipping parse
/// errors (best-effort — never raises).
///
/// Uses `frames_with_resync` internally; only successfully decoded frames
/// appear in the returned list. Suitable for stats / telemetry use where
/// dropping a frame on corruption is preferable to aborting the parse.
#[pyfunction]
#[pyo3(name = "parse_mpeg2_audio_frames_with_resync")]
fn parse_mpeg2_audio_frames_with_resync_py(
    py: Python<'_>,
    bytes_buf: &[u8],
) -> Vec<Mpeg2AudioFrame> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_mpeg2audio_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    frames
        .into_iter()
        .map(|r| Mpeg2AudioFrame::from(&r))
        .collect()
}

// === Opt-in ES parse functions ===

/// Convert a Python `VideoCodec` enum value to the Rust `tst_core::mpegts::demux::VideoCodec`.
///
/// Reads the `.name` attribute of the Python enum (e.g. `"H264"`) and maps it to the
/// corresponding Rust variant. Returns an error for unrecognised names.
fn py_video_codec_to_rust(
    py: Python<'_>,
    codec: &Bound<'_, PyAny>,
) -> PyResult<tst_core::mpegts::demux::VideoCodec> {
    use tst_core::mpegts::demux::VideoCodec;
    let name: String = codec.getattr(pyo3::intern!(py, "name"))?.extract()?;
    match name.as_str() {
        "H264" => Ok(VideoCodec::H264),
        "H265" => Ok(VideoCodec::H265),
        "H266" => Ok(VideoCodec::H266),
        "AV1" => Ok(VideoCodec::Av1),
        other => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "unrecognised VideoCodec name: {other:?}"
        ))),
    }
}

/// Opt-in video ES parse: split a raw encoded access unit into typed
/// `NalUnit` / `Obu` objects.
///
/// `strict=True` raises `ValueError` on the first ES-conformance issue.
/// `strict=False` (default) returns `(units, issues)` where `units` is a
/// `list[NalUnit] | list[Obu]` and `issues` is a `list[str]` of
/// human-readable descriptions of any conformance issues found.
///
/// `carriage` is the AV1 carriage mode (a Python `Av1CarriageMode` enum
/// member, or `None` to default to `Mpeg2TsBinding`). Ignored for
/// H.264/H.265/H.266. For AV1, pass the `av1_carriage` field from the
/// `DemuxEvent.Video` so the framing expectation matches the on-wire bytes.
#[pyfunction]
#[pyo3(signature = (raw, codec, *, strict = false, carriage = None))]
fn split_units(
    py: Python<'_>,
    raw: &[u8],
    codec: &Bound<'_, PyAny>,
    strict: bool,
    carriage: Option<&Bound<'_, PyAny>>,
) -> PyResult<PyObject> {
    use tst_core::mpegts::demux::{split_video, split_video_strict};
    use tst_core::mpegts::mux::Av1CarriageMode;
    use tst_core::shared::SharedBytes;
    let rust_codec = py_video_codec_to_rust(py, codec)?;
    let shared = SharedBytes::from_vec(raw.to_vec());
    let rust_carriage = match carriage {
        Some(v) => crate::mux::py_av1_carriage(v)?,
        None => Av1CarriageMode::Mpeg2TsBinding,
    };
    if strict {
        match split_video_strict(&shared, rust_codec, rust_carriage) {
            Ok(vp) => crate::mpegts::convert_video_payload(py, &vp),
            Err(issue) => Err(pyo3::exceptions::PyValueError::new_err(format!("{issue}"))),
        }
    } else {
        let (vp, issues) = split_video(&shared, rust_codec, rust_carriage);
        let units = crate::mpegts::convert_video_payload(py, &vp)?;
        let issue_strs: Vec<String> = issues.iter().map(|i| format!("{i}")).collect();
        Ok((units, issue_strs).into_py(py))
    }
}

/// Opt-in audio ES parse. Returns a typed frame list from raw audio ES bytes.
///
/// `strict=True` uses `frames()` and raises `CodecError` on the first
/// malformed frame. `strict=False` (default) uses `frames_with_resync()`,
/// which skips past corruption to the next valid frame.
///
/// Returns:
/// - `list[AdtsFrame]` for `AudioCodec.AAC`
/// - `list[Mpeg2AudioFrame]` for `AudioCodec.MP2`
/// - `[]` for codecs with no typed parser (`AAC_LATM`, `AC3`)
#[pyfunction]
#[pyo3(signature = (raw, codec, *, strict = false))]
fn parse_audio(
    py: Python<'_>,
    raw: &[u8],
    codec: &Bound<'_, PyAny>,
    strict: bool,
) -> PyResult<PyObject> {
    let name: String = codec.getattr(pyo3::intern!(py, "name"))?.extract()?;
    let list = pyo3::types::PyList::empty_bound(py);
    match name.as_str() {
        "AAC" => {
            if strict {
                let frames = py.allow_threads(|| {
                    rust_aac_frames(raw)
                        .map(|res| res.map(|f| f.to_owned()))
                        .collect::<Result<Vec<_>, _>>()
                });
                let frames =
                    frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "aac"))?;
                for f in frames {
                    list.append(Py::new(py, AdtsFrame::from(&f))?)?;
                }
            } else {
                let frames: Vec<_> = py.allow_threads(|| {
                    rust_aac_frames_with_resync(raw)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect()
                });
                for f in frames {
                    list.append(Py::new(py, AdtsFrame::from(&f))?)?;
                }
            }
        }
        "MP2" => {
            if strict {
                let frames = py.allow_threads(|| {
                    rust_mpeg2audio_frames(raw)
                        .map(|res| res.map(|f| f.to_owned()))
                        .collect::<Result<Vec<_>, _>>()
                });
                let frames = frames
                    .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "mpeg2audio"))?;
                for f in frames {
                    list.append(Py::new(py, Mpeg2AudioFrame::from(&f))?)?;
                }
            } else {
                let frames: Vec<_> = py.allow_threads(|| {
                    rust_mpeg2audio_frames_with_resync(raw)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect()
                });
                for f in frames {
                    list.append(Py::new(py, Mpeg2AudioFrame::from(&f))?)?;
                }
            }
        }
        _ => {} // AAC_LATM, AC3, unknown → empty list (no typed parser)
    }
    Ok(list.into_py(py))
}

// === MISP timestamp (ST 0604) ===

/// Which MISP time base a [`MispTimestamp`] carries.
///
/// `MICRO` — microseconds since the MISP epoch (valid for H.264 + H.265).
/// `NANO`  — nanoseconds since the MISP epoch (H.265-only, ST 0604.6 §12.2).
#[pyclass(eq, eq_int, name = "MispTimeKind", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MispTimeKindPy {
    #[pyo3(name = "MICRO")]
    Micro,
    #[pyo3(name = "NANO")]
    Nano,
}

impl From<tst_core::codec::misp_time::MispTimeKind> for MispTimeKindPy {
    fn from(k: tst_core::codec::misp_time::MispTimeKind) -> Self {
        match k {
            tst_core::codec::misp_time::MispTimeKind::Nano => Self::Nano,
            // MispTimeKind is #[non_exhaustive] — default to Micro.
            _ => Self::Micro,
        }
    }
}

impl From<MispTimeKindPy> for tst_core::codec::misp_time::MispTimeKind {
    fn from(k: MispTimeKindPy) -> Self {
        match k {
            MispTimeKindPy::Micro => Self::Micro,
            MispTimeKindPy::Nano => Self::Nano,
        }
    }
}

/// Extract the first MISP timestamp SEI from an Annex-B access unit.
///
/// `au` — bytes-like access unit (Annex-B encoded, as delivered by the
/// demuxer's `DemuxEvent.Video.raw`).
/// `codec` — a `VideoCodec` enum member identifying the codec (`H264` or
/// `H265`; the function is liberal and accepts any codec value but will
/// return `None` for `H266` / `AV1` as ST 0604 defines no carriage for
/// them).
///
/// Returns:
/// - `None` — no MISP SEI present in the AU.
/// - `MispTimestamp` — the first MISP SEI found.
/// - Raises `ValueError` — a MISP identifier matched but the payload is
///   malformed (truncated payload or bad guard byte).
#[pyfunction]
fn extract_misp_timestamp(
    py: Python<'_>,
    au: &Bound<'_, PyAny>,
    codec: &Bound<'_, PyAny>,
) -> PyResult<Option<MispTimestamp>> {
    use tst_core::codec::misp_time::{MispTimeExtractError, extract};
    let coerced = crate::util::coerce_bytes_like(py, au)?;
    let au_bytes = coerced.as_bytes();
    let mux_codec = py_video_codec_to_rust(py, codec)?;
    let result = py.allow_threads(|| extract(au_bytes, mux_codec));
    match result {
        Ok(None) => Ok(None),
        Ok(Some(ts)) => Ok(Some(MispTimestamp::from(&ts))),
        Err(MispTimeExtractError::TruncatedSei) => Err(pyo3::exceptions::PyValueError::new_err(
            "MISP SEI payload truncated (need 28 bytes)",
        )),
        Err(MispTimeExtractError::BadGuardByte) => Err(pyo3::exceptions::PyValueError::new_err(
            "MISP SEI modified-timestamp guard byte is not 0xFF",
        )),
        Err(_) => Err(pyo3::exceptions::PyValueError::new_err(
            "malformed MISP SEI payload",
        )),
    }
}

// === Module registration ===

/// Register all codec classes on `m` (`tstrans._native`).
///
/// Classes are added flat on the native extension module — matching the
/// pattern established by `mpegts::register` and `klv::register`. The
/// Python-side `tstrans.codec` module then re-imports them by name. Per-codec
/// classes added by Tasks 9-14 extend this same function.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Shared enums
    m.add_class::<ChromaFormatPy>()?;
    m.add_class::<ColourPrimariesPy>()?;
    m.add_class::<TransferCharacteristicsPy>()?;
    m.add_class::<MatrixCoefficientsPy>()?;
    // The records (generated)
    crate::generated::codec::register(m)?;
    // Typed NAL / OBU wrappers (used on Sample.payload)
    m.add_class::<NalUnitPy>()?;
    // H.264 enums
    m.add_class::<EntropyCodingModePy>()?;
    m.add_class::<H264SliceTypePy>()?;
    // H.264 structs
    // H.264 parser functions
    m.add_function(wrap_pyfunction!(parse_h264_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_slice_header_light_py, m)?)?;
    // H.265 enums
    m.add_class::<H265SliceTypePy>()?;
    // H.265 structs
    // H.265 parser functions
    m.add_function(wrap_pyfunction!(parse_h265_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_vps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_slice_header_light_py, m)?)?;
    // H.266 enums
    m.add_class::<H266SliceTypePy>()?;
    // H.266 structs
    // H.266 parser functions
    m.add_function(wrap_pyfunction!(parse_h266_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_vps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_slice_header_light_py, m)?)?;
    // AV1 structs
    // AV1 parser functions
    m.add_function(wrap_pyfunction!(parse_av1_sequence_header_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_av1_frame_header_light_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_av1_obu_stream_py, m)?)?;
    // AAC enums + channel layout class
    m.add_class::<AacProfilePy>()?;
    m.add_class::<MpegVersionPy>()?;
    m.add_class::<AacChannelLayoutPy>()?;
    // AAC frame + iterator
    m.add_class::<AdtsFrameIterPy>()?;
    // AAC parser functions
    m.add_function(wrap_pyfunction!(iter_aac_frames_py, m)?)?;
    m.add_function(wrap_pyfunction!(iter_aac_frames_with_resync_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_aac_frames_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_aac_frames_with_resync_py, m)?)?;
    // MPEG-2 audio enums
    m.add_class::<LayerPy>()?;
    m.add_class::<VersionPy>()?;
    m.add_class::<ChannelModePy>()?;
    // MPEG-2 audio frame + iterator
    m.add_class::<Mpeg2AudioFrameIterPy>()?;
    // MPEG-2 audio parser functions
    m.add_function(wrap_pyfunction!(iter_mpeg2_audio_frames_py, m)?)?;
    m.add_function(wrap_pyfunction!(iter_mpeg2_audio_frames_with_resync_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_mpeg2_audio_frames_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        parse_mpeg2_audio_frames_with_resync_py,
        m
    )?)?;
    // Opt-in ES parse functions
    m.add_function(wrap_pyfunction!(split_units, m)?)?;
    m.add_function(wrap_pyfunction!(parse_audio, m)?)?;
    // MISP timestamp (ST 0604)
    m.add_class::<MispTimeKindPy>()?;
    m.add_function(wrap_pyfunction!(extract_misp_timestamp, m)?)?;
    Ok(())
}
