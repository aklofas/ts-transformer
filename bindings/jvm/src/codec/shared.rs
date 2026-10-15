//! Shared JNI builders that map `tst_core::codec` value types and
//! `tst_core::mpegts::demux` payload units to their `org.tstrans.codec` Java
//! counterparts. Exercised by the per-codec parser tasks (which build the
//! `NalUnit`/`Obu`/`ColorInfo`/enum objects to hand back to the JVM); declared
//! `pub(crate)` here so those tasks can call them.
//!
//! Each enum builder matches all `tst_core` variants plus a `_ =>` wildcard to
//! the `RESERVED` / `INVALID` catch-all, exactly as the tst-py `From` impls do.

use jni::JNIEnv;
use jni::objects::{JObject, JValue};
use tst_core::codec::{
    ChromaFormat, ColorInfo, ColourPrimaries, MatrixCoefficients, Rational, TransferCharacteristics,
};
use tst_core::codec::{NalUnit, Obu};

use crate::jutil::enum_const;

/// Map `ChromaFormat` → `org.tstrans.codec.ChromaFormat`. The Rust enum has no
/// reserved/catch-all arm, so the mapping is exhaustive; the Java `INVALID`
/// constant exists for the open-enum mirror but is unreachable from this path
/// (matches tst-py's `ChromaFormatPy::from`, which is likewise exhaustive).
pub(crate) fn build_chroma_format<'local>(
    env: &mut JNIEnv<'local>,
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
    env: &mut JNIEnv<'local>,
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
    env: &mut JNIEnv<'local>,
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
    env: &mut JNIEnv<'local>,
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

/// Build `org.tstrans.codec.Rational(long, long)` from a `Rational`.
pub(crate) fn build_rational<'local>(
    env: &mut JNIEnv<'local>,
    r: &Rational,
) -> Result<JObject<'local>, ()> {
    env.new_object(
        "org/tstrans/codec/Rational",
        "(JJ)V",
        &[
            JValue::Long(i64::from(r.num)),
            JValue::Long(i64::from(r.den)),
        ],
    )
    .map_err(|_| ())
}

/// Build `org.tstrans.codec.ColorInfo(primaries, transfer, matrix, fullRange)`.
/// Only the binding-exposed subset is forwarded (chroma_loc /
/// sample_aspect_ratio are intentionally dropped, matching tst-py's ColorInfoPy).
pub(crate) fn build_color_info<'local>(
    env: &mut JNIEnv<'local>,
    c: &ColorInfo,
) -> Result<JObject<'local>, ()> {
    // 8 slots: the 3 enum constants + their lookups + scratch.
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let primaries = build_colour_primaries(env, c.primaries)?;
    let transfer = build_transfer(env, c.transfer)?;
    let matrix = build_matrix(env, c.matrix)?;
    env.new_object(
        "org/tstrans/codec/ColorInfo",
        "(Lorg/tstrans/codec/ColourPrimaries;\
Lorg/tstrans/codec/TransferCharacteristics;\
Lorg/tstrans/codec/MatrixCoefficients;Z)V",
        &[
            JValue::Object(&primaries),
            JValue::Object(&transfer),
            JValue::Object(&matrix),
            JValue::Bool(u8::from(c.full_range)),
        ],
    )
    .map_err(|_| ())
}

/// Build `org.tstrans.codec.NalUnit` from a `tst_core` demux `NalUnit` by
/// calling the matching Java static factory (`h264`/`h265`/`h266`), mirroring
/// `NalUnitPy::make_h264/h265/h266`.
pub(crate) fn build_nal_unit<'local>(
    env: &mut JNIEnv<'local>,
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
    env.call_static_method("org/tstrans/codec/NalUnit", factory, sig, &args)
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build `org.tstrans.codec.Obu(obuType, extension, payload)` from a `tst_core`
/// demux `Obu`. `extension` is null when absent.
pub(crate) fn build_obu<'local>(env: &mut JNIEnv<'local>, o: &Obu) -> Result<JObject<'local>, ()> {
    let extension = match &o.extension {
        Some(ext) => env
            .new_object(
                "org/tstrans/codec/ObuExtension",
                "(II)V",
                &[
                    JValue::Int(i32::from(ext.temporal_id)),
                    JValue::Int(i32::from(ext.spatial_id)),
                ],
            )
            .map_err(|_| ())?,
        None => JObject::null(),
    };
    let arr = env.byte_array_from_slice(&o.payload).map_err(|_| ())?;
    let payload_buf = env
        .call_static_method(
            "java/nio/ByteBuffer",
            "wrap",
            "([B)Ljava/nio/ByteBuffer;",
            &[JValue::Object(&arr)],
        )
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())?;
    env.new_object(
        "org/tstrans/codec/Obu",
        "(ILorg/tstrans/codec/ObuExtension;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(o.obu_type)),
            JValue::Object(&extension),
            JValue::Object(&payload_buf),
        ],
    )
    .map_err(|_| ())
}

/// Call a fluent Builder setter that takes an `int` parameter and returns the
/// builder (discarded — caller chains via `?`). The full JNI descriptor is
/// assembled from `ret_desc` (e.g. `"Lorg/tstrans/codec/H264Sps$Builder;"`).
pub(crate) fn builder_set_int<'local>(
    env: &mut JNIEnv<'local>,
    b: &JObject<'local>,
    ret_desc: &str,
    method: &str,
    v: i32,
) -> Result<(), ()> {
    let sig = format!("(I){ret_desc}");
    env.call_method(b, method, &sig, &[JValue::Int(v)])
        .map_err(|_| ())?;
    Ok(())
}

/// Like [`builder_set_int`] but for a `long` parameter.
pub(crate) fn builder_set_long<'local>(
    env: &mut JNIEnv<'local>,
    b: &JObject<'local>,
    ret_desc: &str,
    method: &str,
    v: i64,
) -> Result<(), ()> {
    let sig = format!("(J){ret_desc}");
    env.call_method(b, method, &sig, &[JValue::Long(v)])
        .map_err(|_| ())?;
    Ok(())
}

/// Like [`builder_set_int`] but for a `boolean` parameter.
pub(crate) fn builder_set_bool<'local>(
    env: &mut JNIEnv<'local>,
    b: &JObject<'local>,
    ret_desc: &str,
    method: &str,
    v: bool,
) -> Result<(), ()> {
    let sig = format!("(Z){ret_desc}");
    env.call_method(b, method, &sig, &[JValue::Bool(u8::from(v))])
        .map_err(|_| ())?;
    Ok(())
}
