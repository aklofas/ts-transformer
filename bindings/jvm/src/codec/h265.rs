//! JNI surface for `org.tstrans.codec.Codec`'s H.265 parser entry points.
//!
//! Five natives, each mirroring a tst-py `parse_h265_*` free function:
//!
//! - `nParseH265Sps(byte[]) -> H265Sps` → `tst_core::codec::h265::parse_sps`
//! - `nParseH265Pps(byte[]) -> H265Pps` → `parse_pps`
//! - `nParseH265Vps(byte[]) -> H265Vps` → `parse_vps`
//! - `nParseH265SliceHeaderLight(byte[], H265Sps, int) -> H265SliceHeaderLight`
//!   → `parse_slice_header_light`
//! - `nParseH265ParameterSets(List<NalUnit>) -> H265ParameterSets`
//!   → `parse_parameter_sets`
//!
//! On a parse error each native calls [`crate::error::map_codec_parse_error`]
//! (which throws `CodecParseException`) and returns a null object — the Java
//! wrapper on `Codec` is declared `throws CodecParseException` so the pending
//! exception propagates. Same shape as the H.264 module.
//!
//! ### Slice-header SPS context
//!
//! `parse_slice_header_light` needs a `&tst_core H265Sps` only to read
//! `pic_order_cnt_lsb`'s bit width (`log2_max_pic_order_cnt_lsb_minus4 + 4`).
//! The Java `H265Sps` record cannot hold the Rust struct, so when the caller
//! passes a non-null `sps` we re-`parse_sps(sps.rawRbsp())` to recover an
//! equivalent Rust SPS context. This matches what tst-py passes (its stored
//! inner SPS, originally produced from the same RBSP bytes).

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jobject};
use tst_core::codec::NalUnit;
use tst_core::codec::h265::{
    H265ParameterSets, H265Pps, H265SliceHeaderLight, H265SliceType, H265Sps, H265Vps,
    parse_parameter_sets, parse_pps, parse_slice_header_light, parse_sps, parse_vps,
};

use crate::codec::shared::{
    build_color_info, build_rational, builder_set_bool, builder_set_int, builder_set_long,
};
use crate::error::map_codec_parse_error;
use crate::jutil::{read_byte_buffer, wrap_heap_byte_buffer};

