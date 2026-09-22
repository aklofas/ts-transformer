//! JNI surface for ST 1204 MIIS Core Identifier decode/encode/text and
//! the ST 0902.8 MISMMS validator.
//!
//! `decodeCoreIdNative(byte[]) -> CoreId` — calls `tst_core::klv::st1204::decode`,
//! maps `St1204Error` to `KlvDecodeException`, and constructs the `CoreId` record.
//!
//! `encodeCoreIdNative(CoreId) -> byte[]` — reads the record fields, builds a Rust
//! `CoreId` via `CoreId::new`, calls `encode_to_vec` (infallible).
//!
//! `coreIdTextNative(CoreId) -> String` — same read path, calls `Display`.
//!
//! `validateMismmsNative(UasDatalinkLs) -> List<MismmsViolation>` — re-uses
//! `read_uas_datalink` from the st0601 module (via a re-export below), calls
//! `validate_mismms`, and marshals the `Vec<MismmsViolation>` into an `ArrayList`
//! of `MismmsViolation` Java records.

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::jobject;
use tst_core::klv::st0601::{MismmsViolation, validate_mismms};
use tst_core::klv::st1204::{CoreId, IdType, St1204Error, decode, encode_to_vec};

use crate::error::throw_klv_decode;
use crate::jutil::require_non_null;
use tst_pipeline::binding::BindingErrorKind;

// ── IdType helpers ────────────────────────────────────────────────────────────

/// Convert a Rust `IdType` to the corresponding Java `IdType` enum ordinal.
/// Physical=0, Virtual=1, Managed=2 (matches declaration order in IdType.java).
fn id_type_ordinal(ty: &IdType) -> i32 {
    match ty {
        IdType::Physical => 0,
        IdType::Virtual => 1,
        IdType::Managed => 2,
        // Non-exhaustive guard: any future variant is unknown — caller treats as error.
        _ => -1,
    }
}

/// Fetch the Java `IdType` enum constant by ordinal (0=PHYSICAL, 1=VIRTUAL, 2=MANAGED).
/// Returns an error if the ordinal is out of range.
fn id_type_from_ordinal<'local>(
    env: &mut JNIEnv<'local>,
    ordinal: i32,
) -> jni::errors::Result<JObject<'local>> {
    let name = match ordinal {
        0 => "PHYSICAL",
        1 => "VIRTUAL",
        2 => "MANAGED",
        _ => {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("unknown IdType ordinal {ordinal}"),
            );
            return Err(jni::errors::Error::JavaException);
        }
    };
    env.get_static_field("org/tstrans/klv/IdType", name, "Lorg/tstrans/klv/IdType;")?
        .l()
}

// ── Map St1204Error → KlvDecodeException ─────────────────────────────────────

fn map_st1204_error(env: &mut JNIEnv, e: &St1204Error) {
    let msg = e.to_string();
    match e {
        St1204Error::Truncated => {
            throw_klv_decode(env, BindingErrorKind::KlvDecodeTruncatedSet, &msg)
        }
        St1204Error::TrailingBytes
        | St1204Error::UnsupportedVersion(_)
        | St1204Error::ReservedBitsSet
        | St1204Error::InvalidUsage => {
            throw_klv_decode(env, BindingErrorKind::KlvDecodeMalformedBytes, &msg)
        }
        _ => throw_klv_decode(env, BindingErrorKind::KlvDecodeMalformedBytes, &msg),
    }
}

// ── Build CoreId Java record from Rust CoreId ─────────────────────────────────

