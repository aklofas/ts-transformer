//! tst-py — codec module.
//!
//! All codec PyO3 wraps for H.264/H.265/H.266/AV1/AAC/MPEG-2 audio,
//! plus shared types (ChromaFormat, Rational, ColorInfo, primaries/transfer/
//! matrix enums), plus NalUnit / Obu / ObuExtension typed wrappers used by
//! `Sample.payload`.

// PyO3 0.22 + Rust 2024 edition: the #[pymethods] macro generates calls to
// internal unsafe functions inside unsafe fn bodies. The `unsafe_op_in_unsafe_fn`
// lint (now a warning in edition 2024) fires on macro-generated code; suppress
// here exactly as in the sibling modules (errors.rs, klv.rs, mpegts.rs, mux.rs).
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::codec::{
    ChromaFormat as RustChromaFormat, ColorInfo as RustColorInfo,
    ColourPrimaries as RustColourPrimaries, MatrixCoefficients as RustMatrixCoefficients,
    Rational as RustRational, TransferCharacteristics as RustTransferCharacteristics,
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

// === Rational + ColorInfo ===

/// Numerator/denominator pair. Mirrors `tst_core::codec::Rational`.
#[pyclass(name = "Rational", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone, Copy)]
pub struct RationalPy {
    pub num: u32,
    pub den: u32,
}

#[pymethods]
impl RationalPy {
    #[new]
    fn new(num: u32, den: u32) -> Self {
        Self { num, den }
    }

    #[getter]
    fn num(&self) -> u32 {
        self.num
    }

    #[getter]
    fn den(&self) -> u32 {
        self.den
    }

    /// Return `num / den` as a floating-point value.
    fn as_float(&self) -> f64 {
        self.num as f64 / self.den as f64
    }

    fn __repr__(&self) -> String {
        format!("Rational(num={}, den={})", self.num, self.den)
    }
}

impl From<RustRational> for RationalPy {
    fn from(r: RustRational) -> Self {
        Self {
            num: r.num,
            den: r.den,
        }
    }
}

/// VUI / video signal type metadata. Mirrors `tst_core::codec::ColorInfo`.
///
/// All fields decoded per ITU-T H.273 / ISO/IEC 23091-2 (the codec-independent
/// registry referenced by both H.264 §E.2.1 and H.265 §E.2.1).
#[pyclass(name = "ColorInfo", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone)]
pub struct ColorInfoPy {
    pub primaries: ColourPrimariesPy,
    pub transfer: TransferCharacteristicsPy,
    pub matrix: MatrixCoefficientsPy,
    pub full_range: bool,
}

#[pymethods]
impl ColorInfoPy {
    #[new]
    fn new(
        primaries: ColourPrimariesPy,
        transfer: TransferCharacteristicsPy,
        matrix: MatrixCoefficientsPy,
        full_range: bool,
    ) -> Self {
        Self {
            primaries,
            transfer,
            matrix,
            full_range,
        }
    }

    #[getter]
    fn primaries(&self) -> ColourPrimariesPy {
        self.primaries
    }

    #[getter]
    fn transfer(&self) -> TransferCharacteristicsPy {
        self.transfer
    }

    #[getter]
    fn matrix(&self) -> MatrixCoefficientsPy {
        self.matrix
    }

    #[getter]
    fn full_range(&self) -> bool {
        self.full_range
    }

    fn __repr__(&self) -> String {
        format!(
            "ColorInfo(primaries={:?}, transfer={:?}, matrix={:?}, full_range={})",
            self.primaries, self.transfer, self.matrix, self.full_range
        )
    }
}

impl From<RustColorInfo> for ColorInfoPy {
    fn from(c: RustColorInfo) -> Self {
        Self {
            primaries: c.primaries.into(),
            transfer: c.transfer.into(),
            matrix: c.matrix.into(),
            full_range: c.full_range,
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

// === Obu + ObuExtension ===

/// AV1 OBU extension header. Present when `obu_extension_flag = 1`.
/// Per AV1 Bitstream Spec §5.3.3.
#[pyclass(name = "ObuExtension", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone, Copy)]
pub struct ObuExtensionPy {
    /// 3-bit `temporal_id` (AV1 §5.3.3).
    pub temporal_id: u8,
    /// 2-bit `spatial_id` (AV1 §5.3.3).
    pub spatial_id: u8,
}

#[pymethods]
impl ObuExtensionPy {
    #[new]
    fn new(temporal_id: u8, spatial_id: u8) -> Self {
        Self {
            temporal_id,
            spatial_id,
        }
    }

    #[getter]
    fn temporal_id(&self) -> u8 {
        self.temporal_id
    }

    #[getter]
    fn spatial_id(&self) -> u8 {
        self.spatial_id
    }

    fn __repr__(&self) -> String {
        format!(
            "ObuExtension(temporal_id={}, spatial_id={})",
            self.temporal_id, self.spatial_id
        )
    }
}

/// One AV1 Open Bitstream Unit. Per AV1 Bitstream Spec §5.3.2.
///
/// The OBU header byte, any extension byte, and the LEB128 `obu_size`
/// field are stripped during split; `payload` carries only the OBU body.
#[pyclass(name = "Obu", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone)]
pub struct ObuPy {
    /// 4-bit `obu_type` (AV1 §5.3.2).
    /// Common values: 1=SequenceHeader, 2=TemporalDelimiter, 3=FrameHeader,
    /// 4=TileGroup, 5=Metadata, 6=Frame, 7=RedundantFrameHeader,
    /// 8=TileList, 15=Padding.
    pub obu_type: u8,
    /// Extension header, present when `obu_extension_flag = 1`.
    pub extension: Option<ObuExtensionPy>,
    // OBU body bytes; exposed via #[getter] returning PyBytes.
    pub payload: Vec<u8>,
}

#[pymethods]
impl ObuPy {
    #[new]
    #[pyo3(signature = (obu_type, extension, payload))]
    fn new(obu_type: u8, extension: Option<ObuExtensionPy>, payload: Vec<u8>) -> Self {
        Self {
            obu_type,
            extension,
            payload,
        }
    }

    #[getter]
    fn obu_type(&self) -> u8 {
        self.obu_type
    }

    #[getter]
    fn extension(&self) -> Option<ObuExtensionPy> {
        self.extension
    }

    /// OBU body bytes — header, extension byte, and LEB128 size field stripped.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.payload)
    }

    fn __repr__(&self) -> String {
        format!(
            "Obu(obu_type={}, payload_len={})",
            self.obu_type,
            self.payload.len()
        )
    }
}

// Crate-internal Rust constructor for ObuPy.
impl ObuPy {
    pub(crate) fn make(obu_type: u8, extension: Option<ObuExtensionPy>, payload: Vec<u8>) -> Self {
        Self {
            obu_type,
            extension,
            payload,
        }
    }
}

// === H.264 ===

use tst_core::codec::h264::{
    EntropyCodingMode as RustEntropyCodingMode, H264ParameterSets as RustH264ParameterSets,
    H264Pps as RustH264Pps, H264SliceHeaderLight as RustH264SliceHeaderLight,
    H264SliceType as RustH264SliceType, H264Sps as RustH264Sps,
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

/// Parsed H.264 Sequence Parameter Set.
/// Mirrors `tst_core::codec::h264::H264Sps`.
#[pyclass(name = "H264Sps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H264SpsPy {
    inner: RustH264Sps,
}

#[pymethods]
impl H264SpsPy {
    /// `seq_parameter_set_id` — identifies this SPS (H.264 §7.4.2.1.1).
    #[getter]
    fn seq_parameter_set_id(&self) -> u8 {
        self.inner.seq_parameter_set_id
    }

    /// Post-crop display width in luma samples.
    #[getter]
    fn width(&self) -> u32 {
        self.inner.width
    }

    /// Post-crop display height in luma samples.
    #[getter]
    fn height(&self) -> u32 {
        self.inner.height
    }

    /// `profile_idc` (66=Baseline, 77=Main, 100=High, …).
    #[getter]
    fn profile_idc(&self) -> u8 {
        self.inner.profile_idc
    }

    /// `level_idc` — e.g. 40 for Level 4.0.
    #[getter]
    fn level_idc(&self) -> u8 {
        self.inner.level_idc
    }

    /// `constraint_set_flags` byte (bits 7-2 = flags; bits 1-0 = reserved zero).
    #[getter]
    fn constraint_set_flags(&self) -> u8 {
        self.inner.constraint_set_flags
    }

    /// Luma bit depth (8 + `bit_depth_luma_minus8`).
    #[getter]
    fn bit_depth_luma(&self) -> u8 {
        self.inner.bit_depth_luma
    }

    /// Chroma bit depth (8 + `bit_depth_chroma_minus8`).
    #[getter]
    fn bit_depth_chroma(&self) -> u8 {
        self.inner.bit_depth_chroma
    }

    /// Chroma subsampling format.
    #[getter]
    fn chroma_format(&self) -> ChromaFormatPy {
        self.inner.chroma_format.into()
    }

    /// True for progressive encoding (`frame_mbs_only_flag=1`).
    #[getter]
    fn frame_mbs_only(&self) -> bool {
        self.inner.frame_mbs_only
    }

    /// True when `fixed_frame_rate_flag=1` in the VUI.
    #[getter]
    fn fixed_frame_rate(&self) -> bool {
        self.inner.fixed_frame_rate
    }

    /// True when the stream may contain B-frames (heuristic; see Rust docs).
    #[getter]
    fn has_b_frames(&self) -> bool {
        self.inner.has_b_frames
    }

    /// Frame rate as `Rational(num, den)`, or `None` when the VUI is absent.
    #[getter]
    fn frame_rate(&self) -> Option<RationalPy> {
        self.inner.frame_rate.map(Into::into)
    }

    /// VUI colour info, or `None` when the VUI is absent or video_signal_type
    /// is not present.
    #[getter]
    fn color(&self) -> Option<ColorInfoPy> {
        self.inner.color.clone().map(Into::into)
    }

    /// Left crop offset in luma samples (H.264 §6.4 after SubWidthC scaling).
    #[getter]
    fn crop_left(&self) -> u32 {
        self.inner.crop_left
    }

    /// Right crop offset in luma samples.
    #[getter]
    fn crop_right(&self) -> u32 {
        self.inner.crop_right
    }

    /// Top crop offset in luma samples.
    #[getter]
    fn crop_top(&self) -> u32 {
        self.inner.crop_top
    }

    /// Bottom crop offset in luma samples.
    #[getter]
    fn crop_bottom(&self) -> u32 {
        self.inner.crop_bottom
    }

    /// `log2_max_frame_num_minus4` — determines the bit width of `frame_num`
    /// in slice headers (`frame_num` width = this + 4).
    #[getter]
    fn log2_max_frame_num_minus4(&self) -> u8 {
        self.inner.log2_max_frame_num_minus4
    }

    /// Original RBSP bytes as supplied to `parse_h264_sps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    /// Coded picture width before `frame_crop` is applied (luma samples).
    /// Equal to `width + crop_left + crop_right`.
    fn coded_width(&self) -> u32 {
        self.inner.coded_width()
    }

    /// Coded picture height before `frame_crop` is applied (luma samples).
    /// Equal to `height + crop_top + crop_bottom`.
    fn coded_height(&self) -> u32 {
        self.inner.coded_height()
    }

    fn __repr__(&self) -> String {
        format!(
            "H264Sps(profile={}, level={}, {}x{}, sps_id={})",
            self.inner.profile_idc,
            self.inner.level_idc,
            self.inner.width,
            self.inner.height,
            self.inner.seq_parameter_set_id,
        )
    }
}

/// Parsed H.264 Picture Parameter Set.
/// Mirrors `tst_core::codec::h264::H264Pps`.
#[pyclass(name = "H264Pps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H264PpsPy {
    inner: RustH264Pps,
}

#[pymethods]
impl H264PpsPy {
    /// `pic_parameter_set_id` ∈ [0, 255].
    #[getter]
    fn pic_parameter_set_id(&self) -> u8 {
        self.inner.pic_parameter_set_id
    }

    /// `seq_parameter_set_id` — links this PPS to an SPS. ∈ [0, 31].
    #[getter]
    fn seq_parameter_set_id(&self) -> u8 {
        self.inner.seq_parameter_set_id
    }

    /// Entropy coding mode: `CAVLC` or `CABAC`.
    #[getter]
    fn entropy_coding_mode(&self) -> EntropyCodingModePy {
        self.inner.entropy_coding_mode.into()
    }

    /// Original RBSP bytes as supplied to `parse_h264_pps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H264Pps(pps_id={}, sps_id={})",
            self.inner.pic_parameter_set_id, self.inner.seq_parameter_set_id
        )
    }
}

