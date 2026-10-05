//! JNI surface for ST 0102 Security Metadata LS decode/encode.
//!
//! `nDecodeSecurity(byte[], boolean strict) -> SecurityLs` — calls
//! `tst_core::klv::st0102::decode` (lenient) or `decode_strict`, then builds
//! the Java `SecurityLs` via its public mutable `Builder` (the pattern every
//! KLV set's JNI module reuses). The 3 enum-typed fields are stored as boxed `Integer` raw
//! codepoints via the `int`-taking Builder setters; absent `Option` fields are
//! simply not set (null is the Builder default).
//!
//! `nEncodeSecurity(SecurityLs) -> byte[]` — reads each field from the Java
//! record via accessor `call_method`s, builds the Rust `SecurityLs`, calls
//! `encode_to_vec`, and maps any `KlvEncodeError` via
//! `crate::error::map_klv_encode_error`. The 3 enum fields are read as
//! nullable `Integer ...Code()` accessors (returning a boxed Integer or null)
//! — identical to tst-py's `enum_field_to_u8` pattern.
//!
//! ### Builder-call pattern (reused by every KLV set's JNI module)
//!
//! ```text
//! let b = env.new_object(BUILDER_CLASS, "()V", &[])?;
//! // per present int-typed field:
//! env.call_method(&b, "<setter>", BUILDER_SIG_INT, &[JValue::Int(v)])?;
//! // per present String field:
//! env.call_method(&b, "<setter>", BUILDER_SIG_STR, &[JValue::Object(&j_str)])?;
//! // per list field:
//! env.call_method(&b, "<setter>", BUILDER_SIG_LIST, &[JValue::Object(&list)])?;
//! // finalize:
//! let built = env.call_method(&b, "build", "()Lorg/tstrans/klv/SecurityLs;", &[])?.l()?;
//! ```
//!
//! VERIFY every descriptor against the actual Java Builder method signatures
//! — a mismatched signature fails at runtime with a `NoSuchMethodError`, not
//! at compile time.
//!
//! ### JNI local-ref capacity (MANDATORY)
//!
//! Each `new_string` / `new_object` in `build_<set>` creates a JNI local
//! reference that stays live until the function returns. The default JNI
//! local-ref table holds only ~16 slots, so a large set will overflow it and
//! abort the JVM. Every `build_<set>` MUST call
//! `env.ensure_local_capacity(field_count)?` at the top (here `32` covers ST
//! 0102's 17 fields + the builder + lists + JNI scratch).
//!
//! - **ST 0601 (~80 fields)** and **ST 0903 (nested target list)** WILL
//!   overflow the default table — size `ensure_local_capacity` to at least the
//!   field count.
//! - **ST 0903's `targets` list**: build each `VTargetPack` inside its own
//!   `env.with_local_frame(n, ...)` so the per-target refs are reclaimed before
//!   the next target — otherwise a frame of many targets accumulates refs
//!   without bound (mirrors the per-entry `with_local_frame` already used in
//!   `jutil::build_field_errors` / `build_unknown_list`).

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::jobject;
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification, SecurityLs,
    decode as decode_lenient, decode_strict, encode_strict_compliance, encode_to_vec,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};
use crate::jutil::{
    build_field_errors, build_unknown_list, checked_u8, checked_u16, read_nullable_int,
    read_nullable_string, read_unknown_list,
};

// Builder class + method descriptor constants.
// Matches: SecurityLs$Builder.<setter>(int) → SecurityLs$Builder
const BUILDER_CLASS: &str = "org/tstrans/klv/SecurityLs$Builder";
const BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/SecurityLs$Builder;";
const BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/SecurityLs$Builder;";
const BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/SecurityLs$Builder;";

/// ST 0102 typed tags: 1..=14, 22, 23, 24. Mirrors tst-py's `is_st0102_typed_tag`.
fn is_st0102_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=14 | 22 | 23 | 24)
}