/// Construct a `org.tstrans.klv.CoreId` Java record from a decoded Rust `CoreId`.
///
/// Returns `Err` with a pending JVM exception if any JNI call fails or if an
/// IdType ordinal cannot be mapped.
fn build_core_id(env: &mut JNIEnv<'_>, id: &CoreId) -> jni::errors::Result<jobject> {
    // 16 local refs: version + sensorType + sensorId + platformType + platformId
    //                + windowId + minorId + record + JNI scratch = comfortably fits.
    env.ensure_local_capacity(16)?;

    // sensorType (nullable IdType enum), sensorId (nullable byte[])
    let (sensor_type_obj, sensor_id_arr) = if let Some((ref ty, ref uuid)) = id.sensor {
        let ordinal = id_type_ordinal(ty);
        if ordinal < 0 {
            throw_klv_decode(
                env,
                BindingErrorKind::Internal,
                "Unknown IdType variant from Rust st1204 decoder",
            );
            return Err(jni::errors::Error::JavaException);
        }
        let type_obj = id_type_from_ordinal(env, ordinal)?;
        let arr = env.byte_array_from_slice(uuid.as_ref())?;
        (type_obj, arr.into())
    } else {
        (JObject::null(), JObject::null())
    };

    // platformType (nullable IdType enum), platformId (nullable byte[])
    let (plat_type_obj, plat_id_arr) = if let Some((ref ty, ref uuid)) = id.platform {
        let ordinal = id_type_ordinal(ty);
        if ordinal < 0 {
            throw_klv_decode(
                env,
                BindingErrorKind::Internal,
                "Unknown IdType variant from Rust st1204 decoder",
            );
            return Err(jni::errors::Error::JavaException);
        }
        let type_obj = id_type_from_ordinal(env, ordinal)?;
        let arr = env.byte_array_from_slice(uuid.as_ref())?;
        (type_obj, arr.into())
    } else {
        (JObject::null(), JObject::null())
    };

    // windowId (nullable byte[])
    let window_arr: JObject<'_> = if let Some(ref uuid) = id.window {
        env.byte_array_from_slice(uuid.as_ref())?.into()
    } else {
        JObject::null()
    };

    // minorId (nullable byte[])
    let minor_arr: JObject<'_> = if let Some(ref uuid) = id.minor {
        env.byte_array_from_slice(uuid.as_ref())?.into()
    } else {
        JObject::null()
    };

    // CoreId record ctor:
    // (int version, IdType sensorType, byte[] sensorId,
    //  IdType platformType, byte[] platformId,
    //  byte[] windowId, byte[] minorId)
    let record = env.new_object(
        "org/tstrans/klv/CoreId",
        "(ILorg/tstrans/klv/IdType;[BLorg/tstrans/klv/IdType;[B[B[B)V",
        &[
            JValue::Int(i32::from(id.version)),
            JValue::Object(&sensor_type_obj),
            JValue::Object(&sensor_id_arr),
            JValue::Object(&plat_type_obj),
            JValue::Object(&plat_id_arr),
            JValue::Object(&window_arr),
            JValue::Object(&minor_arr),
        ],
    )?;
    Ok(record.into_raw())
}

// ── Read CoreId Java record into Rust CoreId ──────────────────────────────────

/// Helper: read a nullable `byte[]` field from a Java record via accessor name.
/// Returns `Ok(None)` if the field is null, `Ok(Some(bytes))` otherwise.
fn read_nullable_bytes<'local>(
    env: &mut JNIEnv<'local>,
    obj: &JObject<'local>,
    method: &str,
) -> jni::errors::Result<Option<Vec<u8>>> {
    let val = env.call_method(obj, method, "()[B", &[])?.l()?;
    if val.is_null() {
        return Ok(None);
    }
    let arr: JByteArray<'_> = val.into();
    Ok(Some(env.convert_byte_array(&arr)?))
}

/// Helper: read a nullable `IdType` enum field, returning its Rust equivalent.
/// Throws `KlvDecodeException(MALFORMED_BYTES)` for unknown ordinals.
fn read_nullable_id_type(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
    method: &str,
) -> jni::errors::Result<Option<IdType>> {
    let val = env
        .call_method(obj, method, "()Lorg/tstrans/klv/IdType;", &[])?
        .l()?;
    if val.is_null() {
        return Ok(None);
    }
    let ordinal = env.call_method(&val, "ordinal", "()I", &[])?.i()?;
    let ty = match ordinal {
        0 => IdType::Physical,
        1 => IdType::Virtual,
        2 => IdType::Managed,
        other => {
            throw_klv_decode(
                env,
                BindingErrorKind::KlvDecodeMalformedBytes,
                &format!("unknown IdType ordinal {other} in CoreId record"),
            );
            return Err(jni::errors::Error::JavaException);
        }
    };
    Ok(Some(ty))
}