/// Light-weight H.264 slice header — fields required for keyframe detection
/// and frame-type classification.
/// Mirrors `tst_core::codec::h264::H264SliceHeaderLight`.
#[pyclass(name = "H264SliceHeaderLight", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H264SliceHeaderLightPy {
    inner: RustH264SliceHeaderLight,
}

#[pymethods]
impl H264SliceHeaderLightPy {
    /// True when `first_mb_in_slice == 0` — start of a new frame.
    #[getter]
    fn first_in_pic(&self) -> bool {
        self.inner.first_in_pic
    }

    /// Slice type (normalised via `slice_type % 5` per H.264 §7.4.3).
    #[getter]
    fn slice_type(&self) -> H264SliceTypePy {
        self.inner.slice_type.into()
    }

    /// `pic_parameter_set_id` — links this slice to a PPS.
    #[getter]
    fn pps_id(&self) -> u8 {
        self.inner.pps_id
    }

    /// `frame_num` using the bit width from the referenced SPS, or `None`
    /// when no SPS context was passed to `parse_h264_slice_header_light`.
    #[getter]
    fn frame_num(&self) -> Option<u32> {
        self.inner.frame_num
    }

    /// True when `nal_unit_type == 5` (IDR slice).
    #[getter]
    fn idr(&self) -> bool {
        self.inner.idr
    }

    /// Original RBSP bytes as supplied to `parse_h264_slice_header_light`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H264SliceHeaderLight(first={}, slice_type={:?}, idr={})",
            self.inner.first_in_pic, self.inner.slice_type, self.inner.idr,
        )
    }
}

/// All SPS and PPS NAL units parsed from an access unit.
/// Mirrors `tst_core::codec::h264::H264ParameterSets`.
#[pyclass(name = "H264ParameterSets", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H264ParameterSetsPy {
    inner: RustH264ParameterSets,
}