/// `org.tstrans.klv.Klv.nDecodeSecurity(byte[], boolean strict) -> SecurityLs`
///
/// Decodes a ST 0102 body (no UL / outer BER wrapper). Uses lenient decode
/// when `strict = false`, strict decode when `strict = true`. On success,
/// builds and returns a Java `SecurityLs` record. On failure, throws a
/// `KlvDecodeException` and returns null.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nDecodeSecurity<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    buf: JByteArray<'local>,
    strict: jni::sys::jboolean,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodeSecurity: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let result = if strict != 0 {
            decode_strict(&bytes)
        } else {
            decode_lenient(&bytes)
        };
        match result {
            Ok(sec) => match build_security(env, &sec) {
                Ok(raw) => raw,
                Err(_) => {
                    // A JNI op inside build_security failed (e.g. OOM building a
                    // local ref). The JVM exception is already pending; return null
                    // so it propagates when control returns to Java.
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// Build a `org.tstrans.klv.SecurityLs` Java record from a Rust `SecurityLs`
/// via the public mutable `Builder`. Only present (non-None) fields are set;
/// the Builder leaves absent fields null by default.
///
/// This is the canonical Builder-marshalling pattern used by every KLV set's
/// JNI module. Each
/// `call_method` on `b` returns the same builder (mutates in place); the
/// return value is discarded here since we hold `b` separately.
fn build_security(env: &mut JNIEnv<'_>, s: &SecurityLs) -> jni::errors::Result<jobject> {
    // Each new_string below holds a JNI local ref live until this fn returns;
    // ST 0102 has 17 fields. Reserve enough table slots up front so a fully
    // populated record can't overflow the default ~16-slot table. See the
    // module-level "JNI local-ref capacity" note — every other KLV set's JNI
    // module does the same, sized to its own field count.
    env.ensure_local_capacity(32)?;

    let b = env.new_object(BUILDER_CLASS, "()V", &[])?;

    // Tag 1 — Security Classification: pass the raw u8 codepoint as int.
    if let Some(v) = s.security_classification {
        env.call_method(
            &b,
            "securityClassification",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v.to_u8()))],
        )?;
    }

    // Tag 2 — Classifying Country Coding Method: pass raw u8 codepoint as int.
    if let Some(v) = s.classifying_country_coding_method {
        env.call_method(
            &b,
            "classifyingCountryCodingMethod",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v.to_u8()))],
        )?;
    }

    // Tag 3 — Classifying Country (String)
    if let Some(ref v) = s.classifying_country {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "classifyingCountry",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 12 — Object Country Coding Method: pass raw u8 codepoint as int.
    if let Some(v) = s.object_country_coding_method {
        env.call_method(
            &b,
            "objectCountryCodingMethod",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v.to_u8()))],
        )?;
    }

    // Tag 13 — Object Country Codes (String, UTF-16 decoded)
    if let Some(ref v) = s.object_country_codes {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "objectCountryCodes",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 22 — Version (u16 → int)
    if let Some(v) = s.version {
        env.call_method(&b, "version", BUILDER_SIG_INT, &[JValue::Int(i32::from(v))])?;
    }

    // Tag 4 — SCI/SHI info
    if let Some(ref v) = s.sci_shi_info {
        let j = env.new_string(v)?;
        env.call_method(&b, "sciShiInfo", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }

    // Tag 5 — Caveats
    if let Some(ref v) = s.caveats {
        let j = env.new_string(v)?;
        env.call_method(&b, "caveats", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }

    // Tag 6 — Releasing Instructions
    if let Some(ref v) = s.releasing_instructions {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "releasingInstructions",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 7 — Classified By
    if let Some(ref v) = s.classified_by {
        let j = env.new_string(v)?;
        env.call_method(&b, "classifiedBy", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }

    // Tag 8 — Derived From
    if let Some(ref v) = s.derived_from {
        let j = env.new_string(v)?;
        env.call_method(&b, "derivedFrom", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }

    // Tag 9 — Classification Reason
    if let Some(ref v) = s.classification_reason {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "classificationReason",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 10 — Declassification Date ("YYYYMMDD")
    if let Some(ref v) = s.declassification_date {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "declassificationDate",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 11 — Classification Marking System
    if let Some(ref v) = s.classification_marking_system {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "classificationMarkingSystem",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 14 — Classification Comments
    if let Some(ref v) = s.classification_comments {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "classificationComments",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 23 — Classifying Country Coding Method Version Date ("YYYY-MM-DD")
    if let Some(ref v) = s.classifying_country_coding_method_version_date {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "classifyingCountryCodingMethodVersionDate",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 24 — Object Country Coding Method Version Date ("YYYY-MM-DD")
    if let Some(ref v) = s.object_country_coding_method_version_date {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "objectCountryCodingMethodVersionDate",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // field_errors — always set (even if empty).
    let fe_list = build_field_errors(env, &s.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    // unknown — always set (even if empty).
    let unk_list = build_unknown_list(env, &s.unknown)?;
    env.call_method(
        &b,
        "unknown",
        BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    // build() → SecurityLs
    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/SecurityLs;", &[])?
        .l()?;
    Ok(built.into_raw())
}

/// `org.tstrans.klv.Klv.nEncodeSecurity(SecurityLs) -> byte[]`
///
/// Reads all fields from the Java `SecurityLs` record via accessor calls,
/// builds a Rust `SecurityLs`, calls `encode_to_vec`, and returns the body
/// bytes (no UL / outer BER wrapper). On error, throws a `KlvEncodeException`
/// and returns null.
///
/// The 3 enum-typed fields are read as raw `Integer ...Code()` accessors
/// (nullable boxed int); null → `None` in Rust, non-null → `from_u8`.
/// Mirrors tst-py's `enum_field_to_u8` + `py_to_security_ls` pattern.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeSecurity<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match read_security(env, &record) {
            Ok(rust_rec) => match encode_to_vec(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("nEncodeSecurity: byte_array_from_slice failed: {e}"),
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
                        format!("nEncodeSecurity: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.nEncodeSecurityStrictCompliance(SecurityLs) -> byte[]`
///
/// Reads all fields from the Java `SecurityLs` record, builds a Rust `SecurityLs`,
/// calls `encode_strict_compliance`. Enforces all 6 mandatory-tag presence
/// (Tags 1, 2, 3, 12, 13, 22) before encoding. Mirrors tst-py's
/// `encode_security_strict_compliance`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeSecurityStrictCompliance<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match read_security(env, &record) {
            Ok(rust_rec) => match encode_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeSecurityStrictCompliance: byte_array_from_slice failed: {e}"
                            ),
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
                        format!("nEncodeSecurityStrictCompliance: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    })
}

/// Read all fields from a Java `SecurityLs` record into a Rust `SecurityLs`.
/// Mirrors tst-py's `py_to_security_ls`.
fn read_security(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<SecurityLs> {
    let mut r = SecurityLs::default();

    // --- 3 enum-typed fields (read as nullable Integer raw code) ---

    // Tag 1: securityClassificationCode() → Integer | null
    if let Some(code) = read_nullable_int(env, rec, "securityClassificationCode")? {
        let c = checked_u8(env, i64::from(code), "securityClassificationCode")?;
        r.security_classification = Some(SecurityClassification::from_u8(c));
    }

    // Tag 2: classifyingCountryCodingMethodCode() → Integer | null
    if let Some(code) = read_nullable_int(env, rec, "classifyingCountryCodingMethodCode")? {
        let c = checked_u8(env, i64::from(code), "classifyingCountryCodingMethodCode")?;
        r.classifying_country_coding_method = Some(ClassifyingCountryCodingMethod::from_u8(c));
    }

    // Tag 12: objectCountryCodingMethodCode() → Integer | null
    if let Some(code) = read_nullable_int(env, rec, "objectCountryCodingMethodCode")? {
        let c = checked_u8(env, i64::from(code), "objectCountryCodingMethodCode")?;
        r.object_country_coding_method = Some(ObjectCountryCodingMethod::from_u8(c));
    }

    // --- Integer field: version (u16) ---
    if let Some(v) = read_nullable_int(env, rec, "version")? {
        r.version = Some(checked_u16(env, i64::from(v), "version")?);
    }

    // --- String fields (null → None) ---
    r.classifying_country = read_nullable_string(env, rec, "classifyingCountry")?;
    r.object_country_codes = read_nullable_string(env, rec, "objectCountryCodes")?;
    r.sci_shi_info = read_nullable_string(env, rec, "sciShiInfo")?;
    r.caveats = read_nullable_string(env, rec, "caveats")?;
    r.releasing_instructions = read_nullable_string(env, rec, "releasingInstructions")?;
    r.classified_by = read_nullable_string(env, rec, "classifiedBy")?;
    r.derived_from = read_nullable_string(env, rec, "derivedFrom")?;
    r.classification_reason = read_nullable_string(env, rec, "classificationReason")?;
    r.declassification_date = read_nullable_string(env, rec, "declassificationDate")?;
    r.classification_marking_system =
        read_nullable_string(env, rec, "classificationMarkingSystem")?;
    r.classification_comments = read_nullable_string(env, rec, "classificationComments")?;
    r.classifying_country_coding_method_version_date =
        read_nullable_string(env, rec, "classifyingCountryCodingMethodVersionDate")?;
    r.object_country_coding_method_version_date =
        read_nullable_string(env, rec, "objectCountryCodingMethodVersionDate")?;

    // --- unknown list (collision-drop per is_st0102_typed_tag) ---
    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    r.unknown = read_unknown_list(env, &unk_obj, is_st0102_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped (mirrors tst-py).
    Ok(r)
}
