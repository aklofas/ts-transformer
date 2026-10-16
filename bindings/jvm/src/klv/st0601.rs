//! JNI surface for ST 0601 UAS Datalink LS decode/encode.
//!
//! `nDecodeUasDatalink(byte[], boolean strict, boolean compliance) -> UasDatalinkLs` —
//! dispatches: `compliance=true` → `decode_strict_compliance`; else `strict=true` →
//! `decode_strict`; else `decode` (lenient). Builds the Java `UasDatalinkLs` via its
//! public mutable `Builder` (the Builder-marshalling pattern used throughout this module).
//!
//! `nEncodeUasDatalinkWithPolicy(UasDatalinkLs, int policy) -> byte[]` — reads all
//! fields via accessor `call_method`s, builds a Rust `UasDatalinkLs`, calls
//! `encode_to_vec_with`. The Java 1-arg `encodeUasDatalink(record)` delegates here
//! with `policy=0` (ERROR). The 2-arg overload passes `policy=1` for INDICATOR.
//! Mirrors tst-py's generated `UasDatalinkLs` converter, including the 16-byte
//! universal-label validation and the typed-tag collision drop on `unknown`.
//!
//! `nEncodeUasDatalinkStrictCompliance(UasDatalinkLs) -> byte[]` — same read path,
//! calls `encode_strict_compliance` instead.
//!
//! ### Local references
//!
//! `build_uas_datalink_ls` and `read_uas_datalink_ls` are generated from the
//! record's declaration. Each reserves `2 × fields + 32` local references up
//! front and builds or reads every list item inside its own local frame, so
//! the live count is bounded by the field count and not by a list's length.
//! `St0601PacksTest.fullyPopulatedWpcFieldsRoundTrip` and the long-list tests
//! exercise it.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jlong, jobject, jstring};
use tst_core::error::KlvFieldError;
use tst_core::klv::st0601::{
    EncodeConfig, OutOfRangePolicy, St0601SentinelMeaning, UasDatalinkLs, decode as decode_lenient,
    decode_strict, decode_strict_compliance, encode_strict_compliance, encode_to_vec_with,
    st0601_sentinel_meaning,
};
use tst_core::klv::st1010::{SdccFlp, decode_sdcc_flp, encode_sdcc_flp_mode2};

use crate::error::{map_klv_decode_error, map_klv_encode_error, throw_klv_decode};
use tst_pipeline::binding::BindingErrorKind;

// -----------------------------------------------------------------------
// Decode entry point
// -----------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.klv.Klv.nDecodeUasDatalink(byte[], boolean strict, boolean compliance)`
    ///
    /// Decodes a full ST 0601 record (full buffer including the 16-byte UL).
    /// Dispatches: compliance → `decode_strict_compliance`; strict → `decode_strict`;
    /// else → lenient `decode`. On success, builds and returns a Java `UasDatalinkLs`
    /// via its `Builder`. On failure, throws a `KlvDecodeException` and returns null.
    fn Java_org_tstrans_klv_Klv_nDecodeUasDatalink<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
        strict: jni::sys::jboolean,
        compliance: jni::sys::jboolean,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodeUasDatalink: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let result = if compliance != 0 {
            decode_strict_compliance(&bytes)
        } else if strict != 0 {
            decode_strict(&bytes)
        } else {
            decode_lenient(&bytes)
        };
        match result {
            Ok(rec) => match crate::generated::st0601::build_uas_datalink_ls(env, &rec) {
                Ok(obj) => obj.into_raw(),
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    }
}

