//! JNI surface for `org.tstrans.codec.Codec`'s H.266 / VVC parser entry points.
//!
//! Five natives, each mirroring a tst-py `parse_h266_*` free function:
//!
//! - `nParseH266Sps(byte[]) -> H266Sps` → `tst_core::codec::h266::parse_sps`
//! - `nParseH266Pps(byte[]) -> H266Pps` → `parse_pps`
//! - `nParseH266Vps(byte[]) -> H266Vps` → `parse_vps`
//! - `nParseH266SliceHeaderLight(byte[], H266Sps, int) -> H266SliceHeaderLight`
//!   → `parse_slice_header_light`
//! - `nParseH266ParameterSets(List<NalUnit>) -> H266ParameterSets`
//!   → `parse_parameter_sets`
//!
//! On a parse error each native calls [`crate::error::map_codec_parse_error`]
//! (which throws `CodecParseException`) and returns a null object — the Java
//! wrapper on `Codec` is declared `throws CodecParseException` so the pending
//! exception propagates. Same shape as the H.264 / H.265 modules.
//!
//! ### Differences from the H.265 module
//!
//! - **PTL is a real nested sub-record.** `H266Sps` carries an
//!   `H266ProfileTierLevel profileTierLevel` field built directly from the
//!   nested Rust `H266ProfileTierLevel` — not a reconstruction method.
//! - **`H266ParameterSets` is List-backed** (`Vec`, not `BTreeMap`): the
//!   builder produces three `java.util.ArrayList`s, not `HashMap`s.
//!
//! ### Slice-header SPS context
//!
//! `parse_slice_header_light` accepts an optional `&tst_core H266Sps`. The Java
//! `H266Sps` record cannot hold the Rust struct, so when the caller passes a
//! non-null `sps` we re-`parse_sps(sps.rawRbsp())` to recover an equivalent Rust
//! SPS context — same pattern as the H.265 module. The H.266 light parser only
//! consults the SPS for the POC bit width on the (deferred) non-IDR path; IDR
//! POC stays implicit 0.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jobject};
use tst_core::codec::NalUnit;
use tst_core::codec::h266::{
    H266ParameterSets, H266Pps, H266ProfileTierLevel, H266SliceHeaderLight, H266SliceType, H266Sps,
    H266Vps, parse_parameter_sets, parse_pps, parse_slice_header_light, parse_sps, parse_vps,
};

use crate::codec::shared::{build_color_info, build_rational, builder_set_int, builder_set_long};
use crate::error::map_codec_parse_error;
use crate::jutil::{read_byte_buffer, wrap_heap_byte_buffer};

/// Map a Rust [`H266SliceType`] to its `org.tstrans.codec.H266SliceType`
/// constant. The `_ =>` arm maps any future marked-non-exhaustive variant to
/// `UNKNOWN`, mirroring tst-py's `Unknown` catch-all.
fn build_slice_type<'local>(
    env: &mut JNIEnv<'local>,
    v: H266SliceType,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        H266SliceType::B => "B",
        H266SliceType::P => "P",
        H266SliceType::I => "I",
        _ => "UNKNOWN",
    };
    env.get_static_field(
        "org/tstrans/codec/H266SliceType",
        name,
        "Lorg/tstrans/codec/H266SliceType;",
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Build a Java `H266ProfileTierLevel(int, boolean, int)` from the nested Rust
/// PTL sub-record.
fn build_ptl<'local>(
    env: &mut JNIEnv<'local>,
    ptl: &H266ProfileTierLevel,
) -> Result<JObject<'local>, ()> {
    env.new_object(
        "org/tstrans/codec/H266ProfileTierLevel",
        "(IZI)V",
        &[
            JValue::Int(i32::from(ptl.general_profile_idc)),
            JValue::Bool(u8::from(ptl.general_tier_flag)),
            JValue::Int(i32::from(ptl.general_level_idc)),
        ],
    )
    .map_err(|_| ())
}

