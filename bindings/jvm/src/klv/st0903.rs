//! JNI surface for ST 0903 VMTI LS + VTargetPack decode/encode.
//!
//! `nDecodeVmti(byte[], boolean strict) -> VmtiLs` calls
//! `tst_core::klv::st0903::decode` (lenient) or `decode_strict` and builds the
//! Java record with the generated `build_vmti_ls`.
//!
//! `nEncodeVmti(VmtiLs) -> byte[]` reads the record with the generated
//! `read_vmti_ls`, calls `encode_to_vec` (embedded body, no UL, no Tag 1
//! checksum). `nEncodeVmtiStandalone(VmtiLs) -> byte[]` calls
//! `encode_to_vec_standalone` (full `[UL][BER][body][Tag1 checksum]`); the
//! two strict-compliance entry points do the same through their strict
//! encoders.
//!
//! The converters live in `crate::generated::st0903`; the target colour,
//! `unknown` and `field_errors` are converted in `crate::custom`. The
//! generated code builds and reads every `VTargetPack` inside its own JNI
//! local frame, so the live reference count stays bounded however many
//! targets a record holds.

use crate::panic::jni_native;
use jni::jni_str;
use jni::objects::{JByteArray, JObject};
use jni::strings::JNIString;
use jni::sys::jobject;
use tst_core::klv::st0903::{
    decode as decode_lenient, decode_strict, encode_standalone_strict_compliance,
    encode_strict_compliance, encode_to_vec, encode_to_vec_standalone,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};

// -----------------------------------------------------------------------
// Decode entry point
// -----------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.klv.Klv.nDecodeVmti(byte[], boolean strict) -> VmtiLs`
    ///
    /// Decodes a ST 0903 body (no UL / outer BER wrapper). Lenient when
    /// `strict = false`; strict when `strict = true`. On success, builds and
    /// returns a Java `VmtiLs` record. On failure, throws a
    /// `KlvDecodeException` and returns null.
    fn Java_org_tstrans_klv_Klv_nDecodeVmti<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
        strict: jni::sys::jboolean,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    jni_str!("java/lang/RuntimeException"),
                    JNIString::from(format!("nDecodeVmti: byte[] read failed: {e}")),
                );
                return JObject::null().into_raw();
            }
        };
        let result = if strict {
            decode_strict(&bytes)
        } else {
            decode_lenient(&bytes)
        };
        match result {
            Ok(vmti) => match crate::generated::st0903::build_vmti_ls(env, &vmti) {
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
    /// `org.tstrans.klv.Klv.nEncodeVmti(VmtiLs) -> byte[]`
    ///
    /// Reads all fields from the Java `VmtiLs` record, builds a Rust `VmtiLs`,
    /// calls `encode_to_vec` (embedded body — no UL, no Tag 1 checksum per
    /// ST 0903.6-120). Mirrors tst-py's `encode_vmti`.
    fn Java_org_tstrans_klv_Klv_nEncodeVmti<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0903::read_vmti_ls(env, &record) {
            Ok(rust_rec) => match encode_to_vec(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!("nEncodeVmti: byte_array_from_slice failed: {e}")),
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
                if !env.exception_check() {
                    let _ = env.throw_new(
                        jni_str!("java/lang/RuntimeException"),
                        JNIString::from(format!("nEncodeVmti: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeVmtiStandalone(VmtiLs) -> byte[]`
    ///
    /// Reads all fields from the Java `VmtiLs` record, builds a Rust `VmtiLs`,
    /// calls `encode_to_vec_standalone` (full `[UL][BER][body][Tag1 checksum]`
    /// per ST 0903.4-17 / ST 0903.6-119). Mirrors tst-py's
    /// `encode_vmti_standalone`.
    fn Java_org_tstrans_klv_Klv_nEncodeVmtiStandalone<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0903::read_vmti_ls(env, &record) {
            Ok(rust_rec) => match encode_to_vec_standalone(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!("nEncodeVmtiStandalone: byte_array_from_slice failed: {e}")),
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
                if !env.exception_check() {
                    let _ = env.throw_new(
                        jni_str!("java/lang/RuntimeException"),
                        JNIString::from(format!("nEncodeVmtiStandalone: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeVmtiStrictCompliance(VmtiLs) -> byte[]`
    ///
    /// Reads all fields from the Java `VmtiLs` record, builds a Rust `VmtiLs`,
    /// calls `encode_strict_compliance` (embedded form). Enforces mandatory items
    /// (Tags 4/6 + non-empty VTargetPacks + unique target IDs) before encoding.
    /// Mirrors tst-py's `encode_vmti_strict_compliance`.
    fn Java_org_tstrans_klv_Klv_nEncodeVmtiStrictCompliance<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0903::read_vmti_ls(env, &record) {
            Ok(rust_rec) => match encode_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!(
                                "nEncodeVmtiStrictCompliance: byte_array_from_slice failed: {e}"
                            )),
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
                if !env.exception_check() {
                    let _ = env.throw_new(
                        jni_str!("java/lang/RuntimeException"),
                        JNIString::from(format!("nEncodeVmtiStrictCompliance: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeVmtiStandaloneStrictCompliance(VmtiLs) -> byte[]`
    ///
    /// Reads all fields from the Java `VmtiLs` record, builds a Rust `VmtiLs`,
    /// calls `encode_standalone_strict_compliance` (full `[UL][BER][body][Tag1 checksum]`
    /// with additional standalone mandatory checks for Tags 2/11/12/13 and
    /// forbidden offset tags). Mirrors tst-py's `encode_vmti_standalone_strict_compliance`.
    fn Java_org_tstrans_klv_Klv_nEncodeVmtiStandaloneStrictCompliance<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0903::read_vmti_ls(env, &record) {
            Ok(rust_rec) => match encode_standalone_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!(
                                "nEncodeVmtiStandaloneStrictCompliance: byte_array_from_slice failed: {e}"
                            )),
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
                if !env.exception_check() {
                    let _ = env.throw_new(
                        jni_str!("java/lang/RuntimeException"),
                        JNIString::from(format!("nEncodeVmtiStandaloneStrictCompliance: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

// -----------------------------------------------------------------------
// Rust → Java builder (decode path)
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Java → Rust reader (encode path)
// -----------------------------------------------------------------------
