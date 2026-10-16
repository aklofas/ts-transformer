//! JNI surface for MISB ST 0805.1 KLV -> Cursor-on-Target (CoT) conversion.
//!
//! `platformPositionXmlNative(UasDatalinkLs, CotConfig, long) -> String` /
//! `sensorPointOfInterestXmlNative(UasDatalinkLs, CotConfig, long) -> String` /
//! `platformUidNative(UasDatalinkLs) -> String` / `spiUidNative(UasDatalinkLs) -> String`
//! — call `tst_core::klv::st0805::{platform_position_xml,
//! sensor_point_of_interest_xml, platform_uid, spi_uid}`. The `UasDatalinkLs`
//! record is read via the shared `st0601::read_uas_datalink_for_validate`
//! reader — the same one `st1204::validateMismmsNative` uses — so this module
//! needs no `UasDatalinkLs` reader of its own.
//!
//! `CotConfig` is a small Java `record` (5 fields), read field-by-field via
//! its accessor methods (`platformType`/`updateIntervalUs`/`producer`/
//! `geoidUndulationM`/`how`) — no `Builder` round-trip needed, unlike
//! `RvtLs`.
//!
//! ### JNI local-ref capacity
//!
//! No entry point here calls `ensure_local_capacity` directly:
//! `read_uas_datalink_for_validate` already reserves 320 on the caller's
//! behalf (same as `validateMismmsNative`), and `read_cot_config` plus the
//! final `new_string` only add a handful more refs — comfortably inside that
//! margin, mirroring how `validateMismmsNative` piggybacks on the same
//! reservation for its own post-read work.
//!
//! ### Error mapping
//!
//! `CotError::MissingField` maps to `IllegalArgumentException` (its
//! `Display` string as the message) — NOT `KlvDecodeException`: a missing
//! input field on an already-decoded record is an invalid-argument error,
//! not a KLV byte-decode failure. This is the same shape as `st1204.rs`'s
//! `id_type_from_ordinal` unknown-ordinal throw. Mirrors tst-py's
//! `cot_error_to_pyerr` -> `ValueError` mapping (`bindings/python/src/klv.rs`),
//! keeping cross-binding symmetry: both are "caller passed something the
//! value cannot satisfy" errors, not codec/decode failures.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::JObject;
use jni::sys::{jlong, jobject};
use tst_core::error::CotError;
use tst_core::klv::st0805::{
    CotConfig as RustCotConfig, platform_position_xml, platform_uid, sensor_point_of_interest_xml,
    spi_uid,
};

use crate::jutil::{read_nullable_double, read_nullable_string, require_non_null};

// ── CotConfig <-> Java record ────────────────────────────────────────────────

/// Read a mandatory (non-null) `String` accessor, throwing
/// `IllegalArgumentException` naming `field` if the value is null. Sibling of
/// `st0601::read_string` (mandatory) / `jutil::read_nullable_string`
/// (optional) — kept local since `CotConfig` is this module's only
/// mandatory-string caller.
fn read_mandatory_string(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
    field: &str,
) -> jni::errors::Result<String> {
    match read_nullable_string(env, obj, field)? {
        Some(s) => Ok(s),
        None => {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("CotConfig.{field} must not be null"),
            );
            Err(jni::errors::Error::JavaException)
        }
    }
}

/// Read a Java `CotConfig` record into a Rust `CotConfig`.
fn read_cot_config(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<RustCotConfig> {
    let platform_type = read_mandatory_string(env, obj, "platformType")?;
    let update_interval_us_raw = env.call_method(obj, "updateIntervalUs", "()J", &[])?.j()?;
    if update_interval_us_raw < 0 {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!(
                "CotConfig.updateIntervalUs must be non-negative; got {update_interval_us_raw}"
            ),
        );
        return Err(jni::errors::Error::JavaException);
    }
    let update_interval_us = update_interval_us_raw as u64;
    let producer = read_mandatory_string(env, obj, "producer")?;
    let geoid_undulation_m = read_nullable_double(env, obj, "geoidUndulationM")?;
    let how = read_mandatory_string(env, obj, "how")?;
    Ok(RustCotConfig {
        platform_type,
        update_interval_us,
        producer,
        geoid_undulation_m,
        how,
    })
}

