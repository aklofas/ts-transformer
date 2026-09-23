//! JNI surface for ST 0806.4 RVT (Remote Video Terminal) Local Set
//! decode/encode, plus its two repeatable nested sets (`RvtPoi`/`RvtAoi`)
//! and the User Defined LS (`RvtUserData`).
//!
//! `nDecodeRvt(byte[]) -> RvtLs` — calls `tst_core::klv::st0806::decode`
//! (body form: no UL, no outer BER length wrapper) and builds the Java
//! `RvtLs` via its public mutable `Builder`. Each `RvtPoi`/`RvtAoi` is
//! built inside its own `with_local_frame` so per-item refs are reclaimed
//! before the next item — same idiom as `VTargetPack` in `st0903.rs`.
//!
//! `nDecodeRvtStandalone(byte[]) -> RvtLs` — calls `decode_standalone`
//! (own UL + BER length + CRC-32/MPEG-2 verification).
//!
//! `nEncodeRvt(RvtLs) -> byte[]` / `nEncodeRvtStandalone(RvtLs) -> byte[]` —
//! read all fields via accessor `call_method`s, build a Rust `RvtLs`, call
//! `encode_to_vec` / `encode_to_vec_standalone`. Mirrors tst-py's
//! `py_to_rvt_ls` / `encode_rvt` / `encode_rvt_standalone`.
//!
//! ### Enum crossing (RvtPoiType / RvtAoiType)
//!
//! Both carry a spec codepoint AND a wire-unknown escape (`Other(u8)`), so
//! they cross as a raw `Integer` codepoint (`RvtPoi.poiTypeCode` /
//! `RvtAoi.aoiTypeCode`) — NEVER as a Java enum ordinal. The typed
//! `poiType()`/`aoiType()` accessors are pure-Java (`RvtPoiType.fromCode`/
//! `RvtAoiType.fromCode`), returning `null` for a wire-unknown codepoint —
//! mirrors the established `IcingDetected`/`icingDetectedCode` pattern in
//! `st0601.rs`. `RvtUserDataType` never crosses the JNI boundary at all —
//! `RvtUserData.dataType()`/`numericId()` are pure-Java computed accessors
//! over the single wire byte `numericIdRaw`, matching tst-py's
//! `@property`-on-`numeric_id_raw` design.
//!
//! ### JNI local-ref capacity
//!
//! `build_rvt_ls`/`read_rvt_ls` both call `env.ensure_local_capacity(128)`.
//! Honest per-call tally of the REAL object/array refs minted in the OUTER
//! frame (excludes anything reclaimed inside a per-item `with_local_frame`,
//! per the WP-C lesson that a hand-wavy estimate can leave zero margin):
//!
//! Build direction: 23 discarded `Builder` fluent-setter "this" refs
//! (crc32/timestampUs/platformTrueAirspeed/platformIndicatedAirspeed/
//! telemetryAccuracyIndicator/fragCircleRadiusM/frameCode/rvtLsVersion/
//! videoDataRate/digitalVideoFileFormat/userDefined/pointsOfInterest/
//! areasOfInterest/aircraftMgrsZone/aircraftMgrsBandGrid/
//! aircraftMgrsEastingM/aircraftMgrsNorthingM/frameCenterMgrsZone/
//! frameCenterMgrsBandGrid/frameCenterMgrsEastingM/
//! frameCenterMgrsNorthingM/unknown/fieldErrors = 23 calls, each pinning
//! one local ref even though the Rust caller discards the returned
//! `Builder` reference) + 1 builder object + 1 built `RvtLs` object + 3
//! `String` objects (`digitalVideoFileFormat`/`aircraftMgrsBandGrid`/
//! `frameCenterMgrsBandGrid`) + 5 `ArrayList` objects (`userDefined`/
//! `pointsOfInterest`/`areasOfInterest`/`unknown`/`fieldErrors`) = 33
//! bare-minimum refs. Read direction: 18 `read_nullable_*` calls (each
//! minting one boxed-object local ref for the returned `Integer`/`Long`/
//! `String`) + 3 list-getter refs (`userDefined`/`pointsOfInterest`/
//! `areasOfInterest`) + 1 `unknown`-list-getter ref = 22 bare-minimum refs
//! (`read_unknown_list`'s own internal per-entry refs are pre-existing
//! shared-helper behavior, not counted here). 128 leaves ~4-6x headroom on
//! both directions — "real headroom, not a tight fit" per the WP-C lesson
//! (`st0601.rs`'s 320, bumped from a self-contradictory 256).
//!
//! `RvtPoi`/`RvtAoi` (10 fields each) run inside their caller's 64-slot
//! `with_local_frame`, matching `VTargetPack`'s precedent — no separate
//! `ensure_local_capacity` needed. `RvtUserData` (2 fields) runs inside a
//! 16-slot per-item frame.

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::jobject;
use tst_core::klv::st0806::{
    RvtAoi as RustRvtAoi, RvtAoiType as RustRvtAoiType, RvtLs as RustRvtLs, RvtPoi as RustRvtPoi,
    RvtPoiType as RustRvtPoiType, RvtUserData as RustRvtUserData, decode as decode_rvt,
    decode_standalone as decode_rvt_standalone, encode_to_vec as encode_rvt,
    encode_to_vec_standalone as encode_rvt_standalone,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};
