//! JNI surface for ST 0903 VMTI LS + VTargetPack decode/encode.
//!
//! `nDecodeVmti(byte[], boolean strict) -> VmtiLs` — calls
//! `tst_core::klv::st0903::decode` (lenient) or `decode_strict`, then builds
//! the Java `VmtiLs` via its public mutable `Builder`. Each `VTargetPack` in
//! the targets list is built inside its own `with_local_frame` so per-target
//! refs are reclaimed before the next target (bounded live-ref count regardless
//! of list length).
//!
//! `nEncodeVmti(VmtiLs) -> byte[]` — reads fields via accessors, builds the
//! Rust `VmtiLs`, calls `encode_to_vec` (embedded body, no UL, no Tag 1
//! checksum). Mirrors tst-py's `py_to_vmti_ls` / `py_to_vtarget_pack`.
//!
//! `nEncodeVmtiStandalone(VmtiLs) -> byte[]` — same but calls
//! `encode_to_vec_standalone` (full `[UL][BER][body][Tag1 checksum]`).
//!
//! ### Per-target local-frame rule (REQUIRED)
//!
//! Building `n` VTargetPacks in a loop would pin `n * (fields_per_pack)` JNI
//! local refs simultaneously, overflowing the default table for any non-trivial
//! target count. Instead we build each pack inside its own
//! `with_local_frame(64, ...)` so the frame is reclaimed before moving to the
//! next target. The assembled `JObject` from each frame is promoted to the
//! outer frame automatically by jni 0.21's `with_local_frame` return-value
//! promotion. We collect these into the `targets` list held in the outer frame.
//!
//! ### JNI local-ref capacity
//!
//! `build_vmti` calls `ensure_local_capacity(64)` at the top (covers ~16 scalar
//! fields + the builder + lists + JNI scratch for the outer frame).
//! `build_vtarget` needs none of its own — it always runs inside `build_vmti`'s
//! 64-slot per-target frame. On the encode path, `read_vmti` likewise runs each
//! `read_vtarget` inside its own 64-slot `with_local_frame`.

use crate::panic::jni_native;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jobject;
use tst_core::klv::st0903::{
    VTargetPack as RustVTargetPack, VmtiLs as RustVmtiLs, decode as decode_lenient, decode_strict,
    encode_standalone_strict_compliance, encode_strict_compliance, encode_to_vec,
    encode_to_vec_standalone,
};

use crate::error::{map_klv_decode_error, map_klv_encode_error};
use crate::jutil::{
    build_field_errors, build_unknown_list, checked_u8, checked_u16, checked_u32, checked_u64,
    read_nullable_byte_buffer, read_nullable_double, read_nullable_int, read_nullable_long,
    read_nullable_string, read_unknown_list, wrap_heap_byte_buffer,
};

// -----------------------------------------------------------------------
// Class / method-descriptor constants
// -----------------------------------------------------------------------

const VMTI_BUILDER_CLASS: &str = "org/tstrans/klv/VmtiLs$Builder";
const VMTI_BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/VmtiLs$Builder;";
const VMTI_BUILDER_SIG_LONG: &str = "(J)Lorg/tstrans/klv/VmtiLs$Builder;";
const VMTI_BUILDER_SIG_DBL: &str = "(D)Lorg/tstrans/klv/VmtiLs$Builder;";
const VMTI_BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/VmtiLs$Builder;";
const VMTI_BUILDER_SIG_BUF: &str = "(Ljava/nio/ByteBuffer;)Lorg/tstrans/klv/VmtiLs$Builder;";
const VMTI_BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/VmtiLs$Builder;";

const VTGT_BUILDER_CLASS: &str = "org/tstrans/klv/VTargetPack$Builder";
const VTGT_BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/VTargetPack$Builder;";
const VTGT_BUILDER_SIG_LONG: &str = "(J)Lorg/tstrans/klv/VTargetPack$Builder;";
const VTGT_BUILDER_SIG_DBL: &str = "(D)Lorg/tstrans/klv/VTargetPack$Builder;";
const VTGT_BUILDER_SIG_BUF: &str = "(Ljava/nio/ByteBuffer;)Lorg/tstrans/klv/VTargetPack$Builder;";
const VTGT_BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/VTargetPack$Builder;";
const VTGT_BUILDER_SIG_COLOR: &str =
    "(Lorg/tstrans/klv/VTargetPack$TargetColor;)Lorg/tstrans/klv/VTargetPack$Builder;";