/// Read a Java `CoreId` record into a Rust `CoreId`.
fn read_core_id<'a>(env: &mut JNIEnv<'a>, obj: &JObject<'a>) -> jni::errors::Result<CoreId> {
    let version = env.call_method(obj, "version", "()I", &[])?.i()?;
    let version = version as u8;

    let sensor_type = read_nullable_id_type(env, obj, "sensorType")?;
    let sensor_id = read_nullable_bytes(env, obj, "sensorId")?;
    let platform_type = read_nullable_id_type(env, obj, "platformType")?;
    let platform_id = read_nullable_bytes(env, obj, "platformId")?;
    let window_id = read_nullable_bytes(env, obj, "windowId")?;
    let minor_id = read_nullable_bytes(env, obj, "minorId")?;

    // Build the sensor pair.
    let sensor = match (sensor_type, sensor_id) {
        (Some(ty), Some(bytes)) => {
            if bytes.len() != 16 {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("CoreId.sensorId must be 16 bytes; got {}", bytes.len()),
                );
                return Err(jni::errors::Error::JavaException);
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&bytes);
            Some((ty, uuid))
        }
        (None, None) => None,
        _ => {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                "CoreId: sensorType and sensorId must both be present or both null",
            );
            return Err(jni::errors::Error::JavaException);
        }
    };

    // Build the platform pair.
    let platform = match (platform_type, platform_id) {
        (Some(ty), Some(bytes)) => {
            if bytes.len() != 16 {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("CoreId.platformId must be 16 bytes; got {}", bytes.len()),
                );
                return Err(jni::errors::Error::JavaException);
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&bytes);
            Some((ty, uuid))
        }
        (None, None) => None,
        _ => {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                "CoreId: platformType and platformId must both be present or both null",
            );
            return Err(jni::errors::Error::JavaException);
        }
    };

    let window = match window_id {
        Some(bytes) => {
            if bytes.len() != 16 {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("CoreId.windowId must be 16 bytes; got {}", bytes.len()),
                );
                return Err(jni::errors::Error::JavaException);
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&bytes);
            Some(uuid)
        }
        None => None,
    };

    let minor = match minor_id {
        Some(bytes) => {
            if bytes.len() != 16 {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("CoreId.minorId must be 16 bytes; got {}", bytes.len()),
                );
                return Err(jni::errors::Error::JavaException);
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&bytes);
            Some(uuid)
        }
        None => None,
    };

    Ok(CoreId::new(version, sensor, platform, window, minor))
}

// ── JNI entry points ─────────────────────────────────────────────────────────

