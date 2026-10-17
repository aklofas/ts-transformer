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
    EntropyCodingMode, H264SliceType, H264Sps, parse_parameter_sets, parse_pps,
    parse_slice_header_light, parse_sps,
};

use crate::error::map_codec_parse_error;
use crate::generated::codec::{
    build_h264_parameter_sets, build_h264_pps, build_h264_slice_header_light, build_h264_sps,
};
use crate::jutil::read_byte_buffer;

/// Map a Rust [`EntropyCodingMode`] to its `org.tstrans.codec.EntropyCodingMode`
/// constant.
pub(crate) fn build_entropy_mode<'local>(
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
pub(crate) fn build_slice_type<'local>(
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
            Ok(sps) => match build_h264_sps(env, &sps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            Ok(pps) => match build_h264_pps(env, &pps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            Ok(h) => match build_h264_slice_header_light(env, &h) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            Ok(ps) => match build_h264_parameter_sets(env, &ps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h264");
                JObject::null().into_raw()
            }
        }
    }
}