/// Build a Java `H266Sps` via its `Builder`. The nested PTL is built as a real
/// sub-record and set as a field (not reconstructed from flattened fields).
/// Returns `Err(())` if any JNI call fails (a pending Java exception is left).
fn build_sps<'local>(env: &mut JNIEnv<'local>, sps: &H266Sps) -> Result<JObject<'local>, ()> {
    // 15 fields + nested PTL/ColorInfo/Rational/enum constants + builder +
    // scratch; 32 slots safely covers the worst case.
    env.ensure_local_capacity(32).map_err(|_| ())?;

    let b = env
        .new_object("org/tstrans/codec/H266Sps$Builder", "()V", &[])
        .map_err(|_| ())?;

    const RET: &str = "Lorg/tstrans/codec/H266Sps$Builder;";

    builder_set_int(env, &b, RET, "spsId", i32::from(sps.sps_id))?;
    builder_set_int(env, &b, RET, "vpsId", i32::from(sps.vps_id))?;

    {
        let ptl = build_ptl(env, &sps.profile_tier_level)?;
        env.call_method(
            &b,
            "profileTierLevel",
            "(Lorg/tstrans/codec/H266ProfileTierLevel;)Lorg/tstrans/codec/H266Sps$Builder;",
            &[JValue::Object(&ptl)],
        )
        .map_err(|_| ())?;
    }

    builder_set_long(env, &b, RET, "width", i64::from(sps.width))?;
    builder_set_long(env, &b, RET, "height", i64::from(sps.height))?;

    {
        let cf = crate::codec::shared::build_chroma_format(env, sps.chroma_format)?;
        env.call_method(
            &b,
            "chromaFormat",
            "(Lorg/tstrans/codec/ChromaFormat;)Lorg/tstrans/codec/H266Sps$Builder;",
            &[JValue::Object(&cf)],
        )
        .map_err(|_| ())?;
    }

    builder_set_int(env, &b, RET, "bitDepthLuma", i32::from(sps.bit_depth_luma))?;
    builder_set_int(
        env,
        &b,
        RET,
        "bitDepthChroma",
        i32::from(sps.bit_depth_chroma),
    )?;

    if let Some(ref c) = sps.color_info {
        let jc = build_color_info(env, c)?;
        env.call_method(
            &b,
            "color",
            "(Lorg/tstrans/codec/ColorInfo;)Lorg/tstrans/codec/H266Sps$Builder;",
            &[JValue::Object(&jc)],
        )
        .map_err(|_| ())?;
    }

    if let Some(ref r) = sps.frame_rate {
        let jr = build_rational(env, r)?;
        env.call_method(
            &b,
            "frameRate",
            "(Lorg/tstrans/codec/Rational;)Lorg/tstrans/codec/H266Sps$Builder;",
            &[JValue::Object(&jr)],
        )
        .map_err(|_| ())?;
    }

    builder_set_long(env, &b, RET, "cropLeft", i64::from(sps.crop_left))?;
    builder_set_long(env, &b, RET, "cropRight", i64::from(sps.crop_right))?;
    builder_set_long(env, &b, RET, "cropTop", i64::from(sps.crop_top))?;
    builder_set_long(env, &b, RET, "cropBottom", i64::from(sps.crop_bottom))?;

    {
        let buf = wrap_heap_byte_buffer(env, &sps.raw_rbsp)?;
        env.call_method(
            &b,
            "rawRbsp",
            "(Ljava/nio/ByteBuffer;)Lorg/tstrans/codec/H266Sps$Builder;",
            &[JValue::Object(&buf)],
        )
        .map_err(|_| ())?;
    }

    env.call_method(&b, "build", "()Lorg/tstrans/codec/H266Sps;", &[])
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a Java `H266Vps(int, int, int, ByteBuffer)` record directly.
fn build_vps<'local>(env: &mut JNIEnv<'local>, vps: &H266Vps) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let buf = wrap_heap_byte_buffer(env, &vps.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H266Vps",
        "(IIILjava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(vps.vps_id)),
            JValue::Int(i32::from(vps.max_layers)),
            JValue::Int(i32::from(vps.max_sub_layers)),
            JValue::Object(&buf),
        ],
    )
    .map_err(|_| ())
}