/// ST 0903.6 VMTI LS typed tags: 1..=13, 101..=103.
/// Mirrors tst-py's `is_st0903_vmti_typed_tag`.
fn is_st0903_vmti_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=13 | 101..=103)
}

/// ST 0903.6 VTargetPack typed tags: 1..=23, 100..=107.
/// Mirrors tst-py's `is_st0903_vtarget_typed_tag`.
fn is_st0903_vtarget_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=23 | 100..=107)
}

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
                    "java/lang/RuntimeException",
                    format!("nDecodeVmti: byte[] read failed: {e}"),
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
            Ok(vmti) => match build_vmti(env, &vmti) {
                Ok(raw) => raw,
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
        match read_vmti(env, &record) {
            Ok(rust_rec) => match encode_to_vec(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("nEncodeVmti: byte_array_from_slice failed: {e}"),
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
                        format!("nEncodeVmti: field read failed: {e}"),
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
        match read_vmti(env, &record) {
            Ok(rust_rec) => match encode_to_vec_standalone(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!("nEncodeVmtiStandalone: byte_array_from_slice failed: {e}"),
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
                        format!("nEncodeVmtiStandalone: field read failed: {e}"),
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
        match read_vmti(env, &record) {
            Ok(rust_rec) => match encode_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeVmtiStrictCompliance: byte_array_from_slice failed: {e}"
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
                        format!("nEncodeVmtiStrictCompliance: field read failed: {e}"),
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
        match read_vmti(env, &record) {
            Ok(rust_rec) => match encode_standalone_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeVmtiStandaloneStrictCompliance: byte_array_from_slice failed: {e}"
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
                        format!("nEncodeVmtiStandaloneStrictCompliance: field read failed: {e}"),
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

/// Build a `org.tstrans.klv.VmtiLs` Java record from a Rust `VmtiLs`
/// via the public mutable `Builder`. Mirrors `convert_vmti_ls` in tst-py.
///
/// Each `VTargetPack` in the targets list is built AND added to the list
/// inside its own `with_local_frame` so per-target JNI refs are reclaimed
/// before the next iteration — the live local-ref count stays bounded
/// regardless of the target count in the VMTI.
fn build_vmti(env: &mut JNIEnv<'_>, v: &RustVmtiLs) -> jni::errors::Result<jobject> {
    // Reserve enough table slots for the outer frame: ~16 scalars + builder
    // + lists + targets list + scratch.
    env.ensure_local_capacity(64)?;

    let b = env.new_object(VMTI_BUILDER_CLASS, "()V", &[])?;

    // Tag 1 — checksum (u16 → int)
    if let Some(c) = v.checksum {
        env.call_method(
            &b,
            "checksum",
            VMTI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(c))],
        )?;
    }

    // Tag 2 — precisionTimeStamp (u64 → long via reinterpret cast)
    if let Some(t) = v.precision_time_stamp {
        env.call_method(
            &b,
            "precisionTimeStamp",
            VMTI_BUILDER_SIG_LONG,
            &[JValue::Long(t as i64)],
        )?;
    }

    // Tag 3 — vmtiSystemName (UTF-8 String)
    if let Some(ref s) = v.vmti_system_name {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "vmtiSystemName",
            VMTI_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 4 — versionNumber (u16 → int)
    if let Some(n) = v.version_number {
        env.call_method(
            &b,
            "versionNumber",
            VMTI_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(n))],
        )?;
    }

    // Tag 5 — totalTargetsInFrame (u32 → long)
    if let Some(n) = v.total_targets_in_frame {
        env.call_method(
            &b,
            "totalTargetsInFrame",
            VMTI_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }

    // Tag 6 — numTargetsReported (u32 → long)
    if let Some(n) = v.num_targets_reported {
        env.call_method(
            &b,
            "numTargetsReported",
            VMTI_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }

    // Tag 7 deprecated in ST 0903.6 — skipped
    // Tag 8 — frameWidth (u32 → long)
    if let Some(n) = v.frame_width {
        env.call_method(
            &b,
            "frameWidth",
            VMTI_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }

    // Tag 9 — frameHeight (u32 → long)
    if let Some(n) = v.frame_height {
        env.call_method(
            &b,
            "frameHeight",
            VMTI_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(n))],
        )?;
    }

    // Tag 10 — sourceSensor (UTF-8 String)
    if let Some(ref s) = v.source_sensor {
        let j = env.new_string(s)?;
        env.call_method(
            &b,
            "sourceSensor",
            VMTI_BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // Tag 11 — horizontalFov (f64 → double)
    if let Some(f) = v.horizontal_fov {
        env.call_method(
            &b,
            "horizontalFov",
            VMTI_BUILDER_SIG_DBL,
            &[JValue::Double(f)],
        )?;
    }

    // Tag 12 — verticalFov (f64 → double)
    if let Some(f) = v.vertical_fov {
        env.call_method(
            &b,
            "verticalFov",
            VMTI_BUILDER_SIG_DBL,
            &[JValue::Double(f)],
        )?;
    }

    // Tag 13 — miisId (Vec<u8> → heap ByteBuffer)
    if let Some(ref m) = v.miis_id {
        let buf = wrap_heap_byte_buffer(env, m).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(&b, "miisId", VMTI_BUILDER_SIG_BUF, &[JValue::Object(&buf)])?;
    }

    // Tag 101 — VTargetSeries: build targets list.
    // Each VTargetPack is built AND added to the list inside its own local
    // frame so per-target refs are reclaimed before the next iteration — only
    // targets_list (outer frame) survives the loop. This bounds the live
    // local-ref count to O(1) per iteration regardless of target count.
    let targets_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for t in &v.targets {
        // 64 slots covers VTargetPack's ~30 fields + builder + lists + scratch.
        env.with_local_frame(64, |inner_env| {
            let pack_raw = build_vtarget(inner_env, t)?;
            let pack_obj = unsafe { JObject::from_raw(pack_raw) };
            inner_env.call_method(
                &targets_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&pack_obj)],
            )?;
            Ok::<_, jni::errors::Error>(())
        })?;
    }
    env.call_method(
        &b,
        "targets",
        VMTI_BUILDER_SIG_LIST,
        &[JValue::Object(&targets_list)],
    )?;

    // Tag 102 — algorithmSeries (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = v.algorithm_series {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "algorithmSeries",
            VMTI_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 103 — ontologySeries (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = v.ontology_series {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "ontologySeries",
            VMTI_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // fieldErrors — always set (even if empty)
    let fe_list = build_field_errors(env, &v.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        VMTI_BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    // unknown — always set (even if empty)
    let unk_list = build_unknown_list(env, &v.unknown)?;
    env.call_method(
        &b,
        "unknown",
        VMTI_BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    // build() → VmtiLs
    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/VmtiLs;", &[])?
        .l()?;
    Ok(built.into_raw())
}

/// Build a `org.tstrans.klv.VTargetPack` Java record from a Rust `VTargetPack`
/// via the public mutable `Builder`. Mirrors `convert_vtarget_pack` in tst-py.
///
/// Called inside `build_vmti`'s 64-slot `with_local_frame_returning_local`, which
/// already guarantees ample local-ref capacity for this fn's ~30 field refs —
/// so no separate `ensure_local_capacity` is needed here.
fn build_vtarget(env: &mut JNIEnv<'_>, p: &RustVTargetPack) -> jni::errors::Result<jobject> {
    let b = env.new_object(
        VTGT_BUILDER_CLASS,
        "(J)V",
        &[JValue::Long(p.target_id as i64)],
    )?;

    // Tag 1 — centroidPixel (u64 → long)
    if let Some(v) = p.centroid_pixel {
        env.call_method(
            &b,
            "centroidPixel",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // Tag 2 — bboxTopLeftPixel (u64 → long)
    if let Some(v) = p.bbox_top_left_pixel {
        env.call_method(
            &b,
            "bboxTopLeftPixel",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // Tag 3 — bboxBottomRightPixel (u64 → long)
    if let Some(v) = p.bbox_bottom_right_pixel {
        env.call_method(
            &b,
            "bboxBottomRightPixel",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // Tag 4 — priority (u8 → int)
    if let Some(v) = p.priority {
        env.call_method(
            &b,
            "priority",
            VTGT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 5 — confidenceLevel (u8 → int)
    if let Some(v) = p.confidence_level {
        env.call_method(
            &b,
            "confidenceLevel",
            VTGT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 6 — history (u16 → int)
    if let Some(v) = p.history {
        env.call_method(
            &b,
            "history",
            VTGT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 7 — percentageOfTargetPixels (u8 → int)
    if let Some(v) = p.percentage_of_target_pixels {
        env.call_method(
            &b,
            "percentageOfTargetPixels",
            VTGT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 8 — targetColor ([u8;3] → TargetColor record)
    if let Some([r, g, b_val]) = p.target_color {
        let color_obj = env.new_object(
            "org/tstrans/klv/VTargetPack$TargetColor",
            "(III)V",
            &[
                JValue::Int(i32::from(r)),
                JValue::Int(i32::from(g)),
                JValue::Int(i32::from(b_val)),
            ],
        )?;
        env.call_method(
            &b,
            "targetColor",
            VTGT_BUILDER_SIG_COLOR,
            &[JValue::Object(&color_obj)],
        )?;
    }

    // Tag 9 — targetIntensity (u32 → long)
    if let Some(v) = p.target_intensity {
        env.call_method(
            &b,
            "targetIntensity",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(v))],
        )?;
    }

    // Tag 10 — centroidLatOffset (f64 → double)
    if let Some(v) = p.centroid_lat_offset {
        env.call_method(
            &b,
            "centroidLatOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 11 — centroidLonOffset (f64 → double)
    if let Some(v) = p.centroid_lon_offset {
        env.call_method(
            &b,
            "centroidLonOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 12 — centroidHae (f64 → double)
    if let Some(v) = p.centroid_hae {
        env.call_method(
            &b,
            "centroidHae",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 13 — bboxTopLeftLatOffset (f64 → double)
    if let Some(v) = p.bbox_top_left_lat_offset {
        env.call_method(
            &b,
            "bboxTopLeftLatOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 14 — bboxTopLeftLonOffset (f64 → double)
    if let Some(v) = p.bbox_top_left_lon_offset {
        env.call_method(
            &b,
            "bboxTopLeftLonOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 15 — bboxBottomRightLatOffset (f64 → double)
    if let Some(v) = p.bbox_bottom_right_lat_offset {
        env.call_method(
            &b,
            "bboxBottomRightLatOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 16 — bboxBottomRightLonOffset (f64 → double)
    if let Some(v) = p.bbox_bottom_right_lon_offset {
        env.call_method(
            &b,
            "bboxBottomRightLonOffset",
            VTGT_BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // Tag 17 — targetLocation (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.target_location {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "targetLocation",
            VTGT_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 18 — geospatialContourSeries (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.geospatial_contour_series {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "geospatialContourSeries",
            VTGT_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 19 — centroidPixRow (u64 → long)
    if let Some(v) = p.centroid_pix_row {
        env.call_method(
            &b,
            "centroidPixRow",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // Tag 20 — centroidPixCol (u64 → long)
    if let Some(v) = p.centroid_pix_col {
        env.call_method(
            &b,
            "centroidPixCol",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // Tag 21 deprecated in ST 0903.6 — skipped
    // Tag 22 — algorithmId (u32 → long)
    if let Some(v) = p.algorithm_id {
        env.call_method(
            &b,
            "algorithmId",
            VTGT_BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(v))],
        )?;
    }

    // Tag 23 — detectionStatus (u8 → int)
    if let Some(v) = p.detection_status {
        env.call_method(
            &b,
            "detectionStatus",
            VTGT_BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 101 — vmask (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.vmask {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(&b, "vmask", VTGT_BUILDER_SIG_BUF, &[JValue::Object(&buf)])?;
    }

    // Tags 102, 103 deprecated in ST 0903.6 — skipped
    // Tag 104 — vtracker (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.vtracker {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "vtracker",
            VTGT_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 105 — vchip (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.vchip {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(&b, "vchip", VTGT_BUILDER_SIG_BUF, &[JValue::Object(&buf)])?;
    }

    // Tag 106 — vchipSeries (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.vchip_series {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "vchipSeries",
            VTGT_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 107 — vobjectSeries (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = p.vobject_series {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "vobjectSeries",
            VTGT_BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // fieldErrors — always set (even if empty)
    let fe_list = build_field_errors(env, &p.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        VTGT_BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    // unknown — always set (even if empty)
    let unk_list = build_unknown_list(env, &p.unknown)?;
    env.call_method(
        &b,
        "unknown",
        VTGT_BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    // build() → VTargetPack
    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/VTargetPack;", &[])?
        .l()?;
    Ok(built.into_raw())
}

// -----------------------------------------------------------------------
// Java → Rust reader (encode path)
// -----------------------------------------------------------------------

/// Read all fields from a Java `VmtiLs` record into a Rust `VmtiLs`.
/// Mirrors tst-py's `py_to_vmti_ls`.
fn read_vmti(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustVmtiLs> {
    let mut r = RustVmtiLs::default();

    // Tag 1 — checksum (nullable Integer → Option<u16>)
    if let Some(v) = read_nullable_int(env, rec, "checksum")? {
        r.checksum = Some(checked_u16(env, i64::from(v), "checksum")?);
    }

    // Tag 2 — precisionTimeStamp (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "precisionTimeStamp")? {
        r.precision_time_stamp = Some(v as u64);
    }

    // Tag 3 — vmtiSystemName (nullable String)
    r.vmti_system_name = read_nullable_string(env, rec, "vmtiSystemName")?;

    // Tag 4 — versionNumber (nullable Integer → Option<u16>)
    if let Some(v) = read_nullable_int(env, rec, "versionNumber")? {
        r.version_number = Some(checked_u16(env, i64::from(v), "versionNumber")?);
    }

    // Tag 5 — totalTargetsInFrame (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "totalTargetsInFrame")? {
        r.total_targets_in_frame = Some(checked_u32(env, v, "totalTargetsInFrame")?);
    }

    // Tag 6 — numTargetsReported (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "numTargetsReported")? {
        r.num_targets_reported = Some(checked_u32(env, v, "numTargetsReported")?);
    }

    // Tag 7 deprecated in ST 0903.6 — skipped
    // Tag 8 — frameWidth (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "frameWidth")? {
        r.frame_width = Some(checked_u32(env, v, "frameWidth")?);
    }

    // Tag 9 — frameHeight (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "frameHeight")? {
        r.frame_height = Some(checked_u32(env, v, "frameHeight")?);
    }

    // Tag 10 — sourceSensor (nullable String)
    r.source_sensor = read_nullable_string(env, rec, "sourceSensor")?;

    // Tag 11 — horizontalFov (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "horizontalFov")? {
        r.horizontal_fov = Some(v);
    }

    // Tag 12 — verticalFov (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "verticalFov")? {
        r.vertical_fov = Some(v);
    }

    // Tag 13 — miisId (nullable ByteBuffer → Option<Vec<u8>>)
    r.miis_id = read_nullable_byte_buffer(env, rec, "miisId")?;

    // Tag 101 — targets (List<VTargetPack>)
    let targets_obj = env
        .call_method(rec, "targets", "()Ljava/util/List;", &[])?
        .l()?;
    let size = env.call_method(&targets_obj, "size", "()I", &[])?.i()?;
    for i in 0..size {
        // Each iteration mints ~45 JNI local refs (List.get + the ~30 per-field
        // accessor calls in read_vtarget), none freed until this native fn
        // returns. Build each target inside its own frame so per-target refs are
        // reclaimed every iteration — otherwise a many-target encode overflows
        // the JNI local-ref table. Mirrors the decode-side per-target frame in
        // build_vmti. The Rust VTargetPack pushed into `r.targets` is owned Rust
        // data, so nothing JNI-borrowed escapes the frame.
        env.with_local_frame(64, |inner_env| {
            let item = inner_env
                .call_method(
                    &targets_obj,
                    "get",
                    "(I)Ljava/lang/Object;",
                    &[JValue::Int(i)],
                )?
                .l()?;
            r.targets.push(read_vtarget(inner_env, &item)?);
            Ok::<_, jni::errors::Error>(())
        })?;
    }

    // Tag 102 — algorithmSeries (nullable ByteBuffer)
    r.algorithm_series = read_nullable_byte_buffer(env, rec, "algorithmSeries")?;

    // Tag 103 — ontologySeries (nullable ByteBuffer)
    r.ontology_series = read_nullable_byte_buffer(env, rec, "ontologySeries")?;

    // unknown (collision-drop per is_st0903_vmti_typed_tag)
    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    r.unknown = read_unknown_list(env, &unk_obj, is_st0903_vmti_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped.
    Ok(r)
}

/// Read all fields from a Java `VTargetPack` record into a Rust `VTargetPack`.
/// Mirrors tst-py's `py_to_vtarget_pack`.
#[allow(clippy::field_reassign_with_default)]
fn read_vtarget(env: &mut JNIEnv<'_>, rec: &JObject<'_>) -> jni::errors::Result<RustVTargetPack> {
    let mut p = RustVTargetPack::default();

    // BER-OID targetId (primitive long — not nullable)
    {
        let v = env.call_method(rec, "targetId", "()J", &[])?.j()?;
        p.target_id = checked_u64(env, v, "targetId")?;
    }

    // Tag 1 — centroidPixel (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "centroidPixel")? {
        p.centroid_pixel = Some(checked_u64(env, v, "centroidPixel")?);
    }

    // Tag 2 — bboxTopLeftPixel (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "bboxTopLeftPixel")? {
        p.bbox_top_left_pixel = Some(checked_u64(env, v, "bboxTopLeftPixel")?);
    }

    // Tag 3 — bboxBottomRightPixel (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "bboxBottomRightPixel")? {
        p.bbox_bottom_right_pixel = Some(checked_u64(env, v, "bboxBottomRightPixel")?);
    }

    // Tag 4 — priority (nullable Integer → Option<u8>)
    if let Some(v) = read_nullable_int(env, rec, "priority")? {
        p.priority = Some(checked_u8(env, i64::from(v), "priority")?);
    }

    // Tag 5 — confidenceLevel (nullable Integer → Option<u8>)
    if let Some(v) = read_nullable_int(env, rec, "confidenceLevel")? {
        p.confidence_level = Some(checked_u8(env, i64::from(v), "confidenceLevel")?);
    }

    // Tag 6 — history (nullable Integer → Option<u16>)
    if let Some(v) = read_nullable_int(env, rec, "history")? {
        p.history = Some(checked_u16(env, i64::from(v), "history")?);
    }

    // Tag 7 — percentageOfTargetPixels (nullable Integer → Option<u8>)
    if let Some(v) = read_nullable_int(env, rec, "percentageOfTargetPixels")? {
        p.percentage_of_target_pixels =
            Some(checked_u8(env, i64::from(v), "percentageOfTargetPixels")?);
    }

    // Tag 8 — targetColor (nullable TargetColor record → Option<[u8;3]>)
    {
        let tc_obj = env
            .call_method(
                rec,
                "targetColor",
                "()Lorg/tstrans/klv/VTargetPack$TargetColor;",
                &[],
            )?
            .l()?;
        if !tc_obj.is_null() {
            let r_raw = env.call_method(&tc_obj, "r", "()I", &[])?.i()?;
            let g_raw = env.call_method(&tc_obj, "g", "()I", &[])?.i()?;
            let b_raw = env.call_method(&tc_obj, "b", "()I", &[])?.i()?;
            let r = checked_u8(env, i64::from(r_raw), "targetColor.r")?;
            let g = checked_u8(env, i64::from(g_raw), "targetColor.g")?;
            let b_val = checked_u8(env, i64::from(b_raw), "targetColor.b")?;
            p.target_color = Some([r, g, b_val]);
        }
    }

    // Tag 9 — targetIntensity (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "targetIntensity")? {
        p.target_intensity = Some(checked_u32(env, v, "targetIntensity")?);
    }

    // Tag 10 — centroidLatOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "centroidLatOffset")? {
        p.centroid_lat_offset = Some(v);
    }

    // Tag 11 — centroidLonOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "centroidLonOffset")? {
        p.centroid_lon_offset = Some(v);
    }

    // Tag 12 — centroidHae (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "centroidHae")? {
        p.centroid_hae = Some(v);
    }

    // Tag 13 — bboxTopLeftLatOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "bboxTopLeftLatOffset")? {
        p.bbox_top_left_lat_offset = Some(v);
    }

    // Tag 14 — bboxTopLeftLonOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "bboxTopLeftLonOffset")? {
        p.bbox_top_left_lon_offset = Some(v);
    }

    // Tag 15 — bboxBottomRightLatOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "bboxBottomRightLatOffset")? {
        p.bbox_bottom_right_lat_offset = Some(v);
    }

    // Tag 16 — bboxBottomRightLonOffset (nullable Double)
    if let Some(v) = read_nullable_double(env, rec, "bboxBottomRightLonOffset")? {
        p.bbox_bottom_right_lon_offset = Some(v);
    }

    // Tag 17 — targetLocation (nullable ByteBuffer)
    p.target_location = read_nullable_byte_buffer(env, rec, "targetLocation")?;

    // Tag 18 — geospatialContourSeries (nullable ByteBuffer)
    p.geospatial_contour_series = read_nullable_byte_buffer(env, rec, "geospatialContourSeries")?;

    // Tag 19 — centroidPixRow (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "centroidPixRow")? {
        p.centroid_pix_row = Some(checked_u64(env, v, "centroidPixRow")?);
    }

    // Tag 20 — centroidPixCol (nullable Long → Option<u64>)
    if let Some(v) = read_nullable_long(env, rec, "centroidPixCol")? {
        p.centroid_pix_col = Some(checked_u64(env, v, "centroidPixCol")?);
    }

    // Tag 21 deprecated in ST 0903.6 — skipped
    // Tag 22 — algorithmId (nullable Long → Option<u32>)
    if let Some(v) = read_nullable_long(env, rec, "algorithmId")? {
        p.algorithm_id = Some(checked_u32(env, v, "algorithmId")?);
    }

    // Tag 23 — detectionStatus (nullable Integer → Option<u8>)
    if let Some(v) = read_nullable_int(env, rec, "detectionStatus")? {
        p.detection_status = Some(checked_u8(env, i64::from(v), "detectionStatus")?);
    }

    // Tag 101 — vmask (nullable ByteBuffer)
    p.vmask = read_nullable_byte_buffer(env, rec, "vmask")?;

    // Tags 102, 103 deprecated in ST 0903.6 — skipped
    // Tag 104 — vtracker (nullable ByteBuffer)
    p.vtracker = read_nullable_byte_buffer(env, rec, "vtracker")?;

    // Tag 105 — vchip (nullable ByteBuffer)
    p.vchip = read_nullable_byte_buffer(env, rec, "vchip")?;

    // Tag 106 — vchipSeries (nullable ByteBuffer)
    p.vchip_series = read_nullable_byte_buffer(env, rec, "vchipSeries")?;

    // Tag 107 — vobjectSeries (nullable ByteBuffer)
    p.vobject_series = read_nullable_byte_buffer(env, rec, "vobjectSeries")?;

    // unknown (collision-drop per is_st0903_vtarget_typed_tag)
    let unk_obj = env
        .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
        .l()?;
    p.unknown = read_unknown_list(env, &unk_obj, is_st0903_vtarget_typed_tag)?;

    // field_errors is decoder-only diagnostic; not round-tripped.
    Ok(p)
}
