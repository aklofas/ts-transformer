//! JNI surface for ST 0605 Precision Time Stamp Pack decode/encode.
//!
//! `nDecodePrecisionTimestamp(byte[]) -> PrecisionTimeStampPack` — calls
//! `tst_core::klv::st0605::decode`, builds the Java record via two
//! `new_object` calls (TimeStatus, then PrecisionTimeStampPack), and maps
//! any `KlvDecodeError` to a thrown `KlvDecodeException` via
//! `crate::error::map_klv_decode_error`.
//!
//! `nEncodePrecisionTimestamp(PrecisionTimeStampPack) -> byte[]` — reads the
//! two record fields via `call_method` accessors, builds the Rust struct,
//! calls `tst_core::klv::st0605::encode` (infallible), and returns the 26
//! output bytes as a Java `byte[]`.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::klv::st0605::{PrecisionTimeStampPack, TimeStatus, decode, encode};

use crate::error::map_klv_decode_error;

jni_native! {
    /// `org.tstrans.klv.Klv.nDecodePrecisionTimestamp(byte[]) -> PrecisionTimeStampPack`
    ///
    /// Decodes a full 26-byte wire-format ST 0605 pack. On success, constructs
    /// and returns a `PrecisionTimeStampPack` record. On failure, throws a
    /// `KlvDecodeException` and returns null.
    fn Java_org_tstrans_klv_Klv_nDecodePrecisionTimestamp<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodePrecisionTimestamp: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode(&bytes) {
            Ok(p) => build_pack(env, &p).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    }
}

/// Build a `org.tstrans.klv.PrecisionTimeStampPack` Java record from a Rust
/// `PrecisionTimeStampPack`. Constructs the nested `TimeStatus(int)` first,
/// then the pack `(TimeStatus, long)`.
fn build_pack(env: &mut JNIEnv<'_>, p: &PrecisionTimeStampPack) -> jni::errors::Result<jobject> {
    // TimeStatus record ctor: (int raw)
    let ts = env.new_object(
        "org/tstrans/klv/TimeStatus",
        "(I)V",
        &[JValue::Int(i32::from(p.time_status.0))],
    )?;
    // PrecisionTimeStampPack record ctor: (TimeStatus timeStatus, long timestampUs)
    let pack = env.new_object(
        "org/tstrans/klv/PrecisionTimeStampPack",
        "(Lorg/tstrans/klv/TimeStatus;J)V",
        &[JValue::Object(&ts), JValue::Long(p.timestamp_us as i64)],
    )?;
    Ok(pack.into_raw())
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodePrecisionTimestamp(PrecisionTimeStampPack) -> byte[]`
    ///
    /// Reads `timeStatus().raw()` (int) and `timestampUs()` (long) from the Java
    /// record via accessor calls, then calls `tst_core::klv::st0605::encode`
    /// (infallible, always 26 bytes). Returns the output as a Java `byte[]`.
    fn Java_org_tstrans_klv_Klv_nEncodePrecisionTimestamp<'local>(
        env,
        _c,
        pack: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        // Read timeStatus() accessor → TimeStatus record
        let ts_obj = match env
            .call_method(&pack, "timeStatus", "()Lorg/tstrans/klv/TimeStatus;", &[])
            .and_then(|v| v.l())
        {
            Ok(o) => o,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nEncodePrecisionTimestamp: timeStatus() call failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        // Read TimeStatus.raw() → int
        let raw = match env
            .call_method(&ts_obj, "raw", "()I", &[])
            .and_then(|v| v.i())
        {
            Ok(r) => r as u8,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nEncodePrecisionTimestamp: TimeStatus.raw() call failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        // Read timestampUs() → long
        let us = match env
            .call_method(&pack, "timestampUs", "()J", &[])
            .and_then(|v| v.j())
        {
            Ok(j) => j as u64,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nEncodePrecisionTimestamp: timestampUs() call failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let wire = encode(&PrecisionTimeStampPack {
            time_status: TimeStatus(raw),
            timestamp_us: us,
        });
        match env.byte_array_from_slice(&wire) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nEncodePrecisionTimestamp: byte_array_from_slice failed: {e}"),
                );
                JObject::null().into_raw()
            }
        }
    }
}