/// `org.tstrans.klv.Klv.decodeCoreIdNative(byte[]) -> CoreId`
///
/// Decodes a MIIS Core Identifier from its binary wire form. On success,
/// constructs and returns a `CoreId` record. On failure, throws a
/// `KlvDecodeException` and returns null.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_decodeCoreIdNative<'local>(
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
                    format!("decodeCoreIdNative: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode(&bytes) {
            Ok(id) => build_core_id(env, &id).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_st1204_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.encodeCoreIdNative(CoreId) -> byte[]`
///
/// Encodes a `CoreId` record to its binary wire form. Infallible on the Rust
/// side; throws `RuntimeException` only on JNI-level failures (field reads or
/// byte-array allocation).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_encodeCoreIdNative<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    id: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Null args must throw NPE, never fall into the reader's silent
        // Err(NullPtr)-with-no-pending-exception path (see require_non_null).
        if require_non_null(env, &id, "id").is_err() {
            return JObject::null().into_raw();
        }
        match read_core_id(env, &id) {
            Ok(rust_id) => {
                let wire = encode_to_vec(&rust_id);
                match env.byte_array_from_slice(&wire) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("encodeCoreIdNative: byte_array_from_slice failed: {e}"),
                        );
                        JObject::null().into_raw()
                    }
                }
            }
            Err(_) => JObject::null().into_raw(),
        }
    })
}

/// `org.tstrans.klv.Klv.coreIdTextNative(CoreId) -> String`
///
/// Returns the ST 1204.3 §7.4.2 textual representation of the given `CoreId`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_coreIdTextNative<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    id: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Same NPE-first contract as encodeCoreIdNative above.
        if require_non_null(env, &id, "id").is_err() {
            return JObject::null().into_raw();
        }
        match read_core_id(env, &id) {
            Ok(rust_id) => {
                let text = alloc::format!("{rust_id}");
                match env.new_string(&text) {
                    Ok(s) => s.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("coreIdTextNative: new_string failed: {e}"),
                        );
                        JObject::null().into_raw()
                    }
                }
            }
            Err(_) => JObject::null().into_raw(),
        }
    })
}

/// `org.tstrans.klv.Klv.validateMismmsNative(UasDatalinkLs) -> List<MismmsViolation>`
///
/// Re-reads the `UasDatalinkLs` Java record into a Rust value (reusing the
/// `read_uas_datalink` helper from st0601), calls `validate_mismms`, then
/// marshals the `Vec<MismmsViolation>` into an `ArrayList<MismmsViolation>`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_validateMismmsNative<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Same NPE-first contract as encodeCoreIdNative above.
        if require_non_null(env, &record, "record").is_err() {
            return JObject::null().into_raw();
        }
        // Re-use the same st0601 reader that the encode entry point uses.
        let rust_rec = match super::st0601::read_uas_datalink_for_validate(env, &record) {
            Ok(r) => r,
            Err(_) => return JObject::null().into_raw(),
        };
        let violations = validate_mismms(&rust_rec);
        match build_violation_list(env, &violations) {
            Ok(list) => list,
            Err(_) => JObject::null().into_raw(),
        }
    })
}

/// Build a `java.util.List<MismmsViolation>` (an `ArrayList`) from a slice of
/// Rust `MismmsViolation`s.
fn build_violation_list(
    env: &mut JNIEnv<'_>,
    violations: &[MismmsViolation],
) -> jni::errors::Result<jobject> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for v in violations {
        // 8 slots: kind_str + tag + name_str + tagB + record + JNI scratch.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let obj = build_violation(env, v)?;
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    Ok(list.into_raw())
}

/// Build a single `org.tstrans.klv.MismmsViolation` record from a Rust variant.
fn build_violation<'local>(
    env: &mut JNIEnv<'local>,
    v: &MismmsViolation,
) -> jni::errors::Result<JObject<'local>> {
    // Map variant to (kind, tag, name, tagB).
    let (kind_str, tag, name_opt, tag_b): (&str, u8, Option<&str>, u8) = match v {
        MismmsViolation::MissingItem { tag, name } => ("missing", *tag, Some(*name), 0),
        MismmsViolation::MissingSecurityItem { tag, name } => {
            ("missing_security", *tag, Some(*name), 0)
        }
        MismmsViolation::ZeroLengthItem { tag } => ("zero_length", *tag, None, 0),
        MismmsViolation::AlternationConflict { tag_a, tag_b } => {
            ("alternation_conflict", *tag_a, None, *tag_b)
        }
        // Non-exhaustive guard: any future variant is unknown — throw rather than fabricate.
        _ => {
            throw_klv_decode(
                env,
                BindingErrorKind::Internal,
                "Unknown MismmsViolation variant from Rust validator crossing the JNI boundary",
            );
            return Err(jni::errors::Error::JavaException);
        }
    };

    let kind_jstr = env.new_string(kind_str)?;
    let name_jstr: JObject<'_> = if let Some(n) = name_opt {
        env.new_string(n)?.into()
    } else {
        JObject::null()
    };
    // MismmsViolation(String kind, int tag, String name, int tagB)
    env.new_object(
        "org/tstrans/klv/MismmsViolation",
        "(Ljava/lang/String;ILjava/lang/String;I)V",
        &[
            JValue::Object(&kind_jstr),
            JValue::Int(i32::from(tag)),
            JValue::Object(&name_jstr),
            JValue::Int(i32::from(tag_b)),
        ],
    )
}

extern crate alloc;