#[pymethods]
impl H264ParameterSetsPy {
    /// Mapping of `sps_id → H264Sps`. Keys are `int`, values are `H264Sps`.
    #[getter]
    fn sps_by_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let dict = pyo3::types::PyDict::new_bound(py);
        for (k, v) in &self.inner.sps_by_id {
            let sps_py = Py::new(py, H264SpsPy { inner: v.clone() })?;
            dict.set_item(*k, sps_py)?;
        }
        Ok(dict)
    }

    /// Mapping of `pps_id → H264Pps`. Keys are `int`, values are `H264Pps`.
    #[getter]
    fn pps_by_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let dict = pyo3::types::PyDict::new_bound(py);
        for (k, v) in &self.inner.pps_by_id {
            let pps_py = Py::new(py, H264PpsPy { inner: v.clone() })?;
            dict.set_item(*k, pps_py)?;
        }
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "H264ParameterSets(n_sps={}, n_pps={})",
            self.inner.sps_by_id.len(),
            self.inner.pps_by_id.len()
        )
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
fn parse_h264_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H264SpsPy> {
    rust_parse_h264_sps(rbsp)
        .map(|inner| H264SpsPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

/// Parse a single H.264 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h264_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h264_pps")]
fn parse_h264_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H264PpsPy> {
    rust_parse_h264_pps(rbsp)
        .map(|inner| H264PpsPy { inner })
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
) -> PyResult<H264ParameterSetsPy> {
    use tst_core::mpegts::demux::event::NalUnit as RustNalUnit;
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
        .map(|inner| H264ParameterSetsPy { inner })
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
    sps: Option<&H264SpsPy>,
    nal_unit_type: u8,
) -> PyResult<H264SliceHeaderLightPy> {
    rust_parse_h264_slice_header_light(rbsp, sps.map(|s| &s.inner), nal_unit_type)
        .map(|inner| H264SliceHeaderLightPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h264"))
}

// === H.265 ===

use tst_core::codec::h265::{
    H265ParameterSets as RustH265ParameterSets, H265Pps as RustH265Pps,
    H265SliceHeaderLight as RustH265SliceHeaderLight, H265SliceType as RustH265SliceType,
    H265Sps as RustH265Sps, H265Vps as RustH265Vps,
    parse_parameter_sets as rust_parse_h265_parameter_sets, parse_pps as rust_parse_h265_pps,
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

/// H.265 profile/tier/level fields parsed from a VPS or SPS NAL unit.
/// Mirrors the fields from `tst_core::codec::h265::H265ProfileTierLevel`.
///
/// These fields are decoded from the `profile_tier_level()` syntax structure
/// at H.265 §7.3.3. Both [`H265SpsPy`] and [`H265VpsPy`] expose the same
/// values via their `profile_tier_level()` method, which returns this object.
/// The fields are also available directly as getters on both SPS and VPS
/// classes for convenience.
///
/// Note: `general_profile_space` is always 0 when reconstructed from an SPS
/// or VPS object because `H265Sps` / `H265Vps` do not store that field
/// separately (it is always 0 for all ITU-T registered profiles).
#[pyclass(name = "H265ProfileTierLevel", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy)]
pub struct H265ProfileTierLevelPy {
    general_profile_space: u8,
    general_tier_flag: bool,
    general_profile_idc: u8,
    general_profile_compatibility_flags: u32,
    general_progressive_source_flag: bool,
    general_interlaced_source_flag: bool,
    general_non_packed_constraint_flag: bool,
    general_frame_only_constraint_flag: bool,
    general_level_idc: u8,
}

#[pymethods]
impl H265ProfileTierLevelPy {
    /// 2-bit `general_profile_space` (§7.3.3). In practice always 0 for
    /// ITU-T registered profiles.
    #[getter]
    fn general_profile_space(&self) -> u8 {
        self.general_profile_space
    }

    /// `general_tier_flag` (§7.4.4): true = High tier, false = Main tier.
    #[getter]
    fn general_tier_flag(&self) -> bool {
        self.general_tier_flag
    }

    /// 5-bit `general_profile_idc` (§7.3.3). Common values: 1=Main,
    /// 2=Main10, 4=Rext, 5=HEVC-HM, 6=Multiview-Main, 7=Scalable-Main.
    #[getter]
    fn general_profile_idc(&self) -> u8 {
        self.general_profile_idc
    }

    /// 32-bit `general_profile_compatibility_flags` (§7.3.3). Bit `i` set
    /// means the stream conforms to profile `i`. MSB-first: spec-bit j lives
    /// at `flags & (1 << (31 - j))`. ffmpeg uses bit 2 (= 1 << 29) to
    /// disambiguate Main vs Main10 vs Main10-Intra.
    #[getter]
    fn general_profile_compatibility_flags(&self) -> u32 {
        self.general_profile_compatibility_flags
    }

    /// `general_progressive_source_flag` (§7.4.4): stream is progressive.
    #[getter]
    fn general_progressive_source_flag(&self) -> bool {
        self.general_progressive_source_flag
    }

    /// `general_interlaced_source_flag` (§7.4.4): stream is interlaced.
    #[getter]
    fn general_interlaced_source_flag(&self) -> bool {
        self.general_interlaced_source_flag
    }

    /// `general_non_packed_constraint_flag` (§7.4.4): no frame-packing
    /// arrangement SEI in the bitstream.
    #[getter]
    fn general_non_packed_constraint_flag(&self) -> bool {
        self.general_non_packed_constraint_flag
    }

    /// `general_frame_only_constraint_flag` (§7.4.4): stream contains only
    /// frames (no field pictures).
    #[getter]
    fn general_frame_only_constraint_flag(&self) -> bool {
        self.general_frame_only_constraint_flag
    }

    /// `general_level_idc` (§7.3.3). Level encoded as `30 * level_major +
    /// 3 * level_minor` for levels up to 6.2: e.g. 120 = Level 4.0,
    /// 150 = Level 5.0, 180 = Level 6.0.
    #[getter]
    fn general_level_idc(&self) -> u8 {
        self.general_level_idc
    }

    fn __repr__(&self) -> String {
        format!(
            "H265ProfileTierLevel(profile_idc={}, tier={}, level_idc={})",
            self.general_profile_idc, self.general_tier_flag, self.general_level_idc,
        )
    }
}

/// Construct an `H265ProfileTierLevelPy` from the fields stored on an
/// `H265Sps` or `H265Vps` (which both carry the PTL fields flattened).
/// `general_profile_space` is not stored on either of those types (it is
/// always 0 for all ITU-T registered profiles).
#[allow(clippy::too_many_arguments)]
fn ptl_from_sps_fields(
    general_tier_flag: bool,
    general_profile_idc: u8,
    general_profile_compatibility_flags: u32,
    general_progressive_source_flag: bool,
    general_interlaced_source_flag: bool,
    general_non_packed_constraint_flag: bool,
    general_frame_only_constraint_flag: bool,
    general_level_idc: u8,
) -> H265ProfileTierLevelPy {
    H265ProfileTierLevelPy {
        general_profile_space: 0,
        general_tier_flag,
        general_profile_idc,
        general_profile_compatibility_flags,
        general_progressive_source_flag,
        general_interlaced_source_flag,
        general_non_packed_constraint_flag,
        general_frame_only_constraint_flag,
        general_level_idc,
    }
}

/// Parsed H.265 Sequence Parameter Set.
/// Mirrors `tst_core::codec::h265::H265Sps`.
#[pyclass(name = "H265Sps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H265SpsPy {
    inner: RustH265Sps,
}

#[pymethods]
impl H265SpsPy {
    /// `sps_seq_parameter_set_id` — identifies this SPS (H.265 §7.4.3.2.1).
    #[getter]
    fn sps_seq_parameter_set_id(&self) -> u8 {
        self.inner.sps_seq_parameter_set_id
    }

    /// `sps_video_parameter_set_id` — links this SPS to a VPS.
    #[getter]
    fn sps_video_parameter_set_id(&self) -> u8 {
        self.inner.sps_video_parameter_set_id
    }

    /// Post-crop display width in luma samples (after conformance window is
    /// applied).
    #[getter]
    fn width(&self) -> u32 {
        self.inner.width
    }

    /// Post-crop display height in luma samples.
    #[getter]
    fn height(&self) -> u32 {
        self.inner.height
    }

    /// `general_profile_idc` (1=Main, 2=Main10, …). Decoded from the
    /// `profile_tier_level()` block inside the SPS (§7.3.3).
    #[getter]
    fn general_profile_idc(&self) -> u8 {
        self.inner.general_profile_idc
    }

    /// `general_tier_flag` — true = High tier, false = Main tier.
    #[getter]
    fn general_tier_flag(&self) -> bool {
        self.inner.general_tier_flag
    }

    /// `general_level_idc` — e.g. 120 for Level 4.0, 150 for Level 5.0.
    #[getter]
    fn general_level_idc(&self) -> u8 {
        self.inner.general_level_idc
    }

    /// 32-bit `general_profile_compatibility_flags`. See
    /// [`H265ProfileTierLevel.general_profile_compatibility_flags`] for
    /// bit-ordering details.
    #[getter]
    fn general_profile_compatibility_flags(&self) -> u32 {
        self.inner.general_profile_compatibility_flags
    }

    /// `general_progressive_source_flag` (§7.4.4).
    #[getter]
    fn general_progressive_source_flag(&self) -> bool {
        self.inner.general_progressive_source_flag
    }

    /// `general_interlaced_source_flag` (§7.4.4).
    #[getter]
    fn general_interlaced_source_flag(&self) -> bool {
        self.inner.general_interlaced_source_flag
    }

    /// `general_non_packed_constraint_flag` (§7.4.4).
    #[getter]
    fn general_non_packed_constraint_flag(&self) -> bool {
        self.inner.general_non_packed_constraint_flag
    }

    /// `general_frame_only_constraint_flag` (§7.4.4).
    #[getter]
    fn general_frame_only_constraint_flag(&self) -> bool {
        self.inner.general_frame_only_constraint_flag
    }

    /// Luma bit depth (8 + `bit_depth_luma_minus8`).
    #[getter]
    fn bit_depth_luma(&self) -> u8 {
        self.inner.bit_depth_luma
    }

    /// Chroma bit depth (8 + `bit_depth_chroma_minus8`).
    #[getter]
    fn bit_depth_chroma(&self) -> u8 {
        self.inner.bit_depth_chroma
    }

    /// Chroma subsampling format.
    #[getter]
    fn chroma_format(&self) -> ChromaFormatPy {
        self.inner.chroma_format.into()
    }

    /// `sps_max_sub_layers_minus1` — max sub-layer temporal scalability.
    #[getter]
    fn max_sub_layers_minus1(&self) -> u8 {
        self.inner.max_sub_layers_minus1
    }

    /// Frame rate as `Rational(num, den)`, or `None` when VUI timing is absent.
    #[getter]
    fn frame_rate(&self) -> Option<RationalPy> {
        self.inner.frame_rate.map(Into::into)
    }

    /// VUI colour info, or `None` when VUI or `video_signal_type_present_flag`
    /// is absent.
    #[getter]
    fn color(&self) -> Option<ColorInfoPy> {
        self.inner.color.clone().map(Into::into)
    }

    /// Left crop offset in luma samples (after `SubWidthC` scaling).
    #[getter]
    fn crop_left(&self) -> u32 {
        self.inner.crop_left
    }

    /// Right crop offset in luma samples.
    #[getter]
    fn crop_right(&self) -> u32 {
        self.inner.crop_right
    }

    /// Top crop offset in luma samples.
    #[getter]
    fn crop_top(&self) -> u32 {
        self.inner.crop_top
    }

    /// Bottom crop offset in luma samples.
    #[getter]
    fn crop_bottom(&self) -> u32 {
        self.inner.crop_bottom
    }

    /// `log2_max_pic_order_cnt_lsb_minus4` (H.265 §7.4.3.2.1). The bit width
    /// of `pic_order_cnt_lsb` in slice headers equals this value plus 4.
    #[getter]
    fn log2_max_pic_order_cnt_lsb_minus4(&self) -> u8 {
        self.inner.log2_max_pic_order_cnt_lsb_minus4
    }

    /// Original RBSP bytes as supplied to `parse_h265_sps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    /// Reconstructed `H265ProfileTierLevel` from the fields decoded inside
    /// this SPS's `profile_tier_level()` block.
    fn profile_tier_level(&self) -> H265ProfileTierLevelPy {
        ptl_from_sps_fields(
            self.inner.general_tier_flag,
            self.inner.general_profile_idc,
            self.inner.general_profile_compatibility_flags,
            self.inner.general_progressive_source_flag,
            self.inner.general_interlaced_source_flag,
            self.inner.general_non_packed_constraint_flag,
            self.inner.general_frame_only_constraint_flag,
            self.inner.general_level_idc,
        )
    }

    /// Coded picture width before conformance-window crop is applied
    /// (luma samples). Equal to `width + crop_left + crop_right`.
    fn coded_width(&self) -> u32 {
        self.inner.coded_width()
    }

    /// Coded picture height before conformance-window crop is applied
    /// (luma samples). Equal to `height + crop_top + crop_bottom`.
    fn coded_height(&self) -> u32 {
        self.inner.coded_height()
    }

    fn __repr__(&self) -> String {
        format!(
            "H265Sps(profile={}, level={}, {}x{}, sps_id={})",
            self.inner.general_profile_idc,
            self.inner.general_level_idc,
            self.inner.width,
            self.inner.height,
            self.inner.sps_seq_parameter_set_id,
        )
    }
}

/// Parsed H.265 Picture Parameter Set.
/// Mirrors `tst_core::codec::h265::H265Pps`.
#[pyclass(name = "H265Pps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H265PpsPy {
    inner: RustH265Pps,
}

#[pymethods]
impl H265PpsPy {
    /// `pps_pic_parameter_set_id` ∈ [0, 63].
    #[getter]
    fn pps_pic_parameter_set_id(&self) -> u8 {
        self.inner.pps_pic_parameter_set_id
    }

    /// `pps_seq_parameter_set_id` — links this PPS to an SPS. ∈ [0, 15].
    #[getter]
    fn pps_seq_parameter_set_id(&self) -> u8 {
        self.inner.pps_seq_parameter_set_id
    }

    /// Original RBSP bytes as supplied to `parse_h265_pps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H265Pps(pps_id={}, sps_id={})",
            self.inner.pps_pic_parameter_set_id, self.inner.pps_seq_parameter_set_id
        )
    }
}