/// Build a Java `H266Pps(int, int, ByteBuffer)` record directly.
fn build_pps<'local>(env: &mut JNIEnv<'local>, pps: &H266Pps) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let buf = wrap_heap_byte_buffer(env, &pps.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H266Pps",
        "(IILjava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(pps.pps_id)),
            JValue::Int(i32::from(pps.sps_id)),
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

/// Build a Java `H266SliceHeaderLight` record directly.
fn build_slice<'local>(
    env: &mut JNIEnv<'local>,
    h: &H266SliceHeaderLight,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(8).map_err(|_| ())?;
    let slice_type = build_slice_type(env, h.slice_type)?;
    let poc = boxed_poc(env, h.pic_order_cnt_lsb)?;
    let buf = wrap_heap_byte_buffer(env, &h.raw_rbsp)?;
    env.new_object(
        "org/tstrans/codec/H266SliceHeaderLight",
        "(ZLorg/tstrans/codec/H266SliceType;ILjava/lang/Integer;ZLjava/nio/ByteBuffer;)V",
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
    fn Java_org_tstrans_codec_Codec_nParseH266Sps<'local>(
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
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH266Pps<'local>(
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
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH266Vps<'local>(
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
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH266SliceHeaderLight<'local>(
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
        // Rust `H266Sps`. A null `sps` means no context. See the module doc.
        let sps_ctx: Option<H266Sps> = if sps.is_null() {
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
                    map_codec_parse_error(env, &e, "h266");
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
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_codec_Codec_nParseH266ParameterSets<'local>(
        env,
        _class,
        nals: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Read each Java NalUnit, filtering to the H266 variant (mirrors tst-py's
        // `parse_h266_parameter_sets_py`: non-H266 entries are dropped before
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
                if kind_str != "H266" {
                    return Ok::<Option<NalUnit>, jni::errors::Error>(None);
                }
                let nal_type_raw = inner.call_method(&item, "nalType", "()I", &[])?.i()?;
                let nal_type = crate::jutil::checked_u8(inner, nal_type_raw as i64, "nalType")?;
                // layerId / temporalIdPlus1 are (nullable) Integer; H266 NalUnits
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
                Ok(Some(NalUnit::H266 {
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
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}

/// Build a Java
/// `H266ParameterSets(List<H266Vps>, List<H266Sps>, List<H266Pps>)`.
/// Each list is a `java.util.ArrayList`; entries are added inside a per-entry
/// local frame so the VPS/SPS/PPS object refs are reclaimed each iteration.
/// Mirrors tst-py's list-backed `H266ParameterSets` (the Rust collections are
/// `Vec`, not `BTreeMap`).
fn build_parameter_sets<'local>(
    env: &mut JNIEnv<'local>,
    ps: &H266ParameterSets,
) -> Result<JObject<'local>, ()> {
    env.ensure_local_capacity(16).map_err(|_| ())?;

    let vps_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for vps in &ps.vpses {
        env.with_local_frame(16, |inner| {
            let val = build_vps(inner, vps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &vps_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    let sps_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for sps in &ps.spses {
        env.with_local_frame(48, |inner| {
            let val = build_sps(inner, sps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &sps_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    let pps_list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(|_| ())?;
    for pps in &ps.ppses {
        env.with_local_frame(16, |inner| {
            let val = build_pps(inner, pps).map_err(|()| jni::errors::Error::JavaException)?;
            inner.call_method(
                &pps_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&val)],
            )?;
            Ok::<(), jni::errors::Error>(())
        })
        .map_err(|_| ())?;
    }

    env.new_object(
        "org/tstrans/codec/H266ParameterSets",
        "(Ljava/util/List;Ljava/util/List;Ljava/util/List;)V",
        &[
            JValue::Object(&vps_list),
            JValue::Object(&sps_list),
            JValue::Object(&pps_list),
        ],
    )
    .map_err(|_| ())
}
