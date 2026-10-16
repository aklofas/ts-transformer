//! JNI surface for `org.tstrans.codec.Codec`'s H.264 parser entry points.
//!
//! Four natives, each mirroring a tst-py `parse_h264_*` free function:
//!
//! - `nParseH264Sps(byte[]) -> H264Sps` → `tst_core::codec::h264::parse_sps`
//! - `nParseH264Pps(byte[]) -> H264Pps` → `parse_pps`
//! - `nParseH264SliceHeaderLight(byte[], H264Sps, int) -> H264SliceHeaderLight`
//!   → `parse_slice_header_light`
//! - `nParseH264ParameterSets(List<NalUnit>) -> H264ParameterSets`
//!   → `parse_parameter_sets`
//!
//! On a parse error each native calls [`crate::error::map_codec_parse_error`]
//! (which throws `CodecParseException`) and returns a null object — the Java
//! wrapper on `Codec` is declared `throws CodecParseException` so the pending
//! exception propagates.
//!
//! ### Slice-header SPS context
//!
//! `parse_slice_header_light` needs a `&tst_core H264Sps` only to read
//! `frame_num`'s bit width (`log2_max_frame_num_minus4 + 4`). The Java
//! `H264Sps` record cannot hold the Rust struct, so when the caller passes a
//! non-null `sps` we re-`parse_sps(sps.rawRbsp())` to recover an equivalent
//! Rust SPS context. This matches what tst-py effectively passes (its stored
//! inner SPS, originally produced from the same RBSP bytes).

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jobject};
use tst_core::codec::NalUnit;
use tst_core::codec::h264::{
    EntropyCodingMode, H264ParameterSets, H264Pps, H264SliceHeaderLight, H264SliceType, H264Sps,
    parse_parameter_sets, parse_pps, parse_slice_header_light, parse_sps,
};

use crate::codec::shared::{
    build_color_info, build_rational, builder_set_bool, builder_set_int, builder_set_long,
};
use crate::error::map_codec_parse_error;
use crate::jutil::{read_byte_buffer, wrap_heap_byte_buffer};