// -----------------------------------------------------------------------
// Encode entry points
// -----------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeUasDatalinkWithPolicy(UasDatalinkLs, int policy) -> byte[]`
    ///
    /// Reads all fields from the Java `UasDatalinkLs` record, builds a Rust
    /// `UasDatalinkLs`, then calls [`encode_to_vec_with`] using an
    /// [`OutOfRangePolicy`] mapped from the `policy` int:
    /// - `0` → [`OutOfRangePolicy::Error`] (throws on any out-of-range value)
    /// - `1` → [`OutOfRangePolicy::Indicator`] (emits the spec's Out-of-Range
    ///   special for eligible linear-range tags: 6, 7, 50, 51, 52, 79, 80,
    ///   90–93; separately, the 14 IMAPB tags — 96, 103-105, 109,
    ///   112-114, 117-120, 132, 134 — get their own ST 1201.5 BelowMin/AboveMax
    ///   special instead of the INT_MIN sentinel)
    ///
    /// Any other ordinal throws `java.lang.IllegalArgumentException` — this
    /// signals enum drift between the Java and Rust layers.
    fn Java_org_tstrans_klv_Klv_nEncodeUasDatalinkWithPolicy<'local>(
        env,
        _c,
        record: JObject<'local>,
        policy: jint,
    ) -> jobject = std::ptr::null_mut() => {
        // Exact-ordinal validation: out-of-range means enum drift between
        // the Java and Rust layers — fail loudly.
        let oor_policy = match policy {
            0 => OutOfRangePolicy::Error,
            1 => OutOfRangePolicy::Indicator,
            other => {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("unknown OutOfRangePolicy ordinal {other}"),
                );
                return JObject::null().into_raw();
            }
        };
        let mut opts = EncodeConfig::default();
        opts.out_of_range_policy = oor_policy;
        match crate::generated::st0601::read_uas_datalink_ls(env, &record) {
            Ok(rust_rec) => match encode_to_vec_with(&rust_rec, &opts) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeUasDatalinkWithPolicy: byte_array_from_slice failed: {e}"
                            ),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeUasDatalinkWithPolicy: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeUasDatalinkStrictCompliance(UasDatalinkLs) -> byte[]`
    ///
    /// Reads all fields from the Java `UasDatalinkLs` record, builds a Rust
    /// `UasDatalinkLs`, calls `encode_strict_compliance`. Enforces mandatory-tag
    /// presence (Tag 2 / Tag 65 / Tag 1) and structural ordering. Mirrors tst-py's
    /// `encode_uas_datalink_strict_compliance`.
    fn Java_org_tstrans_klv_Klv_nEncodeUasDatalinkStrictCompliance<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0601::read_uas_datalink_ls(env, &record) {
            Ok(rust_rec) => match encode_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeUasDatalinkStrictCompliance: byte_array_from_slice failed: {e}"
                            ),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeUasDatalinkStrictCompliance: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

// -----------------------------------------------------------------------
// Sentinel-meaning lookup
// -----------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.klv.Klv.nSt0601SentinelMeaning(long tag) -> String | null`
    ///
    /// Delegates to [`st0601_sentinel_meaning`] (the ST 0601.19 §7.5 special-value
    /// assignments), mapping the [`St0601SentinelMeaning`] variants to the same
    /// strings tst-py returns: `OutOfRange` → `"out_of_range"`, `Reserved` →
    /// `"reserved"`, `NotAvailable` → `"not_available"`, no assignment → null.
    /// Total over its input: a tag outside the u32 range simply has no assigned
    /// meaning (null) — the only failure path is JNI string allocation.
    fn Java_org_tstrans_klv_Klv_nSt0601SentinelMeaning<'local>(
        env,
        _c,
        tag: jlong,
    ) -> jstring = std::ptr::null_mut() => {
        let meaning = u32::try_from(tag).ok().and_then(st0601_sentinel_meaning);
        let name = match meaning {
            Some(St0601SentinelMeaning::OutOfRange) => "out_of_range",
            Some(St0601SentinelMeaning::Reserved) => "reserved",
            Some(St0601SentinelMeaning::NotAvailable) => "not_available",
            // `#[non_exhaustive]` in tst-core: no current variant reaches the
            // wildcard; a future variant surfaces as null until mirrored here.
            Some(_) | None => return std::ptr::null_mut(),
        };
        match env.new_string(name) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nSt0601SentinelMeaning: new_string failed: {e}"),
                );
                std::ptr::null_mut()
            }
        }
    }
}

/// Thin public wrapper so `st1204::validateMismmsNative` can call the shared
/// reader without duplicating it. All the real logic lives in the generated `read_uas_datalink_ls`.
pub fn read_uas_datalink_for_validate(
    env: &mut JNIEnv<'_>,
    rec: &JObject<'_>,
) -> jni::errors::Result<UasDatalinkLs> {
    crate::generated::st0601::read_uas_datalink_ls(env, rec)
}

// ---------------------------------------------------------------------------
// klv::st1010 SDCC-FLP — general-purpose, standalone entry points. NOT
// ST 0601-specific (see the `SdccFlp`/`SdccFlpField` module docs) — kept in
// this file rather than a new module, since the only caller-visible surface
// is `Klv.decodeSdccFlp`/`Klv.encodeSdccFlpMode2`.
// ---------------------------------------------------------------------------