use crate::jutil::{
    build_field_errors, build_long_list, build_unknown_list, checked_u8, checked_u16, checked_u32,
    read_byte_buffer, read_long_list, read_nullable_double, read_nullable_int, read_nullable_long,
    read_nullable_string, read_unknown_list, wrap_heap_byte_buffer,
};

// -----------------------------------------------------------------------
// Class / method-descriptor constants
// -----------------------------------------------------------------------

const RVT_BUILDER_CLASS: &str = "org/tstrans/klv/RvtLs$Builder";
const RVT_BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/RvtLs$Builder;";
const RVT_BUILDER_SIG_LONG: &str = "(J)Lorg/tstrans/klv/RvtLs$Builder;";
const RVT_BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/RvtLs$Builder;";
const RVT_BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/RvtLs$Builder;";

const POI_BUILDER_CLASS: &str = "org/tstrans/klv/RvtPoi$Builder";
const POI_BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/RvtPoi$Builder;";
const POI_BUILDER_SIG_DBL: &str = "(D)Lorg/tstrans/klv/RvtPoi$Builder;";
const POI_BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/RvtPoi$Builder;";
const POI_BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/RvtPoi$Builder;";

const AOI_BUILDER_CLASS: &str = "org/tstrans/klv/RvtAoi$Builder";
const AOI_BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/RvtAoi$Builder;";
const AOI_BUILDER_SIG_DBL: &str = "(D)Lorg/tstrans/klv/RvtAoi$Builder;";
const AOI_BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/RvtAoi$Builder;";
const AOI_BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/RvtAoi$Builder;";

/// ST 0806.4 RVT LS top-level typed tags: 1..=21 (Table 8-1 — no gaps).
/// Mirrors tst-py's `is_st0806_rvt_typed_tag`.
fn is_st0806_rvt_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=21)
}

/// ST 0806.4 POI (Table 8-2) / AOI (Table 8-3) typed tags — both nested
/// sets share the same 1..=10 tag universe. Mirrors tst-py's
/// `is_st0806_poi_aoi_typed_tag`.
fn is_st0806_poi_aoi_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=10)
}

/// Map a Rust `RvtPoiType` to its ST 0806.4 Table 8-2 Tag 5 wire codepoint —
/// tst-core's own table (`RvtPoiType::to_wire`, public since Arc 2 R2). No
/// local copy, so no `#[non_exhaustive]` wildcard and nothing to drift.
fn poi_type_code(v: RustRvtPoiType) -> u8 {
    v.to_wire()
}

/// Inverse of `poi_type_code`.
fn poi_type_from_code(b: u8) -> RustRvtPoiType {
    RustRvtPoiType::from_wire(b)
}

/// Map a Rust `RvtAoiType` to its ST 0806.4 Table 8-3 Tag 6 wire codepoint —
/// tst-core's own table (`RvtAoiType::to_wire`, public since Arc 2 R2). Code 3
/// is "Reserved" here vs. "Target" for `RvtPoiType`.
fn aoi_type_code(v: RustRvtAoiType) -> u8 {
    v.to_wire()
}