/// Validate a `generatedUs` `jlong` is non-negative before the `as u64` cast
/// — a negative Java value would otherwise wrap to a far-future timestamp.
/// Shared by both XML entry points.
fn checked_generated_us(env: &mut JNIEnv<'_>, generated_us: jlong) -> jni::errors::Result<u64> {
    if generated_us < 0 {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("generatedUs must be non-negative; got {generated_us}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(generated_us as u64)
}

// ── Error mapping ─────────────────────────────────────────────────────────────

/// Throw `IllegalArgumentException` with `e`'s `Display` message. See the
/// module doc's "Error mapping" section for why this does not route through
/// `throw_klv_decode`.
fn throw_cot_error(env: &mut JNIEnv, e: &CotError) {
    let _ = env.throw_new("java/lang/IllegalArgumentException", e.to_string());
}

/// Allocate a Java `String` from `s`, or throw `RuntimeException` naming
/// `caller` on a JNI-level allocation failure.
fn new_string_or_throw(env: &mut JNIEnv<'_>, s: &str, caller: &str) -> jobject {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(e) => {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                format!("{caller}: new_string failed: {e}"),
            );
            std::ptr::null_mut()
        }
    }
}

// ── JNI entry points ─────────────────────────────────────────────────────────

jni_native! {
    /// `org.tstrans.klv.Klv.platformPositionXmlNative(UasDatalinkLs, CotConfig, long) -> String`
    fn Java_org_tstrans_klv_Klv_platformPositionXmlNative<'local>(
        env,
        _c,
        record: JObject<'local>,
        config: JObject<'local>,
        generated_us: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        // Null args must throw NPE, never fall into the readers' silent
        // Err(NullPtr)-with-no-pending-exception path (see require_non_null).
        if require_non_null(env, &record, "record").is_err()
            || require_non_null(env, &config, "config").is_err()
        {
            return std::ptr::null_mut();
        }
        let rust_rec = match super::st0601::read_uas_datalink_for_validate(env, &record) {
            Ok(r) => r,
            Err(_) => return std::ptr::null_mut(),
        };
        let cfg = match read_cot_config(env, &config) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        let generated_us = match checked_generated_us(env, generated_us) {
            Ok(v) => v,
            Err(_) => return std::ptr::null_mut(),
        };
        match platform_position_xml(&rust_rec, &cfg, generated_us) {
            Ok(xml) => new_string_or_throw(env, &xml, "platformPositionXmlNative"),
            Err(e) => {
                throw_cot_error(env, &e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.sensorPointOfInterestXmlNative(UasDatalinkLs, CotConfig, long) -> String`
    fn Java_org_tstrans_klv_Klv_sensorPointOfInterestXmlNative<'local>(
        env,
        _c,
        record: JObject<'local>,
        config: JObject<'local>,
        generated_us: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        // Same NPE-first contract as platformPositionXmlNative above.
        if require_non_null(env, &record, "record").is_err()
            || require_non_null(env, &config, "config").is_err()
        {
            return std::ptr::null_mut();
        }
        let rust_rec = match super::st0601::read_uas_datalink_for_validate(env, &record) {
            Ok(r) => r,
            Err(_) => return std::ptr::null_mut(),
        };
        let cfg = match read_cot_config(env, &config) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        let generated_us = match checked_generated_us(env, generated_us) {
            Ok(v) => v,
            Err(_) => return std::ptr::null_mut(),
        };
        match sensor_point_of_interest_xml(&rust_rec, &cfg, generated_us) {
            Ok(xml) => new_string_or_throw(env, &xml, "sensorPointOfInterestXmlNative"),
            Err(e) => {
                throw_cot_error(env, &e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.platformUidNative(UasDatalinkLs) -> String`
    fn Java_org_tstrans_klv_Klv_platformUidNative<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Same NPE-first contract as platformPositionXmlNative above.
        if require_non_null(env, &record, "record").is_err() {
            return std::ptr::null_mut();
        }
        let rust_rec = match super::st0601::read_uas_datalink_for_validate(env, &record) {
            Ok(r) => r,
            Err(_) => return std::ptr::null_mut(),
        };
        match platform_uid(&rust_rec) {
            Ok(uid) => new_string_or_throw(env, &uid, "platformUidNative"),
            Err(e) => {
                throw_cot_error(env, &e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.spiUidNative(UasDatalinkLs) -> String`
    fn Java_org_tstrans_klv_Klv_spiUidNative<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Same NPE-first contract as platformPositionXmlNative above.
        if require_non_null(env, &record, "record").is_err() {
            return std::ptr::null_mut();
        }
        let rust_rec = match super::st0601::read_uas_datalink_for_validate(env, &record) {
            Ok(r) => r,
            Err(_) => return std::ptr::null_mut(),
        };
        match spi_uid(&rust_rec) {
            Ok(uid) => new_string_or_throw(env, &uid, "spiUidNative"),
            Err(e) => {
                throw_cot_error(env, &e);
                std::ptr::null_mut()
            }
        }
    }
}