/// Map a Rust [`EntropyCodingMode`] to its `org.tstrans.codec.EntropyCodingMode`
/// constant.
fn build_entropy_mode<'local>(
    env: &mut JNIEnv<'local>,
    v: EntropyCodingMode,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        EntropyCodingMode::Cavlc => "CAVLC",
        EntropyCodingMode::Cabac => "CABAC",
    };
    env.get_static_field(
        "org/tstrans/codec/EntropyCodingMode",
        name,
        "Lorg/tstrans/codec/EntropyCodingMode;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Map a Rust [`H264SliceType`] to its `org.tstrans.codec.H264SliceType`
/// constant. The `_ =>` arm maps any future marked-non-exhaustive variant to
/// `UNKNOWN`, mirroring tst-py's `Unknown` catch-all.
fn build_slice_type<'local>(
    env: &mut JNIEnv<'local>,
    v: H264SliceType,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        H264SliceType::P => "P",
        H264SliceType::B => "B",
        H264SliceType::I => "I",
        H264SliceType::Sp => "SP",
        H264SliceType::Si => "SI",
        _ => "UNKNOWN",
    };
    env.get_static_field(
        "org/tstrans/codec/H264SliceType",
        name,
        "Lorg/tstrans/codec/H264SliceType;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a Java `H264Sps` via its `Builder`. Mirrors the KLV Builder-marshalling
/// precedent. Returns `Err(())` if any JNI call fails (a pending Java exception
/// is left in that case).
fn build_sps<'local>(env: &mut JNIEnv<'local>, sps: &H264Sps) -> Result<JObject<'local>, ()> {
    // 20 fields + nested ColorInfo/Rational/enum constants + builder + scratch;
    // 32 slots safely covers the worst case.
    env.ensure_local_capacity(32).map_err(|_| ())?;

    let b = env
        .new_object("org/tstrans/codec/H264Sps$Builder", "()V", &[])
        .map_err(|_| ())?;

    const RET: &str = "Lorg/tstrans/codec/H264Sps$Builder;";

    builder_set_int(
        env,
        &b,
        RET,
        "seqParameterSetId",
        i32::from(sps.seq_parameter_set_id),
    )?;
    builder_set_long(env, &b, RET, "width", i64::from(sps.width))?;
    builder_set_long(env, &b, RET, "height", i64::from(sps.height))?;
    builder_set_int(env, &b, RET, "profileIdc", i32::from(sps.profile_idc))?;
    builder_set_int(env, &b, RET, "levelIdc", i32::from(sps.level_idc))?;
    builder_set_int(
        env,
        &b,
        RET,
        "constraintSetFlags",
        i32::from(sps.constraint_set_flags),
    )?;
    builder_set_int(env, &b, RET, "bitDepthLuma", i32::from(sps.bit_depth_luma))?;
    builder_set_int(
        env,
        &b,
        RET,
        "bitDepthChroma",
        i32::from(sps.bit_depth_chroma),
    )?;

    {
        let cf = crate::codec::shared::build_chroma_format(env, sps.chroma_format)?;
        env.call_method(
            &b,
            "chromaFormat",
            "(Lorg/tstrans/codec/ChromaFormat;)Lorg/tstrans/codec/H264Sps$Builder;",
            &[JValue::Object(&cf)],
        )
        .map_err(|_| ())?;
    }

    builder_set_bool(env, &b, RET, "frameMbsOnly", sps.frame_mbs_only)?;
    builder_set_bool(env, &b, RET, "fixedFrameRate", sps.fixed_frame_rate)?;
    builder_set_bool(env, &b, RET, "hasBFrames", sps.has_b_frames)?;

    if let Some(ref r) = sps.frame_rate {
        let jr = build_rational(env, r)?;
        env.call_method(
            &b,
            "frameRate",
            "(Lorg/tstrans/codec/Rational;)Lorg/tstrans/codec/H264Sps$Builder;",
            &[JValue::Object(&jr)],
        )
        .map_err(|_| ())?;
    }

    if let Some(ref c) = sps.color {
        let jc = build_color_info(env, c)?;
        env.call_method(
            &b,
            "color",
            "(Lorg/tstrans/codec/ColorInfo;)Lorg/tstrans/codec/H264Sps$Builder;",
            &[JValue::Object(&jc)],
        )
        .map_err(|_| ())?;
    }

    builder_set_long(env, &b, RET, "cropLeft", i64::from(sps.crop_left))?;
    builder_set_long(env, &b, RET, "cropRight", i64::from(sps.crop_right))?;
    builder_set_long(env, &b, RET, "cropTop", i64::from(sps.crop_top))?;
    builder_set_long(env, &b, RET, "cropBottom", i64::from(sps.crop_bottom))?;
    builder_set_int(
        env,
        &b,
        RET,
        "log2MaxFrameNumMinus4",
        i32::from(sps.log2_max_frame_num_minus4),
    )?;

    {
        let buf = wrap_heap_byte_buffer(env, &sps.raw_rbsp)?;
        env.call_method(
            &b,
            "rawRbsp",
            "(Ljava/nio/ByteBuffer;)Lorg/tstrans/codec/H264Sps$Builder;",
            &[JValue::Object(&buf)],
        )
        .map_err(|_| ())?;
    }

    env.call_method(&b, "build", "()Lorg/tstrans/codec/H264Sps;", &[])
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a Java `H264Pps` record directly (4 fields, no Builder needed).
fn build_pps<'local>(env: &mut JNIEnv<'local>, pps: &H264Pps) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let mode = build_entropy_mode(env, pps.entropy_coding_mode)?;
    let buf = wrap_heap_byte_buffer(env, &pps.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H264Pps",
        "(IILorg/tstrans/codec/EntropyCodingMode;Ljava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(pps.pic_parameter_set_id)),
            JValue::Int(i32::from(pps.seq_parameter_set_id)),
            JValue::Object(&mode),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

/// Box an `Option<u32>` into a `java.lang.Integer` (or null) for the
/// `frameNum` slot.
fn boxed_frame_num<'local>(
    env: &mut JNIEnv<'local>,
    v: Option<u32>,
) -> Result<JObject<'local>, ()> {
    match v {
        // frame_num is at most 16 bits wide → always fits i32.
        Some(n) => env
            .new_object("java/lang/Integer", "(I)V", &[JValue::Int(n as i32)])
            .map_err(|_| ()),
        None => Ok(JObject::null()),
    }
}

