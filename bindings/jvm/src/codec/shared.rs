//! Shared JNI builders that map `tst_core::codec` value types and
//! `tst_core::mpegts::demux` payload units to their `org.tstrans.codec` Java
//! counterparts. Exercised by the per-codec parser tasks (which build the
//! `NalUnit`/`Obu`/`ColorInfo`/enum objects to hand back to the JVM); declared
//! `pub(crate)` here so those tasks can call them.
//!
//! Each enum builder matches all `tst_core` variants plus a `_ =>` wildcard to
//! the `RESERVED` / `INVALID` catch-all, exactly as the tst-py `From` impls do.

use crate::jutil::FindClass;
use jni::Env;
use jni::jni_str;
use jni::objects::{JObject, JValue};
use jni::signature::RuntimeMethodSignature;
use jni::strings::JNIString;
use tst_core::codec::NalUnit;
use tst_core::codec::{ChromaFormat, ColourPrimaries, MatrixCoefficients, TransferCharacteristics};

use crate::jutil::enum_const;

/// Map `ChromaFormat` → `org.tstrans.codec.ChromaFormat`. The Rust enum has no
/// reserved/catch-all arm, so the mapping is exhaustive; the Java `INVALID`
/// constant exists for the open-enum mirror but is unreachable from this path
/// (matches tst-py's `ChromaFormatPy::from`, which is likewise exhaustive).
pub(crate) fn build_chroma_format<'local>(
    env: &mut Env<'local>,
    v: ChromaFormat,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        ChromaFormat::Monochrome => "MONOCHROME",
        ChromaFormat::Yuv420 => "YUV420",
        ChromaFormat::Yuv422 => "YUV422",
        ChromaFormat::Yuv444 => "YUV444",
    };
    enum_const(env, "codec", "ChromaFormat", name)
}

/// Map `ColourPrimaries` → `org.tstrans.codec.ColourPrimaries`. Wildcard → `RESERVED`.
pub(crate) fn build_colour_primaries<'local>(
    env: &mut Env<'local>,
    v: ColourPrimaries,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        ColourPrimaries::Bt709 => "BT709",
        ColourPrimaries::Unspecified => "UNSPECIFIED",
        ColourPrimaries::Bt470M => "BT470M",
        ColourPrimaries::Bt470Bg => "BT470BG",
        ColourPrimaries::Smpte170M => "SMPTE170M",
        ColourPrimaries::Smpte240M => "SMPTE240M",
        ColourPrimaries::Film => "FILM",
        ColourPrimaries::Bt2020 => "BT2020",
        ColourPrimaries::SmpteSt428 => "SMPTE_ST428",
        ColourPrimaries::SmpteRp431_2 => "SMPTE_RP431_2",
        ColourPrimaries::SmpteEg432_1 => "SMPTE_EG432_1",
        ColourPrimaries::Ebu3213E => "EBU3213E",
        _ => "RESERVED",
    };
    enum_const(env, "codec", "ColourPrimaries", name)
}

/// Map `TransferCharacteristics` → Java. Wildcard → `RESERVED`.
pub(crate) fn build_transfer<'local>(
    env: &mut Env<'local>,
    v: TransferCharacteristics,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        TransferCharacteristics::Bt709 => "BT709",
        TransferCharacteristics::Unspecified => "UNSPECIFIED",
        TransferCharacteristics::Gamma22 => "GAMMA22",
        TransferCharacteristics::Gamma28 => "GAMMA28",
        TransferCharacteristics::Smpte170M => "SMPTE170M",
        TransferCharacteristics::Smpte240M => "SMPTE240M",
        TransferCharacteristics::Linear => "LINEAR",
        TransferCharacteristics::Log100 => "LOG100",
        TransferCharacteristics::LogSqrt => "LOG_SQRT",
        TransferCharacteristics::Iec61966_2_4 => "IEC61966_2_4",
        TransferCharacteristics::Bt1361E => "BT1361E",
        TransferCharacteristics::Iec61966_2_1 => "IEC61966_2_1",
        TransferCharacteristics::Bt2020Bits10 => "BT2020_BITS10",
        TransferCharacteristics::Bt2020Bits12 => "BT2020_BITS12",
        TransferCharacteristics::SmpteSt2084 => "SMPTE_ST2084",
        TransferCharacteristics::SmpteSt428 => "SMPTE_ST428",
        TransferCharacteristics::AribStdB67 => "ARIB_STD_B67",
        _ => "RESERVED",
    };
    enum_const(env, "codec", "TransferCharacteristics", name)
}