/// Parsed H.265 Video Parameter Set.
/// Mirrors `tst_core::codec::h265::H265Vps`.
#[pyclass(name = "H265Vps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H265VpsPy {
    inner: RustH265Vps,
}

#[pymethods]
impl H265VpsPy {
    /// 4-bit `vps_video_parameter_set_id` — identifies this VPS.
    #[getter]
    fn vps_video_parameter_set_id(&self) -> u8 {
        self.inner.vps_video_parameter_set_id
    }

    /// `vps_max_layers_minus1` — max number of spatial layers (6-bit).
    #[getter]
    fn max_layers_minus1(&self) -> u8 {
        self.inner.max_layers_minus1
    }

    /// `vps_max_sub_layers_minus1` — max temporal sub-layer count (3-bit).
    #[getter]
    fn max_sub_layers_minus1(&self) -> u8 {
        self.inner.max_sub_layers_minus1
    }

    /// `vps_temporal_id_nesting_flag`: when true, temporal sub-layer
    /// `j < max_sub_layers_minus1` nests inside sub-layer `j+1`.
    #[getter]
    fn temporal_id_nesting_flag(&self) -> bool {
        self.inner.temporal_id_nesting_flag
    }

    /// `general_profile_idc` decoded from the VPS profile_tier_level block.
    #[getter]
    fn general_profile_idc(&self) -> u8 {
        self.inner.general_profile_idc
    }

    /// `general_tier_flag` decoded from the VPS profile_tier_level block.
    #[getter]
    fn general_tier_flag(&self) -> bool {
        self.inner.general_tier_flag
    }

    /// `general_level_idc` decoded from the VPS profile_tier_level block.
    #[getter]
    fn general_level_idc(&self) -> u8 {
        self.inner.general_level_idc
    }

    /// 32-bit `general_profile_compatibility_flags` from the VPS.
    #[getter]
    fn general_profile_compatibility_flags(&self) -> u32 {
        self.inner.general_profile_compatibility_flags
    }

    /// `general_progressive_source_flag` (§7.4.4).
    #[getter]
    fn general_progressive_source_flag(&self) -> bool {
        self.inner.general_progressive_source_flag
    }

    /// `general_interlaced_source_flag` (§7.4.4).
    #[getter]
    fn general_interlaced_source_flag(&self) -> bool {
        self.inner.general_interlaced_source_flag
    }

    /// `general_non_packed_constraint_flag` (§7.4.4).
    #[getter]
    fn general_non_packed_constraint_flag(&self) -> bool {
        self.inner.general_non_packed_constraint_flag
    }

    /// `general_frame_only_constraint_flag` (§7.4.4).
    #[getter]
    fn general_frame_only_constraint_flag(&self) -> bool {
        self.inner.general_frame_only_constraint_flag
    }

    /// Original RBSP bytes as supplied to `parse_h265_vps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    /// Reconstructed `H265ProfileTierLevel` from the fields decoded inside
    /// this VPS's `profile_tier_level()` block.
    fn profile_tier_level(&self) -> H265ProfileTierLevelPy {
        ptl_from_sps_fields(
            self.inner.general_tier_flag,
            self.inner.general_profile_idc,
            self.inner.general_profile_compatibility_flags,
            self.inner.general_progressive_source_flag,
            self.inner.general_interlaced_source_flag,
            self.inner.general_non_packed_constraint_flag,
            self.inner.general_frame_only_constraint_flag,
            self.inner.general_level_idc,
        )
    }

    fn __repr__(&self) -> String {
        format!(
            "H265Vps(vps_id={}, profile={}, level={})",
            self.inner.vps_video_parameter_set_id,
            self.inner.general_profile_idc,
            self.inner.general_level_idc,
        )
    }
}

/// Light-weight H.265 slice segment header — fields required for keyframe
/// detection and frame-type classification.
/// Mirrors `tst_core::codec::h265::H265SliceHeaderLight`.
#[pyclass(name = "H265SliceHeaderLight", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H265SliceHeaderLightPy {
    inner: RustH265SliceHeaderLight,
}

#[pymethods]
impl H265SliceHeaderLightPy {
    /// True when `first_slice_segment_in_pic_flag == 1` — start of a new frame.
    #[getter]
    fn first_in_pic(&self) -> bool {
        self.inner.first_in_pic
    }

    /// Slice type (B / P / I / Unknown).
    #[getter]
    fn slice_type(&self) -> H265SliceTypePy {
        self.inner.slice_type.into()
    }

    /// `slice_pic_parameter_set_id` — links this slice to a PPS.
    #[getter]
    fn pps_id(&self) -> u8 {
        self.inner.pps_id
    }

    /// `pic_order_cnt_lsb` read using the bit width from the supplied SPS, or
    /// `None` when no SPS context was passed to `parse_h265_slice_header_light`.
    /// `Some(0)` for IDR slices (implicit per spec).
    #[getter]
    fn pic_order_cnt_lsb(&self) -> Option<u16> {
        self.inner.pic_order_cnt_lsb
    }

    /// True when `nal_unit_type` is IDR_W_RADL (19) or IDR_N_LP (20).
    #[getter]
    fn idr(&self) -> bool {
        self.inner.idr
    }

    /// Original RBSP bytes as supplied to `parse_h265_slice_header_light`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H265SliceHeaderLight(first={}, slice_type={:?}, idr={})",
            self.inner.first_in_pic, self.inner.slice_type, self.inner.idr,
        )
    }
}

/// All VPS, SPS, and PPS NAL units parsed from a slice.
/// Mirrors `tst_core::codec::h265::H265ParameterSets`.
#[pyclass(name = "H265ParameterSets", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H265ParameterSetsPy {
    inner: RustH265ParameterSets,
}