/// Map a standalone Rust `KlvFieldError` — e.g. from `decode_sdcc_flp`, which
/// returns one directly rather than embedding it in a `KlvDecodeError` — to a
/// `KlvDecodeException`. Mirrors `map_klv_decode_error`'s
/// `FieldError(_) => "MALFORMED_BYTES"` bucket: this function's caller sees
/// exactly one field-level failure with no surrounding local-set context, so
/// every variant folds to that bucket except the substrate-framing one.
/// Mirrors tst-py's `klv_field_error_to_pyerr`.
fn map_sdcc_field_error(env: &mut JNIEnv, e: &KlvFieldError) {
    let msg = e.to_string();
    let kind = match e {
        KlvFieldError::TruncatedField { .. } => BindingErrorKind::KlvDecodeTruncatedSet,
        _ => BindingErrorKind::KlvDecodeMalformedBytes,
    };
    throw_klv_decode(env, kind, &msg);
}

/// Build a `org.tstrans.klv.SdccFlp` Java record from a decoded Rust `SdccFlp`.
fn build_sdcc_flp(env: &mut JNIEnv<'_>, m: &SdccFlp) -> jni::errors::Result<jobject> {
    let std_devs_arr = env.new_double_array(m.std_devs.len() as i32)?;
    env.set_double_array_region(&std_devs_arr, 0, &m.std_devs)?;
    let correlations_arr = env.new_double_array(m.correlations.len() as i32)?;
    env.set_double_array_region(&correlations_arr, 0, &m.correlations)?;
    let presence: Vec<u8> = m.correlation_present.iter().map(|&b| u8::from(b)).collect();
    let presence_arr = env.new_boolean_array(presence.len() as i32)?;
    env.set_boolean_array_region(&presence_arr, 0, &presence)?;
    let obj = env.new_object(
        "org/tstrans/klv/SdccFlp",
        "(J[D[D[Z)V",
        &[
            JValue::Long(m.matrix_size as i64),
            JValue::Object(&std_devs_arr),
            JValue::Object(&correlations_arr),
            JValue::Object(&presence_arr),
        ],
    )?;
    Ok(obj.into_raw())
}

/// Read a Java `double[]` into a `Vec<f64>`.
fn read_double_array(
    env: &mut JNIEnv,
    arr: &jni::objects::JDoubleArray,
) -> jni::errors::Result<Vec<f64>> {
    let len = env.get_array_length(arr)?;
    let mut buf = vec![0.0f64; len as usize];
    env.get_double_array_region(arr, 0, &mut buf)?;
    Ok(buf)
}

jni_native! {
    /// `org.tstrans.klv.Klv.decodeSdccFlpNative(byte[]) -> SdccFlp`
    ///
    /// Decodes a MISB ST 1010.3 SDCC-FLP pack (Mode 1 and Mode 2). `buf` is the
    /// pack bytes starting at Element 1 (Matrix Size) — no outer TLV framing and
    /// no leading Universal Label. General-purpose: not ST 0601-specific.
    /// Mirrors tst-py's `decode_sdcc_flp(buf)`.
    fn Java_org_tstrans_klv_Klv_decodeSdccFlpNative<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("decodeSdccFlpNative: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_sdcc_flp(&bytes) {
            Ok(m) => build_sdcc_flp(env, &m).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_sdcc_field_error(env, &e);
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.encodeSdccFlpMode2Native(double[], double[], int) -> byte[]`
    ///
    /// Encodes a Mode-2 SDCC-FLP: standard deviations as IEEE binary32,
    /// correlations as ST 1201 IMAPB(-1, 1, `clen`). Sparse mode + Bit Vector are
    /// chosen automatically when zero-correlations make it pay. Mirrors tst-py's
    /// `encode_sdcc_flp_mode2(std_devs, correlations, clen)`.
    fn Java_org_tstrans_klv_Klv_encodeSdccFlpMode2Native<'local>(
        env,
        _c,
        std_devs: jni::objects::JDoubleArray<'local>,
        correlations: jni::objects::JDoubleArray<'local>,
        clen: jint,
    ) -> jobject = std::ptr::null_mut() => {
        let std_devs_vec = match read_double_array(env, &std_devs) {
            Ok(v) => v,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("encodeSdccFlpMode2Native: stdDevs read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let correlations_vec = match read_double_array(env, &correlations) {
            Ok(v) => v,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("encodeSdccFlpMode2Native: correlations read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match encode_sdcc_flp_mode2(&std_devs_vec, &correlations_vec, clen as usize) {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(e) => {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("encodeSdccFlpMode2Native: byte_array_from_slice failed: {e}"),
                    );
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                map_klv_encode_error(env, e);
                JObject::null().into_raw()
            }
        }
    }
}