/// Map a Rust [`H265SliceType`] to its `org.tstrans.codec.H265SliceType`
/// constant. The `_ =>` arm maps any future marked-non-exhaustive variant to
/// `UNKNOWN`, mirroring tst-py's `Unknown` catch-all.
fn build_slice_type<'local>(
    env: &mut JNIEnv<'local>,
    v: H265SliceType,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        H265SliceType::B => "B",
        H265SliceType::P => "P",
        H265SliceType::I => "I",
        _ => "UNKNOWN",
    };
    env.get_static_field(
        "org/tstrans/codec/H265SliceType",
        name,
        "Lorg/tstrans/codec/H265SliceType;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a Java `H265Sps` via its `Builder`. Mirrors the H.264 Builder
/// marshalling. Returns `Err(())` if any JNI call fails (a pending Java
/// exception is left in that case).
fn build_sps<'local>(env: &mut JNIEnv<'local>, sps: &H265Sps) -> Result<JObject<'local>, ()> {
    // 24 fields + nested ColorInfo/Rational/enum constants + builder + scratch;
    // 40 slots safely covers the worst case.
    env.ensure_local_capacity(40).map_err(|_| ())?;

    let b = env
        .new_object("org/tstrans/codec/H265Sps$Builder", "()V", &[])
        .map_err(|_| ())?;

    const RET_SPS: &str = "Lorg/tstrans/codec/H265Sps$Builder;";

    builder_set_int(
        env,
        &b,
        RET_SPS,
        "spsSeqParameterSetId",
        i32::from(sps.sps_seq_parameter_set_id),
    )?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "spsVideoParameterSetId",
        i32::from(sps.sps_video_parameter_set_id),
    )?;
    builder_set_long(env, &b, RET_SPS, "width", i64::from(sps.width))?;
    builder_set_long(env, &b, RET_SPS, "height", i64::from(sps.height))?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "generalProfileIdc",
        i32::from(sps.general_profile_idc),
    )?;
    builder_set_bool(env, &b, RET_SPS, "generalTierFlag", sps.general_tier_flag)?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "generalLevelIdc",
        i32::from(sps.general_level_idc),
    )?;
    // general_profile_compatibility_flags is u32 → widen to i64 unsigned.
    builder_set_long(
        env,
        &b,
        RET_SPS,
        "generalProfileCompatibilityFlags",
        i64::from(sps.general_profile_compatibility_flags),
    )?;
    builder_set_bool(
        env,
        &b,
        RET_SPS,
        "generalProgressiveSourceFlag",
        sps.general_progressive_source_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_SPS,
        "generalInterlacedSourceFlag",
        sps.general_interlaced_source_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_SPS,
        "generalNonPackedConstraintFlag",
        sps.general_non_packed_constraint_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_SPS,
        "generalFrameOnlyConstraintFlag",
        sps.general_frame_only_constraint_flag,
    )?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "bitDepthLuma",
        i32::from(sps.bit_depth_luma),
    )?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "bitDepthChroma",
        i32::from(sps.bit_depth_chroma),
    )?;

    {
        let cf = crate::codec::shared::build_chroma_format(env, sps.chroma_format)?;
        env.call_method(
            &b,
            "chromaFormat",
            "(Lorg/tstrans/codec/ChromaFormat;)Lorg/tstrans/codec/H265Sps$Builder;",
            &[JValue::Object(&cf)],
        )
        .map_err(|_| ())?;
    }

    builder_set_int(
        env,
        &b,
        RET_SPS,
        "maxSubLayersMinus1",
        i32::from(sps.max_sub_layers_minus1),
    )?;

    if let Some(ref r) = sps.frame_rate {
        let jr = build_rational(env, r)?;
        env.call_method(
            &b,
            "frameRate",
            "(Lorg/tstrans/codec/Rational;)Lorg/tstrans/codec/H265Sps$Builder;",
            &[JValue::Object(&jr)],
        )
        .map_err(|_| ())?;
    }

    if let Some(ref c) = sps.color {
        let jc = build_color_info(env, c)?;
        env.call_method(
            &b,
            "color",
            "(Lorg/tstrans/codec/ColorInfo;)Lorg/tstrans/codec/H265Sps$Builder;",
            &[JValue::Object(&jc)],
        )
        .map_err(|_| ())?;
    }

    builder_set_long(env, &b, RET_SPS, "cropLeft", i64::from(sps.crop_left))?;
    builder_set_long(env, &b, RET_SPS, "cropRight", i64::from(sps.crop_right))?;
    builder_set_long(env, &b, RET_SPS, "cropTop", i64::from(sps.crop_top))?;
    builder_set_long(env, &b, RET_SPS, "cropBottom", i64::from(sps.crop_bottom))?;
    builder_set_int(
        env,
        &b,
        RET_SPS,
        "log2MaxPicOrderCntLsbMinus4",
        i32::from(sps.log2_max_pic_order_cnt_lsb_minus4),
    )?;

    {
        let buf = wrap_heap_byte_buffer(env, &sps.raw_rbsp)?;
        env.call_method(
            &b,
            "rawRbsp",
            "(Ljava/nio/ByteBuffer;)Lorg/tstrans/codec/H265Sps$Builder;",
            &[JValue::Object(&buf)],
        )
        .map_err(|_| ())?;
    }

    env.call_method(&b, "build", "()Lorg/tstrans/codec/H265Sps;", &[])
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a Java `H265Vps` via its `Builder` (13 fields).
fn build_vps<'local>(env: &mut JNIEnv<'local>, vps: &H265Vps) -> Result<JObject<'local>, ()> {
    // 13 fields + builder + ByteBuffer scratch — 24 slots is ample.
    env.ensure_local_capacity(24).map_err(|_| ())?;

    let b = env
        .new_object("org/tstrans/codec/H265Vps$Builder", "()V", &[])
        .map_err(|_| ())?;

    const RET_VPS: &str = "Lorg/tstrans/codec/H265Vps$Builder;";

    builder_set_int(
        env,
        &b,
        RET_VPS,
        "vpsVideoParameterSetId",
        i32::from(vps.vps_video_parameter_set_id),
    )?;
    builder_set_int(
        env,
        &b,
        RET_VPS,
        "maxLayersMinus1",
        i32::from(vps.max_layers_minus1),
    )?;
    builder_set_int(
        env,
        &b,
        RET_VPS,
        "maxSubLayersMinus1",
        i32::from(vps.max_sub_layers_minus1),
    )?;
    builder_set_bool(
        env,
        &b,
        RET_VPS,
        "temporalIdNestingFlag",
        vps.temporal_id_nesting_flag,
    )?;
    builder_set_int(
        env,
        &b,
        RET_VPS,
        "generalProfileIdc",
        i32::from(vps.general_profile_idc),
    )?;
    builder_set_bool(env, &b, RET_VPS, "generalTierFlag", vps.general_tier_flag)?;
    builder_set_int(
        env,
        &b,
        RET_VPS,
        "generalLevelIdc",
        i32::from(vps.general_level_idc),
    )?;
    builder_set_long(
        env,
        &b,
        RET_VPS,
        "generalProfileCompatibilityFlags",
        i64::from(vps.general_profile_compatibility_flags),
    )?;
    builder_set_bool(
        env,
        &b,
        RET_VPS,
        "generalProgressiveSourceFlag",
        vps.general_progressive_source_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_VPS,
        "generalInterlacedSourceFlag",
        vps.general_interlaced_source_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_VPS,
        "generalNonPackedConstraintFlag",
        vps.general_non_packed_constraint_flag,
    )?;
    builder_set_bool(
        env,
        &b,
        RET_VPS,
        "generalFrameOnlyConstraintFlag",
        vps.general_frame_only_constraint_flag,
    )?;

    {
        let buf = wrap_heap_byte_buffer(env, &vps.raw_rbsp)?;
        env.call_method(
            &b,
            "rawRbsp",
            "(Ljava/nio/ByteBuffer;)Lorg/tstrans/codec/H265Vps$Builder;",
            &[JValue::Object(&buf)],
        )
        .map_err(|_| ())?;
    }

    env.call_method(&b, "build", "()Lorg/tstrans/codec/H265Vps;", &[])
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a Java `H265Pps` record directly (3 fields, no Builder needed).
fn build_pps<'local>(env: &mut JNIEnv<'local>, pps: &H265Pps) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let buf = wrap_heap_byte_buffer(env, &pps.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H265Pps",
        "(IILjava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(pps.pps_pic_parameter_set_id)),
            JValue::Int(i32::from(pps.pps_seq_parameter_set_id)),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

/// Box an `Option<u16>` into a `java.lang.Integer` (or null) for the
/// `picOrderCntLsb` slot.
fn boxed_poc<'local>(env: &mut JNIEnv<'local>, v: Option<u16>) -> Result<JObject<'local>, ()> {
    match v {
        Some(n) => env
            .new_object("java/lang/Integer", "(I)V", &[JValue::Int(i32::from(n))])
            .map_err(|_| ()),
        None => Ok(JObject::null()),
    }
}