/// Build a Java `H264SliceHeaderLight` record directly.
fn build_slice<'local>(
    env: &mut JNIEnv<'local>,
    h: &H264SliceHeaderLight,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let slice_type = build_slice_type(env, h.slice_type)?;
    let frame_num = boxed_frame_num(env, h.frame_num)?;
    let buf = wrap_heap_byte_buffer(env, &h.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H264SliceHeaderLight",
        "(ZLorg/tstrans/codec/H264SliceType;ILjava/lang/Integer;ZLjava/nio/ByteBuffer;)V",
        &[
            JValue::Bool(u8::from(h.first_in_pic)),
            JValue::Object(&slice_type),
            JValue::Int(i32::from(h.pps_id)),
            JValue::Object(&frame_num),
            JValue::Bool(u8::from(h.idr)),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH264Sps<'local>(
        env,
        _class,
        rbsp: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&rbsp) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        match parse_sps(&bytes) {
            Ok(sps) => match build_sps(env, &sps) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h264");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH264Pps<'local>(
        env,
        _class,
        rbsp: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&rbsp) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        match parse_pps(&bytes) {
            Ok(pps) => match build_pps(env, &pps) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h264");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH264SliceHeaderLight<'local>(
        env,
        _class,
        rbsp: JByteArray<'local>,
        sps: JObject<'local>,
        nal_unit_type: jint,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&rbsp) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };

        // SPS context: re-parse the Java SPS's rawRbsp to recover the equivalent
        // Rust `H264Sps` (only `log2_max_frame_num_minus4` is consulted). A null
        // `sps` means no context → `frame_num` stays None. See the module doc.
        let sps_ctx: Option<H264Sps> = if sps.is_null() {
            None
        } else {
            let buf = match env.call_method(&sps, "rawRbsp", "()Ljava/nio/ByteBuffer;", &[]) {
                Ok(v) => match v.l() {
                    Ok(o) => o,
                    Err(_) => return JObject::null().into_raw(),
                },
                Err(_) => return JObject::null().into_raw(),
            };
            let sps_bytes = match read_byte_buffer(env, &buf) {
                Ok(b) => b,
                Err(_) => return JObject::null().into_raw(),
            };
            match parse_sps(&sps_bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    map_codec_parse_error(env, &e, "h264");
                    return JObject::null().into_raw();
                }
            }
        };

        let nut = (nal_unit_type & 0xff) as u8;
        match parse_slice_header_light(&bytes, sps_ctx.as_ref(), nut) {
            Ok(h) => match build_slice(env, &h) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h264");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH264ParameterSets<'local>(
        env,
        _class,
        nals: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Read each Java NalUnit, filtering to the H264 variant (mirrors tst-py's
        // `parse_h264_parameter_sets_py`: non-H264 / H265 / H266 entries are
        // dropped before handing to the Rust parser).
        let size = match env.call_method(&nals, "size", "()I", &[]) {
            Ok(v) => match v.i() {
                Ok(n) => n,
                Err(_) => return JObject::null().into_raw(),
            },
            Err(_) => return JObject::null().into_raw(),
        };

        let mut rust_nals: Vec<NalUnit> = Vec::new();
        for i in 0..size {
            let parsed = env.with_local_frame(16, |inner| {
                let item = inner
                    .call_method(&nals, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                let kind = inner
                    .call_method(&item, "kind", "()Ljava/lang/String;", &[])?
                    .l()?;
                let kind_jstr = jni::objects::JString::from(kind);
                let kind_str: String = inner.get_string(&kind_jstr)?.into();
                if kind_str != "H264" {
                    return Ok::<Option<NalUnit>, jni::errors::Error>(None);
                }
                let nal_type_raw = inner.call_method(&item, "nalType", "()I", &[])?.i()?;
                let nal_type = crate::jutil::checked_u8(inner, nal_type_raw as i64, "nalType")?;
                // refIdc is a (nullable) Integer; H264 NalUnits always carry it, but
                // default to 3 if absent (mirrors tst-py `n.ref_idc.unwrap_or(3)`).
                let ref_idc_obj = inner
                    .call_method(&item, "refIdc", "()Ljava/lang/Integer;", &[])?
                    .l()?;
                let ref_idc = if ref_idc_obj.is_null() {
                    3u8
                } else {
                    let ref_idc_raw = inner
                        .call_method(&ref_idc_obj, "intValue", "()I", &[])?
                        .i()?;
                    crate::jutil::checked_u8(inner, ref_idc_raw as i64, "refIdc")?
                };
                let payload_buf = inner
                    .call_method(&item, "payload", "()Ljava/nio/ByteBuffer;", &[])?
                    .l()?;
                let payload = read_byte_buffer(inner, &payload_buf)?;
                Ok(Some(NalUnit::H264 {
                    nal_type,
                    ref_idc,
                    payload: payload.into(),
                }))
            });
            match parsed {
                Ok(Some(n)) => rust_nals.push(n),
                Ok(None) => {}
                Err(_) => return JObject::null().into_raw(),
            }
        }

        match parse_parameter_sets(&rust_nals) {
            Ok(ps) => match build_parameter_sets(env, &ps) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h264");
                JObject::null().into_raw()
            }
        }
    }
}

/// Build a Java `H264ParameterSets(Map<Integer,H264Sps>, Map<Integer,H264Pps>)`.
/// Each map is a `java.util.HashMap`; entries are added inside a per-entry local
/// frame so the SPS/PPS object refs are reclaimed each iteration.
fn build_parameter_sets<'local>(
    env: &mut JNIEnv<'local>,
    ps: &H264ParameterSets,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let sps_map = env
        .new_object("java/util/HashMap", "()V", &[])
        .map_err(|_| ())?;
    for (id, sps) in &ps.sps_by_id {
        env.with_local_frame(48, |inner| {
            let key =
                inner.new_object("java/lang/Integer", "(I)V", &[JValue::Int(i32::from(*id))])?;
            let val = build_sps(inner, sps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &sps_map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[JValue::Object(&key), JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    let pps_map = env
        .new_object("java/util/HashMap", "()V", &[])
        .map_err(|_| ())?;
    for (id, pps) in &ps.pps_by_id {
        env.with_local_frame(16, |inner| {
            let key =
                inner.new_object("java/lang/Integer", "(I)V", &[JValue::Int(i32::from(*id))])?;
            let val = build_pps(inner, pps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &pps_map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[JValue::Object(&key), JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    env.new_object(
        "org/tstrans/codec/H264ParameterSets",
        "(Ljava/util/Map;Ljava/util/Map;)V",
        &[JValue::Object(&sps_map), JValue::Object(&pps_map)],
    )
    .map_err(|_| ())
}