#[pymethods]
impl H265ParameterSetsPy {
    /// Mapping of `vps_id → H265Vps`. Keys are `int`, values are `H265Vps`.
    #[getter]
    fn vps_by_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let dict = pyo3::types::PyDict::new_bound(py);
        for (k, v) in &self.inner.vps_by_id {
            let vps_py = Py::new(py, H265VpsPy { inner: v.clone() })?;
            dict.set_item(*k, vps_py)?;
        }
        Ok(dict)
    }

    /// Mapping of `sps_id → H265Sps`. Keys are `int`, values are `H265Sps`.
    #[getter]
    fn sps_by_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let dict = pyo3::types::PyDict::new_bound(py);
        for (k, v) in &self.inner.sps_by_id {
            let sps_py = Py::new(py, H265SpsPy { inner: v.clone() })?;
            dict.set_item(*k, sps_py)?;
        }
        Ok(dict)
    }

    /// Mapping of `pps_id → H265Pps`. Keys are `int`, values are `H265Pps`.
    #[getter]
    fn pps_by_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let dict = pyo3::types::PyDict::new_bound(py);
        for (k, v) in &self.inner.pps_by_id {
            let pps_py = Py::new(py, H265PpsPy { inner: v.clone() })?;
            dict.set_item(*k, pps_py)?;
        }
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "H265ParameterSets(n_vps={}, n_sps={}, n_pps={})",
            self.inner.vps_by_id.len(),
            self.inner.sps_by_id.len(),
            self.inner.pps_by_id.len()
        )
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
fn parse_h265_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265SpsPy> {
    rust_parse_h265_sps(rbsp)
        .map(|inner| H265SpsPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse a single H.265 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h265_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h265_pps")]
fn parse_h265_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265PpsPy> {
    rust_parse_h265_pps(rbsp)
        .map(|inner| H265PpsPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

/// Parse a single H.265 VPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h265_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h265_vps")]
fn parse_h265_vps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H265VpsPy> {
    rust_parse_h265_vps(rbsp)
        .map(|inner| H265VpsPy { inner })
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
) -> PyResult<H265ParameterSetsPy> {
    use tst_core::mpegts::demux::event::NalUnit as RustNalUnit;
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
        .map(|inner| H265ParameterSetsPy { inner })
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
    sps: Option<&H265SpsPy>,
    nal_unit_type: u8,
) -> PyResult<H265SliceHeaderLightPy> {
    rust_parse_h265_slice_header_light(rbsp, sps.map(|s| &s.inner), nal_unit_type)
        .map(|inner| H265SliceHeaderLightPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h265"))
}

// === H.266 ===

use tst_core::codec::h266::{
    H266ParameterSets as RustH266ParameterSets, H266Pps as RustH266Pps,
    H266SliceHeaderLight as RustH266SliceHeaderLight, H266SliceType as RustH266SliceType,
    H266Sps as RustH266Sps, H266Vps as RustH266Vps,
    parse_parameter_sets as rust_parse_h266_parameter_sets, parse_pps as rust_parse_h266_pps,
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

/// H.266 profile/tier/level fields parsed from an SPS NAL unit.
/// Mirrors the fields from `tst_core::codec::h266::H266ProfileTierLevel`.
///
/// H.266 V4 §7.3.3 PTL carries fewer fields than H.265 — only
/// `general_profile_idc`, `general_tier_flag`, and `general_level_idc` are
/// surfaced here. Both [`H266SpsPy`] exposes the same values via its
/// `profile_tier_level()` method.
#[pyclass(name = "H266ProfileTierLevel", module = "tstrans.codec")]
#[derive(Debug, Clone, Copy)]
pub struct H266ProfileTierLevelPy {
    general_profile_idc: u8,
    general_tier_flag: bool,
    general_level_idc: u8,
}

#[pymethods]
impl H266ProfileTierLevelPy {
    /// 7-bit `general_profile_idc` (H.266 V4 §7.3.3). Common values:
    /// 1=Main10, 2=MultilayerMain10 (H.266 V4 Annex A).
    #[getter]
    fn general_profile_idc(&self) -> u8 {
        self.general_profile_idc
    }

    /// `general_tier_flag` — false = Main tier, true = High tier.
    #[getter]
    fn general_tier_flag(&self) -> bool {
        self.general_tier_flag
    }

    /// `general_level_idc` — H.266 V4 Annex A.4 level table. Encoded as the
    /// level value multiplied by 16: e.g. 64 = Level 4.0, 80 = Level 5.0.
    #[getter]
    fn general_level_idc(&self) -> u8 {
        self.general_level_idc
    }

    fn __repr__(&self) -> String {
        format!(
            "H266ProfileTierLevel(profile_idc={}, tier={}, level_idc={})",
            self.general_profile_idc, self.general_tier_flag, self.general_level_idc,
        )
    }
}

/// Parsed H.266 Sequence Parameter Set.
/// Mirrors `tst_core::codec::h266::H266Sps`.
#[pyclass(name = "H266Sps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H266SpsPy {
    inner: RustH266Sps,
}

#[pymethods]
impl H266SpsPy {
    /// `sps_seq_parameter_set_id` (4-bit) — identifies this SPS (H.266 §7.3.2.4).
    #[getter]
    fn sps_id(&self) -> u8 {
        self.inner.sps_id
    }

    /// `sps_video_parameter_set_id` (4-bit) — links this SPS to a VPS.
    #[getter]
    fn vps_id(&self) -> u8 {
        self.inner.vps_id
    }

    /// Post-crop display width in luma samples (after conformance window is
    /// applied).
    #[getter]
    fn width(&self) -> u32 {
        self.inner.width
    }

    /// Post-crop display height in luma samples.
    #[getter]
    fn height(&self) -> u32 {
        self.inner.height
    }

    /// `general_profile_idc` decoded from the SPS profile_tier_level block
    /// (H.266 V4 §7.3.3). Common values: 1=Main10, 2=MultilayerMain10.
    #[getter]
    fn general_profile_idc(&self) -> u8 {
        self.inner.profile_tier_level.general_profile_idc
    }

    /// `general_tier_flag` — false = Main tier, true = High tier.
    #[getter]
    fn general_tier_flag(&self) -> bool {
        self.inner.profile_tier_level.general_tier_flag
    }

    /// `general_level_idc` — H.266 V4 Annex A.4 level table.
    #[getter]
    fn general_level_idc(&self) -> u8 {
        self.inner.profile_tier_level.general_level_idc
    }

    /// Luma bit depth. In H.266 V4, a single `sps_bitdepth_minus8` encodes
    /// both luma and chroma — they are always equal.
    #[getter]
    fn bit_depth_luma(&self) -> u8 {
        self.inner.bit_depth_luma
    }

    /// Chroma bit depth. Equal to `bit_depth_luma` per H.266 V4 §7.4.3.4
    /// (single `sps_bitdepth_minus8` field covers both planes).
    #[getter]
    fn bit_depth_chroma(&self) -> u8 {
        self.inner.bit_depth_chroma
    }

    /// Chroma subsampling format.
    #[getter]
    fn chroma_format(&self) -> ChromaFormatPy {
        self.inner.chroma_format.into()
    }

    /// Frame rate as `Rational(num, den)`, or `None` when timing_hrd
    /// parameters are absent. In H.266, timing lives in
    /// `general_timing_hrd_parameters()` (§7.3.5.1), not the VUI.
    #[getter]
    fn frame_rate(&self) -> Option<RationalPy> {
        self.inner.frame_rate.map(Into::into)
    }

    /// VUI colour info, or `None` when the VUI is absent or colour_description
    /// is not present. Decoded per H.274 §7.2 (codec-independent colour registry).
    #[getter]
    fn color(&self) -> Option<ColorInfoPy> {
        self.inner.color_info.clone().map(Into::into)
    }

    /// Left crop offset in luma samples (H.266 §7.4.3.4 after SubWidthC
    /// scaling). See `coded_width()` for the pre-crop dimension.
    #[getter]
    fn crop_left(&self) -> u32 {
        self.inner.crop_left
    }

    /// Right crop offset in luma samples.
    #[getter]
    fn crop_right(&self) -> u32 {
        self.inner.crop_right
    }

    /// Top crop offset in luma samples.
    #[getter]
    fn crop_top(&self) -> u32 {
        self.inner.crop_top
    }

    /// Bottom crop offset in luma samples.
    #[getter]
    fn crop_bottom(&self) -> u32 {
        self.inner.crop_bottom
    }

    /// Original RBSP bytes as supplied to `parse_h266_sps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    /// Reconstructed `H266ProfileTierLevel` from the fields decoded inside
    /// this SPS's `profile_tier_level()` block (H.266 V4 §7.3.3).
    fn profile_tier_level(&self) -> H266ProfileTierLevelPy {
        H266ProfileTierLevelPy {
            general_profile_idc: self.inner.profile_tier_level.general_profile_idc,
            general_tier_flag: self.inner.profile_tier_level.general_tier_flag,
            general_level_idc: self.inner.profile_tier_level.general_level_idc,
        }
    }

    /// Pre-crop luma width — `pic_width_max_in_luma_samples` before conformance-
    /// window cropping. Equal to `width + crop_left + crop_right`.
    fn coded_width(&self) -> u32 {
        self.inner.coded_width()
    }

    /// Pre-crop luma height — `pic_height_max_in_luma_samples` before
    /// conformance-window cropping. Equal to `height + crop_top + crop_bottom`.
    fn coded_height(&self) -> u32 {
        self.inner.coded_height()
    }

    fn __repr__(&self) -> String {
        format!(
            "H266Sps(profile={}, level={}, {}x{}, sps_id={})",
            self.inner.profile_tier_level.general_profile_idc,
            self.inner.profile_tier_level.general_level_idc,
            self.inner.width,
            self.inner.height,
            self.inner.sps_id,
        )
    }
}

/// Parsed H.266 Picture Parameter Set.
/// Mirrors `tst_core::codec::h266::H266Pps`.
#[pyclass(name = "H266Pps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H266PpsPy {
    inner: RustH266Pps,
}

#[pymethods]
impl H266PpsPy {
    /// `pps_id` (6-bit) ∈ [0, 63] — H.266 V4 §7.3.2.5.
    #[getter]
    fn pps_id(&self) -> u8 {
        self.inner.pps_id
    }

    /// `sps_id` (4-bit) — links this PPS to an SPS.
    #[getter]
    fn sps_id(&self) -> u8 {
        self.inner.sps_id
    }

    /// Original RBSP bytes as supplied to `parse_h266_pps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H266Pps(pps_id={}, sps_id={})",
            self.inner.pps_id, self.inner.sps_id
        )
    }
}

/// Parsed H.266 Video Parameter Set.
/// Mirrors `tst_core::codec::h266::H266Vps`.
#[pyclass(name = "H266Vps", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H266VpsPy {
    inner: RustH266Vps,
}

#[pymethods]
impl H266VpsPy {
    /// 4-bit `vps_id` — identifies this VPS (H.266 V4 §7.3.2.3).
    #[getter]
    fn vps_id(&self) -> u8 {
        self.inner.vps_id
    }

    /// Maximum number of spatial layers (`vps_max_layers_minus1 + 1`).
    #[getter]
    fn max_layers(&self) -> u8 {
        self.inner.max_layers
    }

    /// Maximum number of temporal sub-layers (`vps_max_sub_layers_minus1 + 1`).
    #[getter]
    fn max_sub_layers(&self) -> u8 {
        self.inner.max_sub_layers
    }

    /// Original RBSP bytes as supplied to `parse_h266_vps`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H266Vps(vps_id={}, max_layers={}, max_sub_layers={})",
            self.inner.vps_id, self.inner.max_layers, self.inner.max_sub_layers,
        )
    }
}

/// Light-weight H.266 slice header — fields required for keyframe detection.
/// Mirrors `tst_core::codec::h266::H266SliceHeaderLight`.
///
/// # Known limitations
///
/// `slice_type` and `pps_id` are returned as **sentinels** — always
/// `H266SliceType.I` and `0` respectively. Accurate extraction requires
/// walking through `picture_header_rbsp()`, whose length is governed by
/// SPS / PPS context fields that the light parser does not carry. This
/// remains deferred work pending a context-tracking H.266 parser.
///
/// Only `idr`, `first_in_pic`, and `pic_order_cnt_lsb` are accurate.
#[pyclass(name = "H266SliceHeaderLight", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H266SliceHeaderLightPy {
    inner: RustH266SliceHeaderLight,
}

#[pymethods]
impl H266SliceHeaderLightPy {
    /// True when `picture_header_in_slice_header_flag == 1` — the picture
    /// header is embedded in this slice, marking the start of a new picture.
    /// This field is **accurate**.
    #[getter]
    fn first_in_pic(&self) -> bool {
        self.inner.first_in_pic
    }