/// Build a Java `H265SliceHeaderLight` record directly.
fn build_slice<'local>(
    env: &mut JNIEnv<'local>,
    h: &H265SliceHeaderLight,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let slice_type = build_slice_type(env, h.slice_type)?;
    let poc = boxed_poc(env, h.pic_order_cnt_lsb)?;
    let buf = wrap_heap_byte_buffer(env, &h.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H265SliceHeaderLight",
        "(ZLorg/tstrans/codec/H265SliceType;ILjava/lang/Integer;ZLjava/nio/ByteBuffer;)V",
        &[
            JValue::Bool(u8::from(h.first_in_pic)),
            JValue::Object(&slice_type),
            JValue::Int(i32::from(h.pps_id)),
            JValue::Object(&poc),
            JValue::Bool(u8::from(h.idr)),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH265Sps<'local>(
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
                map_codec_parse_error(env, &e, "h265");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH265Pps<'local>(
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
                map_codec_parse_error(env, &e, "h265");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH265Vps<'local>(
        env,
        _class,
        rbsp: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&rbsp) {
            Ok(b) => b,
            Err(_) => return JObject::null().into_raw(),
        };
        match parse_vps(&bytes) {
            Ok(vps) => match build_vps(env, &vps) {
                Ok(obj) => obj.into_raw(),
                Err(()) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h265");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH265SliceHeaderLight<'local>(
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
        // Rust `H265Sps` (only `log2_max_pic_order_cnt_lsb_minus4` is consulted). A
        // null `sps` means no context → `pic_order_cnt_lsb` stays None. See the
        // module doc.
        let sps_ctx: Option<H265Sps> = if sps.is_null() {
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
                    map_codec_parse_error(env, &e, "h265");
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
                map_codec_parse_error(env, &e, "h265");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH265ParameterSets<'local>(
        env,
        _class,
        nals: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Read each Java NalUnit, filtering to the H265 variant (mirrors tst-py's
        // `parse_h265_parameter_sets_py`: non-H265 entries are dropped before
        // handing to the Rust parser).
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
                if kind_str != "H265" {
                    return Ok::<Option<NalUnit>, jni::errors::Error>(None);
                }
                let nal_type_raw = inner.call_method(&item, "nalType", "()I", &[])?.i()?;
                let nal_type = crate::jutil::checked_u8(inner, nal_type_raw as i64, "nalType")?;
                // layerId / temporalIdPlus1 are (nullable) Integer; H265 NalUnits
                // always carry them, but default to 0 / 1 if absent (mirrors tst-py
                // `layer_id.unwrap_or(0)` / `temporal_id_plus1.unwrap_or(1)`).
                let layer_id_obj = inner
                    .call_method(&item, "layerId", "()Ljava/lang/Integer;", &[])?
                    .l()?;
                let layer_id = if layer_id_obj.is_null() {
                    0u8
                } else {
                    let layer_id_raw = inner
                        .call_method(&layer_id_obj, "intValue", "()I", &[])?
                        .i()?;
                    crate::jutil::checked_u8(inner, layer_id_raw as i64, "layerId")?
                };
                let tid_obj = inner
                    .call_method(&item, "temporalIdPlus1", "()Ljava/lang/Integer;", &[])?
                    .l()?;
                let temporal_id_plus1 = if tid_obj.is_null() {
                    1u8
                } else {
                    let tid_raw = inner.call_method(&tid_obj, "intValue", "()I", &[])?.i()?;
                    crate::jutil::checked_u8(inner, tid_raw as i64, "temporalIdPlus1")?
                };
                let payload_buf = inner
                    .call_method(&item, "payload", "()Ljava/nio/ByteBuffer;", &[])?
                    .l()?;
                let payload = read_byte_buffer(inner, &payload_buf)?;
                Ok(Some(NalUnit::H265 {
                    nal_type,
                    layer_id,
                    temporal_id_plus1,
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
                map_codec_parse_error(env, &e, "h265");
                JObject::null().into_raw()
            }
        }
    }
}

/// Build a Java
/// `H265ParameterSets(Map<Integer,H265Vps>, Map<Integer,H265Sps>, Map<Integer,H265Pps>)`.
/// Each map is a `java.util.HashMap`; entries are added inside a per-entry local
/// frame so the VPS/SPS/PPS object refs are reclaimed each iteration.
fn build_parameter_sets<'local>(
    env: &mut JNIEnv<'local>,
    ps: &H265ParameterSets,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let vps_map = env
        .new_object("java/util/HashMap", "()V", &[])
        .map_err(|_| ())?;
    for (id, vps) in &ps.vps_by_id {
        env.with_local_frame(32, |inner| {
            let key =
                inner.new_object("java/lang/Integer", "(I)V", &[JValue::Int(i32::from(*id))])?;
            let val = build_vps(inner, vps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &vps_map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[JValue::Object(&key), JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

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
        "org/tstrans/codec/H265ParameterSets",
        "(Ljava/util/Map;Ljava/util/Map;Ljava/util/Map;)V",
        &[
            JValue::Object(&vps_map),
            JValue::Object(&sps_map),
            JValue::Object(&pps_map),
        ],
    )
    .map_err(|_| ())
}