/// Inverse of `aoi_type_code`.
fn aoi_type_from_code(b: u8) -> RustRvtAoiType {
    RustRvtAoiType::from_wire(b)
}

// -----------------------------------------------------------------------
// Decode entry points
// -----------------------------------------------------------------------

/// `org.tstrans.klv.Klv.nDecodeRvt(byte[]) -> RvtLs`
///
/// Decodes an RVT LS body (no UL / outer BER wrapper). On success, builds
/// and returns a Java `RvtLs`. On failure, throws a `KlvDecodeException`
/// and returns null.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nDecodeRvt<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    buf: JByteArray<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodeRvt: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_rvt(&bytes) {
            Ok(ls) => build_rvt_ls(env, &ls).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.nDecodeRvtStandalone(byte[]) -> RvtLs`
///
/// Decodes a standalone RVT LS: 16-byte UL + BER length + body, verifying
/// the CRC-32/MPEG-2 checksum (Tag 1) when present.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nDecodeRvtStandalone<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    buf: JByteArray<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodeRvtStandalone: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_rvt_standalone(&bytes) {
            Ok(ls) => build_rvt_ls(env, &ls).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

// -----------------------------------------------------------------------
// Encode entry points
// -----------------------------------------------------------------------

/// `org.tstrans.klv.Klv.nEncodeRvt(RvtLs) -> byte[]`
///
/// Reads all fields from the Java `RvtLs` record, builds a Rust `RvtLs`,
/// calls `encode_to_vec` (embedded body — no UL, no BER length, no Tag 1
/// CRC). Mirrors tst-py's `encode_rvt`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeRvt<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match read_rvt_ls(env, &record) {
            Ok(rust_rec) => match encode_rvt(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("nEncodeRvt: byte_array_from_slice failed: {e}"),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, &e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeRvt: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.nEncodeRvtStandalone(RvtLs) -> byte[]`
///
/// Same read path as `nEncodeRvt`, calls `encode_to_vec_standalone`
/// instead: `[RVT_LS_UL:16][outer BER length][Tag2 timestamp first][body]
/// [Tag1 CRC-32/MPEG-2 last]` per ST 0806.4-02/-04. Mirrors tst-py's
/// `encode_rvt_standalone`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeRvtStandalone<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match read_rvt_ls(env, &record) {
            Ok(rust_rec) => match encode_rvt_standalone(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("nEncodeRvtStandalone: byte_array_from_slice failed: {e}"),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, &e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeRvtStandalone: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    })
}

// -----------------------------------------------------------------------
// Rust → Java builders (decode path)
// -----------------------------------------------------------------------

/// Build a `org.tstrans.klv.RvtUserData` Java record from a Rust
/// `RvtUserData`. Plain record (no Builder) — only two differently-typed
/// fields, no transposition hazard.
fn build_user_data(env: &mut JNIEnv<'_>, ud: &RustRvtUserData) -> jni::errors::Result<jobject> {
    let buf =
        wrap_heap_byte_buffer(env, &ud.data).map_err(|()| jni::errors::Error::JavaException)?;
    let obj = env.new_object(
        "org/tstrans/klv/RvtUserData",
        "(ILjava/nio/ByteBuffer;)V",
        &[
            JValue::Int(i32::from(ud.numeric_id_raw)),
            JValue::Object(&buf),
        ],
    )?;
    Ok(obj.into_raw())
}