    /// Slice type. **Always returns `H266SliceType.I` as a sentinel.**
    ///
    /// Accurate extraction requires walking through `picture_header_rbsp()`,
    /// whose length depends on SPS / PPS context fields that this light parser
    /// does not carry. Deferred pending a context-tracking H.266 parser.
    #[getter]
    fn slice_type(&self) -> H266SliceTypePy {
        self.inner.slice_type.into()
    }

    /// PPS id. **Always returns `0` as a sentinel** — see `slice_type`
    /// for the same reason and deferral note.
    #[getter]
    fn pps_id(&self) -> u8 {
        self.inner.pps_id
    }

    /// `slice_pic_order_cnt_lsb`. `Some(0)` for IDR slices (implicit per
    /// H.266 spec); `None` for non-IDR slices where SPS context is required
    /// to determine the bit width. This field is **accurate** for IDR slices.
    #[getter]
    fn pic_order_cnt_lsb(&self) -> Option<u16> {
        self.inner.pic_order_cnt_lsb
    }

    /// True when `nal_unit_type` is IDR_W_RADL (7) or IDR_N_LP (8) per
    /// H.266 V4 Table 5. This field is **accurate**.
    #[getter]
    fn idr(&self) -> bool {
        self.inner.idr
    }

    /// Original RBSP bytes as supplied to `parse_h266_slice_header_light`.
    #[getter]
    fn raw_rbsp<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_rbsp)
    }

    fn __repr__(&self) -> String {
        format!(
            "H266SliceHeaderLight(first={}, slice_type={:?}, idr={})",
            self.inner.first_in_pic, self.inner.slice_type, self.inner.idr,
        )
    }
}

/// All VPS, SPS, and PPS NAL units parsed from a sequence.
/// Mirrors `tst_core::codec::h266::H266ParameterSets`.
///
/// Unlike the H.265 version (which uses dict-by-id), H.266 parameter sets
/// are stored as ordered lists — use `vpses[i].vps_id`, `spses[i].sps_id`,
/// and `ppses[i].pps_id` to look up by id.
#[pyclass(name = "H266ParameterSets", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct H266ParameterSetsPy {
    inner: RustH266ParameterSets,
}

#[pymethods]
impl H266ParameterSetsPy {
    /// List of parsed `H266Vps` objects, ordered by `vps_id`.
    #[getter]
    fn vpses(&self, py: Python<'_>) -> PyResult<pyo3::Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty_bound(py);
        for v in &self.inner.vpses {
            list.append(Py::new(py, H266VpsPy { inner: v.clone() })?)?;
        }
        Ok(list.unbind())
    }

    /// List of parsed `H266Sps` objects, ordered by `sps_id`.
    #[getter]
    fn spses(&self, py: Python<'_>) -> PyResult<pyo3::Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty_bound(py);
        for s in &self.inner.spses {
            list.append(Py::new(py, H266SpsPy { inner: s.clone() })?)?;
        }
        Ok(list.unbind())
    }

    /// List of parsed `H266Pps` objects, ordered by `pps_id`.
    #[getter]
    fn ppses(&self, py: Python<'_>) -> PyResult<pyo3::Py<pyo3::types::PyList>> {
        let list = pyo3::types::PyList::empty_bound(py);
        for p in &self.inner.ppses {
            list.append(Py::new(py, H266PpsPy { inner: p.clone() })?)?;
        }
        Ok(list.unbind())
    }

    fn __repr__(&self) -> String {
        format!(
            "H266ParameterSets(n_vps={}, n_sps={}, n_pps={})",
            self.inner.vpses.len(),
            self.inner.spses.len(),
            self.inner.ppses.len()
        )
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
fn parse_h266_sps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266SpsPy> {
    rust_parse_h266_sps(rbsp)
        .map(|inner| H266SpsPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse a single H.266 PPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h266_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h266_pps")]
fn parse_h266_pps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266PpsPy> {
    rust_parse_h266_pps(rbsp)
        .map(|inner| H266PpsPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

/// Parse a single H.266 VPS RBSP.
///
/// `rbsp` must be the raw RBSP body — same contract as `parse_h266_sps`.
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_h266_vps")]
fn parse_h266_vps_py(py: Python<'_>, rbsp: &[u8]) -> PyResult<H266VpsPy> {
    rust_parse_h266_vps(rbsp)
        .map(|inner| H266VpsPy { inner })
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
) -> PyResult<H266ParameterSetsPy> {
    use tst_core::mpegts::demux::event::NalUnit as RustNalUnit;
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
        .map(|inner| H266ParameterSetsPy { inner })
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
    sps: Option<&H266SpsPy>,
    nal_unit_type: u8,
) -> PyResult<H266SliceHeaderLightPy> {
    rust_parse_h266_slice_header_light(rbsp, sps.map(|s| &s.inner), nal_unit_type)
        .map(|inner| H266SliceHeaderLightPy { inner })
        .map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "h266"))
}

// === AV1 ===

use tst_core::codec::av1::{
    Av1FrameHeaderLight as RustAv1FrameHeaderLight, Av1ObuStream as RustAv1ObuStream,
    Av1SequenceHeader as RustAv1SequenceHeader,
    parse_frame_header_light as rust_parse_av1_frame_header_light,
    parse_obu_stream as rust_parse_av1_obu_stream,
    parse_sequence_header as rust_parse_av1_sequence_header,
};
use tst_core::mpegts::demux::event::{Obu as RustObu, ObuExtension as RustObuExtension};

/// Parsed AV1 Sequence Header OBU. Mirrors `tst_core::codec::av1::Av1SequenceHeader`.
#[pyclass(name = "Av1SequenceHeader", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct Av1SequenceHeaderPy {
    inner: RustAv1SequenceHeader,
}

#[pymethods]
impl Av1SequenceHeaderPy {
    /// `seq_profile` — 0=Main, 1=High, 2=Professional.
    #[getter]
    fn profile(&self) -> u8 {
        self.inner.profile
    }

    /// `seq_level_idx[0]` — operating point 0 level index.
    #[getter]
    fn level(&self) -> u8 {
        self.inner.level
    }

    /// `seq_tier[0]` — operating point 0 tier (0 unless level > 7).
    #[getter]
    fn tier(&self) -> u8 {
        self.inner.tier
    }

    /// `max_frame_width_minus_1 + 1`.
    #[getter]
    fn max_frame_width(&self) -> u32 {
        self.inner.max_frame_width
    }

    /// `max_frame_height_minus_1 + 1`.
    #[getter]
    fn max_frame_height(&self) -> u32 {
        self.inner.max_frame_height
    }

    /// 8, 10, or 12 per BitDepth derivation in AV1 §5.5.2.
    #[getter]
    fn bit_depth(&self) -> u8 {
        self.inner.bit_depth
    }

    /// True when `mono_chrome = 1` (Y-only stream).
    #[getter]
    fn monochrome(&self) -> bool {
        self.inner.monochrome
    }

    /// Chroma subsampling format derived from profile + mono_chrome bits.
    #[getter]
    fn chroma_format(&self) -> ChromaFormatPy {
        self.inner.chroma_format.into()
    }

    /// True when `still_picture = 1`.
    #[getter]
    fn still_picture(&self) -> bool {
        self.inner.still_picture
    }

    /// True when `reduced_still_picture_header = 1`.
    #[getter]
    fn reduced_still_picture_header(&self) -> bool {
        self.inner.reduced_still_picture_header
    }

    /// Colour metadata (primaries/transfer/matrix + full_range flag),
    /// or `None` when the wire format contained no color config section.
    /// AV1 always writes a `color_range` bit in the wire format when
    /// `color_description_present_flag == 0`, so a successful parse
    /// always populates this with at least the dynamic-range signal.
    #[getter]
    fn color_info(&self) -> Option<ColorInfoPy> {
        self.inner.color_info.clone().map(Into::into)
    }

    /// Frame rate as `Rational(num, den)`, derived from
    /// `time_scale / num_units_in_display_tick` when
    /// `timing_info_present_flag == 1` and `equal_picture_interval == 1`.
    /// Otherwise `None`.
    #[getter]
    fn frame_rate(&self) -> Option<RationalPy> {
        self.inner.frame_rate.map(Into::into)
    }

    /// Original payload bytes as passed to `parse_av1_sequence_header`.
    #[getter]
    fn raw<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw)
    }

    fn __repr__(&self) -> String {
        format!(
            "Av1SequenceHeader(profile={}, {}x{}, bit_depth={})",
            self.inner.profile,
            self.inner.max_frame_width,
            self.inner.max_frame_height,
            self.inner.bit_depth,
        )
    }
}

/// Light-weight AV1 Frame Header.
/// Mirrors `tst_core::codec::av1::Av1FrameHeaderLight`.
///
/// Light scope: `frame_type` + `show_frame` + `show_existing_frame` only.
/// `frame_size` is always `None` — full per-frame size extraction requires
/// reference-frame management beyond this parser's scope.
#[pyclass(name = "Av1FrameHeaderLight", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct Av1FrameHeaderLightPy {
    inner: RustAv1FrameHeaderLight,
}

#[pymethods]
impl Av1FrameHeaderLightPy {
    /// `frame_type` per AV1 §5.9.1: 0=KEY_FRAME, 1=INTER_FRAME,
    /// 2=INTRA_ONLY_FRAME, 3=SWITCH_FRAME.
    #[getter]
    fn frame_type(&self) -> u8 {
        self.inner.frame_type
    }

    /// True when the decoded frame is displayed immediately.
    #[getter]
    fn show_frame(&self) -> bool {
        self.inner.show_frame
    }

    /// True when this OBU references a previously decoded frame for display.
    #[getter]
    fn show_existing_frame(&self) -> bool {
        self.inner.show_existing_frame
    }

