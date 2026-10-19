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

use crate::jutil::FindClass;
use crate::panic::jni_native;
use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JObject, JValue};
use jni::strings::JNIString;
use jni::sys::{jint, jobject};
use tst_core::codec::NalUnit;
use tst_core::codec::h266::{
    H266SliceType, H266Sps, parse_parameter_sets, parse_pps, parse_slice_header_light, parse_sps,
    parse_vps,
};

use crate::error::map_codec_parse_error;
use crate::generated::codec::{
    build_h266_parameter_sets, build_h266_pps, build_h266_slice_header_light, build_h266_sps,
    build_h266_vps,
};
use crate::jutil::read_byte_buffer;

/// Map a Rust [`H266SliceType`] to its `org.tstrans.codec.H266SliceType`
/// constant. The `_ =>` arm maps any future marked-non-exhaustive variant to
/// `UNKNOWN`, mirroring tst-py's `Unknown` catch-all.
pub(crate) fn build_slice_type<'local>(
    env: &mut Env<'local>,
    v: H266SliceType,
) -> Result<JObject<'local>, ()> {
    let name = match v {
        H266SliceType::B => "B",
        H266SliceType::P => "P",
        H266SliceType::I => "I",
        _ => "UNKNOWN",
    };
    env.get_static_field(
        FindClass(jni_str!("org/tstrans/codec/H266SliceType")),
        JNIString::from(name),
        jni_sig!("Lorg/tstrans/codec/H266SliceType;"),
    )
    .map_err(|_| ())?
    .l()
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
            Ok(sps) => match build_h266_sps(env, &sps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            Ok(pps) => match build_h266_pps(env, &pps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            Ok(vps) => match build_h266_vps(env, &vps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
            let buf = match env.call_method(&sps, jni_str!("rawRbsp"), jni_sig!("()Ljava/nio/ByteBuffer;"), &[]) {
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
            Ok(h) => match build_h266_slice_header_light(env, &h) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
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
        let size = match env.call_method(&nals, jni_str!("size"), jni_sig!("()I"), &[]) {
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
                    .call_method(&nals, jni_str!("get"), jni_sig!("(I)Ljava/lang/Object;"), &[JValue::Int(i)])?
                    .l()?;
                let kind = inner
                    .call_method(&item, jni_str!("kind"), jni_sig!("()Ljava/lang/String;"), &[])?
                    .l()?;
                let kind_jstr = inner.cast_local::<jni::objects::JString>(kind)?;
                let kind_str: String = kind_jstr.try_to_string(inner)?;
                if kind_str != "H266" {
                    return Ok::<Option<NalUnit>, jni::errors::Error>(None);
                }
                let nal_type_raw = inner.call_method(&item, jni_str!("nalType"), jni_sig!("()I"), &[])?.i()?;
                let nal_type = crate::jutil::checked_u8(inner, nal_type_raw as i64, "nalType")?;
                // layerId / temporalIdPlus1 are (nullable) Integer; H266 NalUnits
                // always carry them, but default to 0 / 1 if absent (mirrors tst-py
                // `layer_id.unwrap_or(0)` / `temporal_id_plus1.unwrap_or(1)`).
                let layer_id_obj = inner
                    .call_method(&item, jni_str!("layerId"), jni_sig!("()Ljava/lang/Integer;"), &[])?
                    .l()?;
                let layer_id = if layer_id_obj.is_null() {
                    0u8
                } else {
                    let layer_id_raw = inner
                        .call_method(&layer_id_obj, jni_str!("intValue"), jni_sig!("()I"), &[])?
                        .i()?;
                    crate::jutil::checked_u8(inner, layer_id_raw as i64, "layerId")?
                };
                let tid_obj = inner
                    .call_method(&item, jni_str!("temporalIdPlus1"), jni_sig!("()Ljava/lang/Integer;"), &[])?
                    .l()?;
                let temporal_id_plus1 = if tid_obj.is_null() {
                    1u8
                } else {
                    let tid_raw = inner.call_method(&tid_obj, jni_str!("intValue"), jni_sig!("()I"), &[])?.i()?;
                    crate::jutil::checked_u8(inner, tid_raw as i64, "temporalIdPlus1")?
                };
                let payload_buf = inner
                    .call_method(&item, jni_str!("payload"), jni_sig!("()Ljava/nio/ByteBuffer;"), &[])?
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
            Ok(ps) => match build_h266_parameter_sets(env, &ps) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_codec_parse_error(env, &e, "h266");
                JObject::null().into_raw()
            }
        }
    }
}
