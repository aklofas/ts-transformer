//! JNI surface for ST 0806.4 RVT (Remote Video Terminal) Local Set
//! decode/encode, plus its two repeatable nested sets (`RvtPoi`/`RvtAoi`)
//! and the User Defined LS (`RvtUserData`).
//!
//! `nDecodeRvt(byte[]) -> RvtLs` calls `tst_core::klv::st0806::decode` (body
//! form: no UL, no outer BER length wrapper) and builds the Java `RvtLs` with
//! the generated `build_rvt_ls`. `nDecodeRvtStandalone(byte[]) -> RvtLs` calls
//! `decode_standalone` (own UL + BER length + CRC-32/MPEG-2 verification).
//!
//! `nEncodeRvt(RvtLs) -> byte[]` and `nEncodeRvtStandalone(RvtLs) -> byte[]`
//! read the record with the generated `read_rvt_ls`, then call
//! `encode_to_vec` / `encode_to_vec_standalone`.
//!
//! The converters live in `crate::generated::st0806`; the POI and AOI type
//! codes, `sentinelTags`, `unknown` and `field_errors` are converted in
//! `crate::custom`. The type codes cross as a raw `Integer` code point
//! (`RvtPoi.poiTypeCode` / `RvtAoi.aoiTypeCode`), never as a Java enum
//! ordinal; the typed `poiType()` / `aoiType()` accessors are pure Java.
//! `RvtUserDataType` never crosses: `RvtUserData.dataType()` and `numericId()`
//! are computed over the single wire byte `numericIdRaw`.
//!
//! The generated code builds and reads every nested item inside its own JNI
//! local frame, so the live reference count stays bounded however long the
//! lists are.

use crate::panic::jni_native;
use jni::jni_str;
use jni::objects::{JByteArray, JObject};
use jni::strings::JNIString;
use jni::sys::jobject;
use tst_core::klv::st0806::{
    decode as decode_rvt, decode_standalone as decode_rvt_standalone, encode_to_vec as encode_rvt,
    encode_to_vec_standalone as encode_rvt_standalone,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};

// -----------------------------------------------------------------------
// Decode entry points
// -----------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.klv.Klv.nDecodeRvt(byte[]) -> RvtLs`
    ///
    /// Decodes an RVT LS body (no UL / outer BER wrapper). On success, builds
    /// and returns a Java `RvtLs`. On failure, throws a `KlvDecodeException`
    /// and returns null.
    fn Java_org_tstrans_klv_Klv_nDecodeRvt<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    jni_str!("java/lang/RuntimeException"),
                    JNIString::from(format!("nDecodeRvt: byte[] read failed: {e}")),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_rvt(&bytes) {
            Ok(ls) => crate::generated::st0806::build_rvt_ls(env, &ls)
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
    /// `org.tstrans.klv.Klv.nDecodeRvtStandalone(byte[]) -> RvtLs`
    ///
    /// Decodes a standalone RVT LS: 16-byte UL + BER length + body, verifying
    /// the CRC-32/MPEG-2 checksum (Tag 1) when present.
    fn Java_org_tstrans_klv_Klv_nDecodeRvtStandalone<'local>(
        env,
        _c,
        buf: JByteArray<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    jni_str!("java/lang/RuntimeException"),
                    JNIString::from(format!("nDecodeRvtStandalone: byte[] read failed: {e}")),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_rvt_standalone(&bytes) {
            Ok(ls) => crate::generated::st0806::build_rvt_ls(env, &ls)
                .map(JObject::into_raw)
                .unwrap_or_else(|_| JObject::null().into_raw()),
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
    /// `org.tstrans.klv.Klv.nEncodeRvt(RvtLs) -> byte[]`
    ///
    /// Reads all fields from the Java `RvtLs` record, builds a Rust `RvtLs`,
    /// calls `encode_to_vec` (embedded body — no UL, no BER length, no Tag 1
    /// CRC). Mirrors tst-py's `encode_rvt`.
    fn Java_org_tstrans_klv_Klv_nEncodeRvt<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0806::read_rvt_ls(env, &record) {
            Ok(rust_rec) => match encode_rvt(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!("nEncodeRvt: byte_array_from_slice failed: {e}")),
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
                        JNIString::from(format!("nEncodeRvt: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `org.tstrans.klv.Klv.nEncodeRvtStandalone(RvtLs) -> byte[]`
    ///
    /// Same read path as `nEncodeRvt`, calls `encode_to_vec_standalone`
    /// instead: `[RVT_LS_UL:16][outer BER length][Tag2 timestamp first][body]
    /// [Tag1 CRC-32/MPEG-2 last]` per ST 0806.4-02/-04. Mirrors tst-py's
    /// `encode_rvt_standalone`.
    fn Java_org_tstrans_klv_Klv_nEncodeRvtStandalone<'local>(
        env,
        _c,
        record: JObject<'local>,
    ) -> jobject = std::ptr::null_mut() => {
        match crate::generated::st0806::read_rvt_ls(env, &record) {
            Ok(rust_rec) => match encode_rvt_standalone(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            jni_str!("java/lang/RuntimeException"),
                            JNIString::from(format!("nEncodeRvtStandalone: byte_array_from_slice failed: {e}")),
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
                        JNIString::from(format!("nEncodeRvtStandalone: field read failed: {e}")),
                    );
                }
                JObject::null().into_raw()
            }
        }
    }
}

// -----------------------------------------------------------------------
// Rust → Java builders (decode path)
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Java → Rust readers (encode path)
// -----------------------------------------------------------------------
