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
//! `CotConfig` is a small Java `record` (5 components) read by the generated
//! `read_cot_config`; a null string component is an
//! `IllegalArgumentException` naming the component, and a negative
//! `updateIntervalUs` is read as its unsigned bit pattern, as `CotConfig`
//! documents.
//!
//! ### JNI local-ref capacity
//!
//! `read_cot_config` reserves its own capacity, and the final `new_string` adds
//! one more reference; `read_uas_datalink_for_validate` has already reserved
//! its own on the caller's behalf (same as `validateMismmsNative`).
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

use crate::jutil::FindClass;
use crate::panic::jni_native;
use jni::Env;
use jni::jni_str;
use jni::objects::JObject;
use jni::strings::JNIString;
use jni::sys::{jlong, jobject};
use tst_core::error::CotError;
use tst_core::klv::st0805::{
    platform_position_xml, platform_uid, sensor_point_of_interest_xml, spi_uid,
};

use crate::jutil::require_non_null;

// ── CotConfig <-> Java record ────────────────────────────────────────────────

// ── Error mapping ─────────────────────────────────────────────────────────────

/// Throw `IllegalArgumentException` with `e`'s `Display` message. See the
/// module doc's "Error mapping" section for why this does not route through
/// `throw_klv_decode`.
fn throw_cot_error(env: &mut Env, e: &CotError) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
        JNIString::from(e.to_string()),
    );
}

/// Allocate a Java `String` from `s`, or throw `RuntimeException` naming
/// `caller` on a JNI-level allocation failure.
fn new_string_or_throw(env: &mut Env<'_>, s: &str, caller: &str) -> jobject {
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(format!("{caller}: new_string failed: {e}")),
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
        let cfg = match crate::generated::st0805::read_cot_config(env, &config) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        // Bit pattern: Java has no unsigned long, so a negative value is a large unsigned time.
        let generated_us = generated_us as u64;
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
        let cfg = match crate::generated::st0805::read_cot_config(env, &config) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        // Bit pattern: Java has no unsigned long, so a negative value is a large unsigned time.
        let generated_us = generated_us as u64;
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