    /// Per-frame size override, or `None` in the current light scope.
    #[getter]
    fn frame_size(&self) -> Option<(u32, u32)> {
        self.inner.frame_size
    }

    /// Original payload bytes as passed to `parse_av1_frame_header_light`.
    #[getter]
    fn raw<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw)
    }

    fn __repr__(&self) -> String {
        format!(
            "Av1FrameHeaderLight(type={}, show={}, show_existing={})",
            self.inner.frame_type, self.inner.show_frame, self.inner.show_existing_frame,
        )
    }
}

/// Aggregate of all typed structs extracted from a sequence of AV1 OBUs.
/// Mirrors `tst_core::codec::av1::Av1ObuStream`.
///
/// Build a list of `Obu` objects, then call `parse_av1_obu_stream` to
/// populate the three fields. Partial-success-tolerant: OBUs that fail
/// to parse accumulate in `unparseable` rather than aborting the walk.
#[pyclass(name = "Av1ObuStream", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct Av1ObuStreamPy {
    inner: RustAv1ObuStream,
}

#[pymethods]
impl Av1ObuStreamPy {
    /// All successfully parsed Sequence Header OBUs in encounter order.
    #[getter]
    fn sequence_headers(&self) -> Vec<Av1SequenceHeaderPy> {
        self.inner
            .sequence_headers
            .iter()
            .cloned()
            .map(|inner| Av1SequenceHeaderPy { inner })
            .collect()
    }

    /// All successfully parsed Frame Header OBUs in encounter order.
    #[getter]
    fn frame_headers(&self) -> Vec<Av1FrameHeaderLightPy> {
        self.inner
            .frame_headers
            .iter()
            .cloned()
            .map(|inner| Av1FrameHeaderLightPy { inner })
            .collect()
    }

    /// List of `(obu_type, error_message)` for each OBU that failed to parse.
    /// Frame Header OBUs arriving before any Sequence Header land here with a
    /// synthesised "frame header before sequence header" error message.
    #[getter]
    fn unparseable(&self) -> Vec<(u8, String)> {
        self.inner
            .unparseable
            .iter()
            .map(|(t, e)| (*t, format!("{e}")))
            .collect()
    }
}

