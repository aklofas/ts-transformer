//! JNI surface for ST 0605 Precision Time Stamp Pack decode/encode.
//!
//! `nDecodePrecisionTimestamp(byte[]) -> PrecisionTimeStampPack` calls
//! `tst_core::klv::st0605::decode`, builds the Java record with the generated
//! `build_precision_time_stamp_pack`, and maps any `KlvDecodeError` to a thrown
//! `KlvDecodeException` via `crate::error::map_klv_decode_error`.
//!
//! `nEncodePrecisionTimestamp(PrecisionTimeStampPack) -> byte[]` reads the
//! record with the generated `read_precision_time_stamp_pack`, calls
//! `tst_core::klv::st0605::encode` (infallible), and returns the 26 output
//! bytes as a Java `byte[]`. The nested `TimeStatus` is converted in
//! `crate::custom`.

use crate::jutil::FindClass;
use crate::panic::jni_native;
use jni::jni_str;
use jni::objects::{JByteArray, JObject};
use jni::strings::JNIString;
use jni::sys::jobject;
use tst_core::klv::st0605::{decode, encode};

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
                    FindClass(jni_str!("java/lang/RuntimeException")),
                    JNIString::from(format!("nDecodePrecisionTimestamp: byte[] read failed: {e}")),
                );
                return JObject::null().into_raw();
            }
        };
        match decode(&bytes) {
            Ok(p) => crate::generated::st0605::build_precision_time_stamp_pack(env, &p)
                .map(JObject::into_raw)
                .unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodePrecisionTimestamp(PrecisionTimeStampPack) -> byte[]`
    ///
    /// Reads `timeStatus().raw()` and `timestampUs()` from the Java record, then
    /// calls `tst_core::klv::st0605::encode` (infallible, always 26 bytes).
    /// Returns the output as a Java `byte[]`.
    fn Java_org_tstrans_klv_Klv_nEncodePrecisionTimestamp<'local>(
        env,
        _c,
        pack: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let rust_pack = match crate::generated::st0605::read_precision_time_stamp_pack(env, &pack) {
            Ok(p) => p,
            Err(e) => {
                if !env.exception_check() {
                    let _ = env.throw_new(
                        FindClass(jni_str!("java/lang/RuntimeException")),
                        JNIString::from(format!("nEncodePrecisionTimestamp: field read failed: {e}")),
                    );
                }
                return JObject::null().into_raw();
            }
        };
        match env.byte_array_from_slice(&encode(&rust_pack)) {
            Ok(arr) => arr.into_raw(),
            Err(e) => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/RuntimeException")),
                    JNIString::from(format!("nEncodePrecisionTimestamp: byte_array_from_slice failed: {e}")),
                );
                JObject::null().into_raw()
            }
        }
    }
}