/// Build a `org.tstrans.klv.RvtPoi` Java record from a Rust `RvtPoi` via
/// its public mutable `Builder`. Called inside the caller's 64-slot
/// `with_local_frame` (matches `VTargetPack`'s precedent) — no separate
/// `ensure_local_capacity` needed here.
fn build_poi(env: &mut JNIEnv<'_>, p: &RustRvtPoi) -> jni::errors::Result<jobject> {
    let b = env.new_object(POI_BUILDER_CLASS, "()V", &[])?;

    if let Some(v) = p.number {
        env.call_method(
            &b,
            "number",
            POI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    if let Some(v) = p.lat_deg {
        env.call_method(&b, "latDeg", POI_BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = p.lon_deg {
        env.call_method(&b, "lonDeg", POI_BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = p.alt_m {
        env.call_method(&b, "altM", POI_BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(t) = p.poi_type {
        let code = poi_type_code(t);
        env.call_method(
            &b,
            "poiTypeCode",
            POI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(code))],
        )?;
    }
    if let Some(ref s) = p.text {
        let j = env.new_string(s)?;
        env.call_method(&b, "text", POI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = p.source_icon {
        let j = env.new_string(s)?;
        env.call_method(&b, "sourceIcon", POI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = p.source_id {
        let j = env.new_string(s)?;
        env.call_method(&b, "sourceId", POI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = p.label {
        let j = env.new_string(s)?;
        env.call_method(&b, "label", POI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = p.operation_id {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "operationId",
            POI_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    let sentinel_list = build_long_list(env, p.sentinel_tags.iter().map(|&t| i64::from(t)))?;
    env.call_method(
        &b,
        "sentinelTags",
        POI_BUILDER_SIG_LIST,
        &[JValue::Object(&sentinel_list)],
    )?;

    let unk_list = build_unknown_list(env, &p.unknown)?;
    env.call_method(
        &b,
        "unknown",
        POI_BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    let fe_list = build_field_errors(env, &p.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        POI_BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/RvtPoi;", &[])?
        .l()?;
    Ok(built.into_raw())
}

/// Build a `org.tstrans.klv.RvtAoi` Java record from a Rust `RvtAoi` via
/// its public mutable `Builder`. Same per-item-frame contract as
/// [`build_poi`].
fn build_aoi(env: &mut JNIEnv<'_>, a: &RustRvtAoi) -> jni::errors::Result<jobject> {
    let b = env.new_object(AOI_BUILDER_CLASS, "()V", &[])?;

    if let Some(v) = a.number {
        env.call_method(
            &b,
            "number",
            AOI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    if let Some(v) = a.corner_lat_p1_deg {
        env.call_method(
            &b,
            "cornerLatP1Deg",
            AOI_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = a.corner_lon_p1_deg {
        env.call_method(
            &b,
            "cornerLonP1Deg",
            AOI_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = a.corner_lat_p3_deg {
        env.call_method(
            &b,
            "cornerLatP3Deg",
            AOI_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = a.corner_lon_p3_deg {
        env.call_method(
            &b,
            "cornerLonP3Deg",
            AOI_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(t) = a.aoi_type {
        let code = aoi_type_code(t);
        env.call_method(
            &b,
            "aoiTypeCode",
            AOI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(code))],
        )?;
    }
    if let Some(ref s) = a.text {
        let j = env.new_string(s)?;
        env.call_method(&b, "text", AOI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = a.source_id {
        let j = env.new_string(s)?;
        env.call_method(&b, "sourceId", AOI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = a.label {
        let j = env.new_string(s)?;
        env.call_method(&b, "label", AOI_BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref s) = a.operation_id {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "operationId",
            AOI_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    let sentinel_list = build_long_list(env, a.sentinel_tags.iter().map(|&t| i64::from(t)))?;
    env.call_method(
        &b,
        "sentinelTags",
        AOI_BUILDER_SIG_LIST,
        &[JValue::Object(&sentinel_list)],
    )?;

    let unk_list = build_unknown_list(env, &a.unknown)?;
    env.call_method(
        &b,
        "unknown",
        AOI_BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    let fe_list = build_field_errors(env, &a.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        AOI_BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/RvtAoi;", &[])?
        .l()?;
    Ok(built.into_raw())
}

/// Build a `org.tstrans.klv.RvtLs` Java record from a Rust `RvtLs` via its
/// public mutable `Builder`. See the module doc for the `ensure_local_capacity`
/// arithmetic. Each `RvtUserData`/`RvtPoi`/`RvtAoi` is built AND added to its
/// list inside its own local frame so per-item refs are reclaimed before the
/// next iteration — bounds the live local-ref count to O(1) per item
/// regardless of list length (mirrors `build_vmti` in `st0903.rs`).
fn build_rvt_ls(env: &mut JNIEnv<'_>, v: &RustRvtLs) -> jni::errors::Result<jobject> {
    env.ensure_local_capacity(128)?;

    let b = env.new_object(RVT_BUILDER_CLASS, "()V", &[])?;

    // Tag 1 — crc32 (u32 → long)
    if let Some(c) = v.crc32 {
        env.call_method(
            &b,
            "crc32",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(c))],
        )?;
    }
    // Tag 2 — timestampUs (u64 → long via reinterpret cast)
    if let Some(t) = v.timestamp_us {
        env.call_method(
            &b,
            "timestampUs",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(t as i64)],
        )?;
    }
    // Tag 3 — platformTrueAirspeed (u16 → int)
    if let Some(n) = v.platform_true_airspeed {
        env.call_method(
            &b,
            "platformTrueAirspeed",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 4 — platformIndicatedAirspeed (u16 → int)
    if let Some(n) = v.platform_indicated_airspeed {
        env.call_method(
            &b,
            "platformIndicatedAirspeed",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 5 — telemetryAccuracyIndicator (u8 → int)
    if let Some(n) = v.telemetry_accuracy_indicator {
        env.call_method(
            &b,
            "telemetryAccuracyIndicator",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 6 — fragCircleRadiusM (u16 → int)
    if let Some(n) = v.frag_circle_radius_m {
        env.call_method(
            &b,
            "fragCircleRadiusM",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 7 — frameCode (u32 → long)
    if let Some(n) = v.frame_code {
        env.call_method(
            &b,
            "frameCode",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }
    // Tag 8 — rvtLsVersion (u8 → int)
    if let Some(n) = v.rvt_ls_version {
        env.call_method(
            &b,
            "rvtLsVersion",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 9 — videoDataRate (u32 → long)
    if let Some(n) = v.video_data_rate {
        env.call_method(
            &b,
            "videoDataRate",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }
    // Tag 10 — digitalVideoFileFormat (UTF-8 String)
    if let Some(ref s) = v.digital_video_file_format {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "digitalVideoFileFormat",
            RVT_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 11 — userDefined: build list, each item inside its own 16-slot frame.
    let ud_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for ud in &v.user_defined {
        env.with_local_frame(16, |inner_env| {
            let raw = build_user_data(inner_env, ud)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &ud_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok::<_, jni::errors::Error>(())
        })?;
    }
    env.call_method(
        &b,
        "userDefined",
        RVT_BUILDER_SIG_LIST,
        &[JValue::Object(&ud_list)],
    )?;

    // Tag 12 — pointsOfInterest: build list, each item inside its own 64-slot frame.
    let poi_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for poi in &v.points_of_interest {
        env.with_local_frame(64, |inner_env| {
            let raw = build_poi(inner_env, poi)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &poi_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok::<_, jni::errors::Error>(())
        })?;
    }
    env.call_method(
        &b,
        "pointsOfInterest",
        RVT_BUILDER_SIG_LIST,
        &[JValue::Object(&poi_list)],
    )?;

    // Tag 13 — areasOfInterest: build list, each item inside its own 64-slot frame.
    let aoi_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for aoi in &v.areas_of_interest {
        env.with_local_frame(64, |inner_env| {
            let raw = build_aoi(inner_env, aoi)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &aoi_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok::<_, jni::errors::Error>(())
        })?;
    }
    env.call_method(
        &b,
        "areasOfInterest",
        RVT_BUILDER_SIG_LIST,
        &[JValue::Object(&aoi_list)],
    )?;

    // Tag 14 — aircraftMgrsZone (u8 → int)
    if let Some(n) = v.aircraft_mgrs_zone {
        env.call_method(
            &b,
            "aircraftMgrsZone",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 15 — aircraftMgrsBandGrid (3-char String)
    if let Some(ref s) = v.aircraft_mgrs_band_grid {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "aircraftMgrsBandGrid",
            RVT_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 16 — aircraftMgrsEastingM (u24 → long)
    if let Some(n) = v.aircraft_mgrs_easting_m {
        env.call_method(
            &b,
            "aircraftMgrsEastingM",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }
    // Tag 17 — aircraftMgrsNorthingM (u24 → long)
    if let Some(n) = v.aircraft_mgrs_northing_m {
        env.call_method(
            &b,
            "aircraftMgrsNorthingM",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }
    // Tag 18 — frameCenterMgrsZone (u8 → int)
    if let Some(n) = v.frame_center_mgrs_zone {
        env.call_method(
            &b,
            "frameCenterMgrsZone",
            RVT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }
    // Tag 19 — frameCenterMgrsBandGrid (3-char String)
    if let Some(ref s) = v.frame_center_mgrs_band_grid {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "frameCenterMgrsBandGrid",
            RVT_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 20 — frameCenterMgrsEastingM (u24 → long)
    if let Some(n) = v.frame_center_mgrs_easting_m {
        env.call_method(
            &b,
            "frameCenterMgrsEastingM",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }
    // Tag 21 — frameCenterMgrsNorthingM (u24 → long)
    if let Some(n) = v.frame_center_mgrs_northing_m {
        env.call_method(
            &b,
            "frameCenterMgrsNorthingM",
            RVT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }

    // unknown — always set (even if empty)
    let unk_list = build_unknown_list(env, &v.unknown)?;
    env.call_method(
        &b,
        "unknown",
        RVT_BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    // fieldErrors — always set (even if empty)
    let fe_list = build_field_errors(env, &v.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        RVT_BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/RvtLs;", &[])?
        .l()?;
    Ok(built.into_raw())
}

// -----------------------------------------------------------------------
// Java → Rust readers (encode path)
// -----------------------------------------------------------------------

/// Read a Java `RvtUserData` record into a Rust `RvtUserData`.
fn read_user_data(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustRvtUserData> {
    let raw = env.call_method(rec, "numericIdRaw", "()I", &[])?.i()?;
    let numeric_id_raw = checked_u8(env, i64::from(raw), "numericIdRaw")?;
    let buf_obj = env
        .call_method(rec, "data", "()Ljava/nio/ByteBuffer;", &[])?
        .l()?;
    let data = if buf_obj.is_null() {
        Vec::new()
    } else {
        read_byte_buffer(env, &buf_obj)?
    };
    Ok(RustRvtUserData {
        numeric_id_raw,
        data,
    })
}

/// Read a Java `RvtPoi` record into a Rust `RvtPoi`. Mirrors tst-py's
/// `py_to_rvt_poi`. Called inside the caller's 64-slot `with_local_frame`.
fn read_poi(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustRvtPoi> {
    let mut p = RustRvtPoi::default();

    if let Some(v) = read_nullable_int(env, rec, "number")? {
        p.number = Some(checked_u16(env, i64::from(v), "number")?);
    }
    p.lat_deg = read_nullable_double(env, rec, "latDeg")?;
    p.lon_deg = read_nullable_double(env, rec, "lonDeg")?;
    p.alt_m = read_nullable_double(env, rec, "altM")?;
    if let Some(v) = read_nullable_int(env, rec, "poiTypeCode")? {
        let code = checked_u8(env, i64::from(v), "poiTypeCode")?;
        p.poi_type = Some(poi_type_from_code(code));
    }
    p.text = read_nullable_string(env, rec, "text")?;
    p.source_icon = read_nullable_string(env, rec, "sourceIcon")?;
    p.source_id = read_nullable_string(env, rec, "sourceId")?;
    p.label = read_nullable_string(env, rec, "label")?;
    p.operation_id = read_nullable_string(env, rec, "operationId")?;

    let sentinel_obj = env
        .call_method(rec, "sentinelTags", "()Ljava/util/List;", &[])?
        .l()?;
    for v in read_long_list(env, &sentinel_obj)? {
        let Ok(tag) = u32::try_from(v) else {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("RvtPoi.sentinelTags entry out of u32 range: {v}"),
            );
            return Err(jni::errors::Error::JavaException);
        };
        p.sentinel_tags.push(tag);
    }

    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    p.unknown = read_unknown_list(env, &unk_obj, is_st0806_poi_aoi_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped.
    Ok(p)
}

/// Read a Java `RvtAoi` record into a Rust `RvtAoi`. Mirrors tst-py's
/// `py_to_rvt_aoi`. Same per-item-frame contract as [`read_poi`].
fn read_aoi(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustRvtAoi> {
    let mut a = RustRvtAoi::default();

    if let Some(v) = read_nullable_int(env, rec, "number")? {
        a.number = Some(checked_u16(env, i64::from(v), "number")?);
    }
    a.corner_lat_p1_deg = read_nullable_double(env, rec, "cornerLatP1Deg")?;
    a.corner_lon_p1_deg = read_nullable_double(env, rec, "cornerLonP1Deg")?;
    a.corner_lat_p3_deg = read_nullable_double(env, rec, "cornerLatP3Deg")?;
    a.corner_lon_p3_deg = read_nullable_double(env, rec, "cornerLonP3Deg")?;
    if let Some(v) = read_nullable_int(env, rec, "aoiTypeCode")? {
        let code = checked_u8(env, i64::from(v), "aoiTypeCode")?;
        a.aoi_type = Some(aoi_type_from_code(code));
    }
    a.text = read_nullable_string(env, rec, "text")?;
    a.source_id = read_nullable_string(env, rec, "sourceId")?;
    a.label = read_nullable_string(env, rec, "label")?;
    a.operation_id = read_nullable_string(env, rec, "operationId")?;

    let sentinel_obj = env
        .call_method(rec, "sentinelTags", "()Ljava/util/List;", &[])?
        .l()?;
    for v in read_long_list(env, &sentinel_obj)? {
        let Ok(tag) = u32::try_from(v) else {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("RvtAoi.sentinelTags entry out of u32 range: {v}"),
            );
            return Err(jni::errors::Error::JavaException);
        };
        a.sentinel_tags.push(tag);
    }

    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    a.unknown = read_unknown_list(env, &unk_obj, is_st0806_poi_aoi_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped.
    Ok(a)
}

/// Read all fields from a Java `RvtLs` record into a Rust `RvtLs`. Mirrors
/// tst-py's `py_to_rvt_ls`. See the module doc for the
/// `ensure_local_capacity` arithmetic.
fn read_rvt_ls(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustRvtLs> {
    let mut r = RustRvtLs::default();

    env.ensure_local_capacity(128)?;

    if let Some(v) = read_nullable_long(env, rec, "crc32")? {
        r.crc32 = Some(checked_u32(env, v, "crc32")?);
    }
    if let Some(v) = read_nullable_long(env, rec, "timestampUs")? {
        r.timestamp_us = Some(v as u64);
    }
    if let Some(v) = read_nullable_int(env, rec, "platformTrueAirspeed")? {
        r.platform_true_airspeed = Some(checked_u16(env, i64::from(v), "platformTrueAirspeed")?);
    }
    if let Some(v) = read_nullable_int(env, rec, "platformIndicatedAirspeed")? {
        r.platform_indicated_airspeed =
            Some(checked_u16(env, i64::from(v), "platformIndicatedAirspeed")?);
    }
    if let Some(v) = read_nullable_int(env, rec, "telemetryAccuracyIndicator")? {
        r.telemetry_accuracy_indicator =
            Some(checked_u8(env, i64::from(v), "telemetryAccuracyIndicator")?);
    }
    if let Some(v) = read_nullable_int(env, rec, "fragCircleRadiusM")? {
        r.frag_circle_radius_m = Some(checked_u16(env, i64::from(v), "fragCircleRadiusM")?);
    }
    if let Some(v) = read_nullable_long(env, rec, "frameCode")? {
        r.frame_code = Some(checked_u32(env, v, "frameCode")?);
    }
    if let Some(v) = read_nullable_int(env, rec, "rvtLsVersion")? {
        r.rvt_ls_version = Some(checked_u8(env, i64::from(v), "rvtLsVersion")?);
    }
    if let Some(v) = read_nullable_long(env, rec, "videoDataRate")? {
        r.video_data_rate = Some(checked_u32(env, v, "videoDataRate")?);
    }
    r.digital_video_file_format = read_nullable_string(env, rec, "digitalVideoFileFormat")?;

    // Tag 11 — userDefined (List<RvtUserData>)
    let ud_obj = env
        .call_method(rec, "userDefined", "()Ljava/util/List;", &[])?
        .l()?;
    let ud_size = env.call_method(&ud_obj, "size", "()I", &[])?.i()?;
    for i in 0..ud_size {
        env.with_local_frame(16, |inner_env| {
            let item = inner_env
                .call_method(&ud_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            r.user_defined.push(read_user_data(inner_env, &item)?);
            Ok::<_, jni::errors::Error>(())
        })?;
    }

    // Tag 12 — pointsOfInterest (List<RvtPoi>)
    let poi_obj = env
        .call_method(rec, "pointsOfInterest", "()Ljava/util/List;", &[])?
        .l()?;
    let poi_size = env.call_method(&poi_obj, "size", "()I", &[])?.i()?;
    for i in 0..poi_size {
        env.with_local_frame(64, |inner_env| {
            let item = inner_env
                .call_method(&poi_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            r.points_of_interest.push(read_poi(inner_env, &item)?);
            Ok::<_, jni::errors::Error>(())
        })?;
    }

    // Tag 13 — areasOfInterest (List<RvtAoi>)
    let aoi_obj = env
        .call_method(rec, "areasOfInterest", "()Ljava/util/List;", &[])?
        .l()?;
    let aoi_size = env.call_method(&aoi_obj, "size", "()I", &[])?.i()?;
    for i in 0..aoi_size {
        env.with_local_frame(64, |inner_env| {
            let item = inner_env
                .call_method(&aoi_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            r.areas_of_interest.push(read_aoi(inner_env, &item)?);
            Ok::<_, jni::errors::Error>(())
        })?;
    }

    if let Some(v) = read_nullable_int(env, rec, "aircraftMgrsZone")? {
        r.aircraft_mgrs_zone = Some(checked_u8(env, i64::from(v), "aircraftMgrsZone")?);
    }
    r.aircraft_mgrs_band_grid = read_nullable_string(env, rec, "aircraftMgrsBandGrid")?;
    if let Some(v) = read_nullable_long(env, rec, "aircraftMgrsEastingM")? {
        r.aircraft_mgrs_easting_m = Some(checked_u32(env, v, "aircraftMgrsEastingM")?);
    }
    if let Some(v) = read_nullable_long(env, rec, "aircraftMgrsNorthingM")? {
        r.aircraft_mgrs_northing_m = Some(checked_u32(env, v, "aircraftMgrsNorthingM")?);
    }
    if let Some(v) = read_nullable_int(env, rec, "frameCenterMgrsZone")? {
        r.frame_center_mgrs_zone = Some(checked_u8(env, i64::from(v), "frameCenterMgrsZone")?);
    }
    r.frame_center_mgrs_band_grid = read_nullable_string(env, rec, "frameCenterMgrsBandGrid")?;
    if let Some(v) = read_nullable_long(env, rec, "frameCenterMgrsEastingM")? {
        r.frame_center_mgrs_easting_m = Some(checked_u32(env, v, "frameCenterMgrsEastingM")?);
    }
    if let Some(v) = read_nullable_long(env, rec, "frameCenterMgrsNorthingM")? {
        r.frame_center_mgrs_northing_m = Some(checked_u32(env, v, "frameCenterMgrsNorthingM")?);
    }

    // unknown (collision-drop per is_st0806_rvt_typed_tag)
    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    r.unknown = read_unknown_list(env, &unk_obj, is_st0806_rvt_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped.
    Ok(r)
}

#[cfg(test)]
mod wire_inventory {
    //! RVT POI/AOI type codes cross the JNI boundary as tst-core's own wire
    //! codepoints, for every variant in `ALL` (Arc 2 R2). See the twin module
    //! in `st0601.rs`.
    use tst_core::klv::st0806::{RvtAoiType, RvtPoiType};

    #[test]
    fn every_rvt_coded_enum_variant_round_trips_through_the_jni_codepoint() {
        for v in RvtPoiType::ALL {
            assert_eq!(super::poi_type_from_code(super::poi_type_code(*v)), *v);
        }
        for v in RvtAoiType::ALL {
            assert_eq!(super::aoi_type_from_code(super::aoi_type_code(*v)), *v);
        }
    }

    /// The JNI codepoint IS tst-core's wire codepoint (see the st0601 twin).
    #[test]
    fn the_jni_codepoint_is_tst_cores_wire_codepoint() {
        for v in RvtPoiType::ALL {
            assert_eq!(super::poi_type_code(*v), v.to_wire());
        }
        for v in RvtAoiType::ALL {
            assert_eq!(super::aoi_type_code(*v), v.to_wire());
        }
    }
}