/// Map `MatrixCoefficients` → Java. Wildcard → `RESERVED`.
pub(crate) fn build_matrix<'local>(
    env: &mut Env<'local>,
    v: MatrixCoefficients,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        MatrixCoefficients::Identity => "IDENTITY",
        MatrixCoefficients::Bt709 => "BT709",
        MatrixCoefficients::Unspecified => "UNSPECIFIED",
        MatrixCoefficients::FccMc => "FCC_MC",
        MatrixCoefficients::Bt470Bg => "BT470BG",
        MatrixCoefficients::Smpte170M => "SMPTE170M",
        MatrixCoefficients::Smpte240M => "SMPTE240M",
        MatrixCoefficients::YCgCo => "YCGCO",
        MatrixCoefficients::Bt2020NonConstant => "BT2020_NON_CONSTANT",
        MatrixCoefficients::Bt2020Constant => "BT2020_CONSTANT",
        MatrixCoefficients::SmpteSt2085 => "SMPTE_ST2085",
        MatrixCoefficients::ChromaDerivedNonConstant => "CHROMA_DERIVED_NON_CONSTANT",
        MatrixCoefficients::ChromaDerivedConstant => "CHROMA_DERIVED_CONSTANT",
        MatrixCoefficients::IctCp => "ICTCP",
        MatrixCoefficients::IptC2 => "IPT_C2",
        MatrixCoefficients::YCgCoRe => "YCGCO_RE",
        MatrixCoefficients::YCgCoRo => "YCGCO_RO",
        _ => "RESERVED",
    };
    enum_const(env, "codec", "MatrixCoefficients", name)
}

/// Build `org.tstrans.codec.NalUnit` from a `tst_core` demux `NalUnit` by
/// calling the matching Java static factory (`h264`/`h265`/`h266`), mirroring
/// `NalUnitPy::make_h264/h265/h266`.
pub(crate) fn build_nal_unit<'local>(
    env: &mut Env<'local>,
    n: &NalUnit,
) -> Result<JObject<'local>, ()> {
    let (factory, sig, nal_type, a, b, payload): (&str, &str, u8, u8, u8, &[u8]) = match n {
        NalUnit::H264 {
            nal_type,
            ref_idc,
            payload,
        } => (
            "h264",
            "(II[B)Lorg/tstrans/codec/NalUnit;",
            *nal_type,
            *ref_idc,
            0,
            payload,
        ),
        NalUnit::H265 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        } => (
            "h265",
            "(III[B)Lorg/tstrans/codec/NalUnit;",
            *nal_type,
            *layer_id,
            *temporal_id_plus1,
            payload,
        ),
        NalUnit::H266 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        } => (
            "h266",
            "(III[B)Lorg/tstrans/codec/NalUnit;",
            *nal_type,
            *layer_id,
            *temporal_id_plus1,
            payload,
        ),
    };
    let arr = env.byte_array_from_slice(payload).map_err(|_| ())?;
    // H.264 factory is (nalType, refIdc, payload); H.265/H.266 are
    // (nalType, layerId, temporalIdPlus1, payload). The H.264 call passes only
    // two ints, so build the arg list per arity.
    let args: Vec<JValue> = if factory == "h264" {
        vec![
            JValue::Int(i32::from(nal_type)),
            JValue::Int(i32::from(a)),
            JValue::Object(&arr),
        ]
    } else {
        vec![
            JValue::Int(i32::from(nal_type)),
            JValue::Int(i32::from(a)),
            JValue::Int(i32::from(b)),
            JValue::Object(&arr),
        ]
    };
    env.call_static_method(
        FindClass(jni_str!("org/tstrans/codec/NalUnit")),
        JNIString::from(factory),
        RuntimeMethodSignature::from_str(sig)
            .map_err(|_| ())?
            .method_signature(),
        &args,
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}