/// Parse an AV1 Sequence Header OBU body.
///
/// `payload` carries the OBU body bytes — the OBU header byte and any
/// LEB128 `obu_size` prefix are stripped before calling this function
/// (as `Obu.payload` provides from a demuxed stream).
///
/// Raises `CodecError` on parse failure.
#[pyfunction]
#[pyo3(name = "parse_av1_sequence_header")]
fn parse_av1_sequence_header_py(py: Python<'_>, payload: &[u8]) -> PyResult<Av1SequenceHeaderPy> {
    rust_parse_av1_sequence_header(payload)
        .map(|inner| Av1SequenceHeaderPy { inner })
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
    seq: &Av1SequenceHeaderPy,
) -> PyResult<Av1FrameHeaderLightPy> {
    rust_parse_av1_frame_header_light(payload, &seq.inner)
        .map(|inner| Av1FrameHeaderLightPy { inner })
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
fn parse_av1_obu_stream_py(obus: Vec<ObuPy>) -> Av1ObuStreamPy {
    let rust_obus: Vec<RustObu> = obus
        .into_iter()
        .map(|o| RustObu {
            obu_type: o.obu_type,
            extension: o.extension.map(|ext| RustObuExtension {
                temporal_id: ext.temporal_id,
                spatial_id: ext.spatial_id,
            }),
            payload: o.payload.into(),
        })
        .collect();
    let inner = rust_parse_av1_obu_stream(&rust_obus);
    Av1ObuStreamPy { inner }
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

/// Decoded ADTS frame. Wraps `tst_core::codec::aac::AdtsFrameOwned`.
///
/// The `payload` getter returns the full frame bytes (header + body) sourced
/// from `AdtsFrameOwned.body` — the Rust field name for the owned body slice.
#[pyclass(name = "AdtsFrame", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct AdtsFramePy {
    pub(crate) inner: RustAdtsFrameOwned,
}

#[pymethods]
impl AdtsFramePy {
    /// AAC profile (Main / LC / SSR / LTP).
    #[getter]
    fn profile(&self) -> AacProfilePy {
        self.inner.profile.into()
    }

    /// Sample rate in Hz (e.g. 44100, 48000).
    #[getter]
    fn sample_rate_hz(&self) -> u32 {
        self.inner.sample_rate_hz
    }

    /// Raw 3-bit `channel_configuration` field from the ADTS header.
    /// `0` = PCE-defined; `1..=7` = canonical channel counts.
    #[getter]
    fn channel_configuration(&self) -> u8 {
        self.inner.channel_configuration
    }

    /// Typed channel layout.
    #[getter]
    fn channel_layout(&self) -> AacChannelLayoutPy {
        self.inner.channel_layout.into()
    }

    /// Total frame byte count (header + body), as encoded in the ADTS header.
    #[getter]
    fn frame_length_bytes(&self) -> u32 {
        self.inner.frame_length_bytes
    }

    /// Number of PCM samples in the frame (1024 for standard ADTS).
    #[getter]
    fn samples_per_frame(&self) -> u16 {
        self.inner.samples_per_frame
    }

    /// Number of raw data blocks in the frame (logical, not wire value).
    #[getter]
    fn num_raw_data_blocks(&self) -> u8 {
        self.inner.num_raw_data_blocks
    }

    /// True when a 16-bit CRC follows the 7-byte fixed header.
    #[getter]
    fn has_crc(&self) -> bool {
        self.inner.has_crc
    }

    /// MPEG version: `MpegVersion.MPEG2` or `MpegVersion.MPEG4`.
    #[getter]
    fn mpeg_version(&self) -> MpegVersionPy {
        self.inner.mpeg_version.into()
    }

    /// Raw ADTS header bytes (7 bytes without CRC, 9 with CRC).
    #[getter]
    fn raw_header<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_header)
    }

    /// Full frame bytes (header + body). Sources from `AdtsFrameOwned.body`.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.body)
    }

    fn __repr__(&self) -> String {
        format!(
            "AdtsFrame(profile={:?}, sr={}, ch={}, frame_len={})",
            self.inner.profile,
            self.inner.sample_rate_hz,
            self.inner.channel_configuration,
            self.inner.frame_length_bytes,
        )
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

    fn __next__(&mut self) -> Option<AdtsFramePy> {
        if self.index < self.frames.len() {
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Some(AdtsFramePy { inner: frame })
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
fn parse_aac_frames_py(py: Python<'_>, bytes_buf: &[u8]) -> PyResult<Vec<AdtsFramePy>> {
    // GIL-release rationale: see `iter_aac_frames_py`. Parse + own-copy
    // off-thread, then map errors to PyErr inside the GIL (cheap).
    let frames = py.allow_threads(|| {
        rust_aac_frames(bytes_buf)
            .map(|res| res.map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()
    });
    let frames = frames.map_err(|e| crate::errors::codec_parse_error_to_pyerr(py, &e, "aac"))?;
    Ok(frames
        .into_iter()
        .map(|inner| AdtsFramePy { inner })
        .collect())
}

/// Eagerly parse all ADTS frames from `bytes_buf`, skipping parse errors
/// (best-effort — never raises).
///
/// Uses `frames_with_resync` internally; only successfully decoded frames
/// appear in the returned list. Suitable for stats / telemetry use where
/// dropping a frame on corruption is preferable to aborting the parse.
#[pyfunction]
#[pyo3(name = "parse_aac_frames_with_resync")]
fn parse_aac_frames_with_resync_py(py: Python<'_>, bytes_buf: &[u8]) -> Vec<AdtsFramePy> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_aac_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    frames
        .into_iter()
        .map(|inner| AdtsFramePy { inner })
        .collect()
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

/// Decoded MPEG audio frame. Wraps `tst_core::codec::mpegaudio::FrameOwned`.
///
/// The `payload` getter returns the full frame bytes (header + body) sourced
/// from `FrameOwned.body` — the Rust field name for the owned body slice.
/// The `raw_header` getter returns the 4-byte fixed-size header array.
#[pyclass(name = "Mpeg2AudioFrame", module = "tstrans.codec")]
#[derive(Debug, Clone)]
pub struct Mpeg2AudioFramePy {
    pub(crate) inner: RustMpeg2AudioFrameOwned,
}

#[pymethods]
impl Mpeg2AudioFramePy {
    /// MPEG audio layer (I, II, or III).
    #[getter]
    fn layer(&self) -> LayerPy {
        self.inner.layer.into()
    }

    /// MPEG audio version (MPEG-1, MPEG-2, or MPEG-2.5).
    #[getter]
    fn version(&self) -> VersionPy {
        self.inner.version.into()
    }

    /// Bitrate in kilobits per second (e.g. 128, 192, 320).
    #[getter]
    fn bitrate_kbps(&self) -> u32 {
        self.inner.bitrate_kbps
    }

    /// Sample rate in Hz (e.g. 44100, 48000).
    #[getter]
    fn sample_rate_hz(&self) -> u32 {
        self.inner.sample_rate_hz
    }

    /// Channel mode (Stereo, JointStereo, DualChannel, or Mono).
    #[getter]
    fn channel_mode(&self) -> ChannelModePy {
        self.inner.channel_mode.into()
    }

    /// Number of audio channels (1 for Mono, 2 for all other modes).
    #[getter]
    fn channels(&self) -> u8 {
        self.inner.channels
    }

    /// Total frame byte count as computed from the header fields.
    #[getter]
    fn frame_length_bytes(&self) -> u32 {
        self.inner.frame_length_bytes
    }

    /// Number of PCM samples encoded in this frame.
    ///
    /// Depends on (version, layer): Layer I = 384; Layer II = 1152;
    /// Layer III MPEG-1 = 1152; Layer III MPEG-2/2.5 = 576.
    #[getter]
    fn samples_per_frame(&self) -> u16 {
        self.inner.samples_per_frame
    }

    /// True when a 16-bit CRC follows the 4-byte header
    /// (protection_bit == 0 in the raw header).
    #[getter]
    fn has_crc(&self) -> bool {
        self.inner.has_crc
    }

    /// Raw 4-byte MPEG audio frame header bytes.
    #[getter]
    fn raw_header<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.raw_header)
    }

    /// Full frame bytes (header + body). Sources from `FrameOwned.body`.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new_bound(py, &self.inner.body)
    }

    fn __repr__(&self) -> String {
        format!(
            "Mpeg2AudioFrame(layer={:?}, version={:?}, bitrate={}kbps, sr={}Hz, ch_mode={:?}, frame_len={})",
            self.inner.layer,
            self.inner.version,
            self.inner.bitrate_kbps,
            self.inner.sample_rate_hz,
            self.inner.channel_mode,
            self.inner.frame_length_bytes,
        )
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

    fn __next__(&mut self) -> Option<Mpeg2AudioFramePy> {
        if self.index < self.frames.len() {
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Some(Mpeg2AudioFramePy { inner: frame })
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
fn parse_mpeg2_audio_frames_py(
    py: Python<'_>,
    bytes_buf: &[u8],
) -> PyResult<Vec<Mpeg2AudioFramePy>> {
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
        .map(|inner| Mpeg2AudioFramePy { inner })
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
) -> Vec<Mpeg2AudioFramePy> {
    // GIL-release rationale: see `iter_aac_frames_py`.
    let frames: Vec<_> = py.allow_threads(|| {
        rust_mpeg2audio_frames_with_resync(bytes_buf)
            .filter_map(|res| res.ok())
            .map(|f| f.to_owned())
            .collect()
    });
    frames
        .into_iter()
        .map(|inner| Mpeg2AudioFramePy { inner })
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
                    list.append(Py::new(py, AdtsFramePy { inner: f })?)?;
                }
            } else {
                let frames: Vec<_> = py.allow_threads(|| {
                    rust_aac_frames_with_resync(raw)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect()
                });
                for f in frames {
                    list.append(Py::new(py, AdtsFramePy { inner: f })?)?;
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
                    list.append(Py::new(py, Mpeg2AudioFramePy { inner: f })?)?;
                }
            } else {
                let frames: Vec<_> = py.allow_threads(|| {
                    rust_mpeg2audio_frames_with_resync(raw)
                        .filter_map(|res| res.ok())
                        .map(|f| f.to_owned())
                        .collect()
                });
                for f in frames {
                    list.append(Py::new(py, Mpeg2AudioFramePy { inner: f })?)?;
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

/// One MISP timestamp destined for (or extracted from) a video SEI.
///
/// Construct via `MispTimestamp.micros(value_us, time_status)` or
/// `MispTimestamp.nanos(value_ns, time_status)`.  Direct construction
/// via `MispTimestamp(kind, time_status, value)` is also accepted.
///
/// `kind` — `MispTimeKind.MICRO` or `MispTimeKind.NANO`.
/// `time_status` — MISB ST 0603 Time Status byte (0x00 = unknown).
/// `value` — microseconds or nanoseconds since the MISP epoch.
#[pyclass(name = "MispTimestamp", module = "tstrans.codec", frozen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MispTimestampPy {
    pub kind: MispTimeKindPy,
    pub time_status: u8,
    pub value: u64,
}

#[pymethods]
impl MispTimestampPy {
    #[new]
    fn new(kind: MispTimeKindPy, time_status: u8, value: u64) -> Self {
        Self {
            kind,
            time_status,
            value,
        }
    }

    #[getter]
    fn kind(&self) -> MispTimeKindPy {
        self.kind
    }

    #[getter]
    fn time_status(&self) -> u8 {
        self.time_status
    }

    #[getter]
    fn value(&self) -> u64 {
        self.value
    }

    /// Construct a microsecond-precision timestamp (valid for H.264 + H.265).
    #[staticmethod]
    fn micros(value_us: u64, time_status: u8) -> Self {
        Self {
            kind: MispTimeKindPy::Micro,
            time_status,
            value: value_us,
        }
    }

    /// Construct a nanosecond-precision timestamp (H.265-only, ST 0604.6 §12.2).
    #[staticmethod]
    fn nanos(value_ns: u64, time_status: u8) -> Self {
        Self {
            kind: MispTimeKindPy::Nano,
            time_status,
            value: value_ns,
        }
    }

    fn __repr__(&self) -> String {
        let kind_str = match self.kind {
            MispTimeKindPy::Micro => "MispTimeKind.MICRO",
            MispTimeKindPy::Nano => "MispTimeKind.NANO",
        };
        format!(
            "MispTimestamp(kind={}, time_status={:#04X}, value={})",
            kind_str, self.time_status, self.value
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

impl From<tst_core::codec::misp_time::MispTimestamp> for MispTimestampPy {
    fn from(ts: tst_core::codec::misp_time::MispTimestamp) -> Self {
        Self {
            kind: MispTimeKindPy::from(ts.kind),
            time_status: ts.time_status,
            value: ts.value,
        }
    }
}

impl From<MispTimestampPy> for tst_core::codec::misp_time::MispTimestamp {
    fn from(ts: MispTimestampPy) -> Self {
        use tst_core::codec::misp_time::MispTimestamp;
        match ts.kind {
            MispTimeKindPy::Micro => MispTimestamp::micros(ts.value, ts.time_status),
            MispTimeKindPy::Nano => MispTimestamp::nanos(ts.value, ts.time_status),
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
) -> PyResult<Option<MispTimestampPy>> {
    use tst_core::codec::misp_time::{MispTimeExtractError, extract};
    let coerced = crate::util::coerce_bytes_like(py, au)?;
    let au_bytes = coerced.as_bytes();
    // Bridge via the demux codec converter, then to the mux codec via From.
    let demux_codec = py_video_codec_to_rust(py, codec)?;
    let mux_codec = tst_core::mpegts::mux::VideoCodec::from(demux_codec);
    let result = py.allow_threads(|| extract(au_bytes, mux_codec));
    match result {
        Ok(None) => Ok(None),
        Ok(Some(ts)) => Ok(Some(MispTimestampPy::from(ts))),
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
    // Shared structs
    m.add_class::<RationalPy>()?;
    m.add_class::<ColorInfoPy>()?;
    // Typed NAL / OBU wrappers (used on Sample.payload)
    m.add_class::<NalUnitPy>()?;
    m.add_class::<ObuExtensionPy>()?;
    m.add_class::<ObuPy>()?;
    // H.264 enums
    m.add_class::<EntropyCodingModePy>()?;
    m.add_class::<H264SliceTypePy>()?;
    // H.264 structs
    m.add_class::<H264SpsPy>()?;
    m.add_class::<H264PpsPy>()?;
    m.add_class::<H264SliceHeaderLightPy>()?;
    m.add_class::<H264ParameterSetsPy>()?;
    // H.264 parser functions
    m.add_function(wrap_pyfunction!(parse_h264_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h264_slice_header_light_py, m)?)?;
    // H.265 enums
    m.add_class::<H265SliceTypePy>()?;
    // H.265 structs
    m.add_class::<H265ProfileTierLevelPy>()?;
    m.add_class::<H265SpsPy>()?;
    m.add_class::<H265PpsPy>()?;
    m.add_class::<H265VpsPy>()?;
    m.add_class::<H265SliceHeaderLightPy>()?;
    m.add_class::<H265ParameterSetsPy>()?;
    // H.265 parser functions
    m.add_function(wrap_pyfunction!(parse_h265_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_vps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h265_slice_header_light_py, m)?)?;
    // H.266 enums
    m.add_class::<H266SliceTypePy>()?;
    // H.266 structs
    m.add_class::<H266ProfileTierLevelPy>()?;
    m.add_class::<H266SpsPy>()?;
    m.add_class::<H266PpsPy>()?;
    m.add_class::<H266VpsPy>()?;
    m.add_class::<H266SliceHeaderLightPy>()?;
    m.add_class::<H266ParameterSetsPy>()?;
    // H.266 parser functions
    m.add_function(wrap_pyfunction!(parse_h266_sps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_pps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_vps_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_parameter_sets_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_h266_slice_header_light_py, m)?)?;
    // AV1 structs
    m.add_class::<Av1SequenceHeaderPy>()?;
    m.add_class::<Av1FrameHeaderLightPy>()?;
    m.add_class::<Av1ObuStreamPy>()?;
    // AV1 parser functions
    m.add_function(wrap_pyfunction!(parse_av1_sequence_header_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_av1_frame_header_light_py, m)?)?;
    m.add_function(wrap_pyfunction!(parse_av1_obu_stream_py, m)?)?;
    // AAC enums + channel layout class
    m.add_class::<AacProfilePy>()?;
    m.add_class::<MpegVersionPy>()?;
    m.add_class::<AacChannelLayoutPy>()?;
    // AAC frame + iterator
    m.add_class::<AdtsFramePy>()?;
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
    m.add_class::<Mpeg2AudioFramePy>()?;
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
    m.add_class::<MispTimestampPy>()?;
    m.add_function(wrap_pyfunction!(extract_misp_timestamp, m)?)?;
    Ok(())
}
