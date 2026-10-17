//! JNI surface for ST 0102 Security Metadata LS decode/encode.
//!
//! `nDecodeSecurity(byte[], boolean strict) -> SecurityLs` calls
//! `tst_core::klv::st0102::decode` (lenient) or `decode_strict` and builds the
//! Java record with the generated `build_security_ls`.
//!
//! `nEncodeSecurity(SecurityLs) -> byte[]` reads the record with the generated
//! `read_security_ls`, calls `encode_to_vec`, and maps any `KlvEncodeError`
//! via `crate::error::map_klv_encode_error`.
//!
//! The converters live in `crate::generated::st0102`; the three coded-enum
//! fields, `unknown` and `field_errors` are converted in `crate::custom`. The
//! generated builder and reader reserve their own JNI local-reference
//! capacity.

use crate::panic::jni_native;
use jni::objects::{JByteArray, JObject};
use jni::sys::jobject;
use tst_core::klv::st0102::{
    decode as decode_lenient, decode_strict, encode_strict_compliance, encode_to_vec,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};

jni_native! {
    /// `org.tstrans.klv.Klv.nDecodeSecurity(byte[], boolean strict) -> SecurityLs`
    ///
    /// Decodes a ST 0102 body (no UL / outer BER wrapper). Uses lenient decode
    /// when `strict = false`, strict decode when `strict = true`. On success,
    /// builds and returns a Java `SecurityLs` record. On failure, throws a
    /// `KlvDecodeException` and returns null.
    fn Java_org_tstrans_klv_Klv_nDecodeSecurity<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
        strict: jni::sys::jboolean,
    ) -> jobject = std::ptr::null_mut() => {
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
            Ok(sec) => match crate::generated::st0102::build_security_ls(env, &sec) {
                Ok(obj) => obj.into_raw(),
                Err(_) => {
                    // A JNI op inside the builder failed (e.g. OOM building a
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
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeSecurity(SecurityLs) -> byte[]`
    ///
    /// Reads all fields from the Java `SecurityLs` record via accessor calls,
    /// builds a Rust `SecurityLs`, calls `encode_to_vec`, and returns the body
    /// bytes (no UL / outer BER wrapper). On error, throws a `KlvEncodeException`
    /// and returns null.
    ///
    /// The 3 enum-typed fields are read as raw `Integer ...Code()` accessors
    /// (nullable boxed int); null → `None` in Rust, non-null → `from_wire`.
    /// Mirrors tst-py's `security_ls_from_py`.
    fn Java_org_tstrans_klv_Klv_nEncodeSecurity<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0102::read_security_ls(env, &record) {
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
                    map_klv_encode_error(env, e);
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
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeSecurityStrictCompliance(SecurityLs) -> byte[]`
    ///
    /// Reads all fields from the Java `SecurityLs` record, builds a Rust `SecurityLs`,
    /// calls `encode_strict_compliance`. Enforces all 6 mandatory-tag presence
    /// (Tags 1, 2, 3, 12, 13, 22) before encoding. Mirrors tst-py's
    /// `encode_security_strict_compliance`.
    fn Java_org_tstrans_klv_Klv_nEncodeSecurityStrictCompliance<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0102::read_security_ls(env, &record) {
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
                    map_klv_encode_error(env, e);
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
    }
}
