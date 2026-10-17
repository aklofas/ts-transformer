//! Hand-written converters for fields a generated record marks `@[custom]`.
//!
//! A custom field `<field>` of record `<Record>` is converted by a function in
//! this module named for the record and the field, in snake case. The
//! generator calls it and never writes it. See "Adding a crossing record" in
//! `docs/reference/binding-authors.md`.

mod codec;
pub(crate) use codec::*;

use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use tst_core::error::KlvFieldError;
use tst_core::klv::ImapbSpecial;
use tst_core::klv::UniversalLabel;
use tst_core::klv::pack::OwnedRawField;
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use tst_core::klv::st0601::{
    IcingDetected, OperationalMode, PayloadType, PlatformStatus, SensorControlMode, SensorFovName,
};
use tst_core::klv::st0605::TimeStatus;
use tst_core::klv::st0806::{RvtAoiType, RvtPoiType};

use crate::jutil::{
    build_field_errors, build_long_list, build_unknown_list, checked_u8, read_byte_buffer,
    read_long_list, read_nullable_int, read_unknown_list, wrap_heap_byte_buffer,
};

/// Throws `IllegalArgumentException` and returns the error that carries it.
fn illegal_argument(env: &mut JNIEnv<'_>, msg: String) -> jni::errors::Error {
    let _ = env.throw_new("java/lang/IllegalArgumentException", msg);
    jni::errors::Error::JavaException
}

// ---------------------------------------------------------------------------
// ST 0601 — UasDatalinkLs
// ---------------------------------------------------------------------------

pub(crate) fn build_uas_datalink_ls_universal_label<'l>(
    env: &mut JNIEnv<'l>,
    v: &UniversalLabel,
) -> jni::errors::Result<JObject<'l>> {
    wrap_heap_byte_buffer(env, &v.0).map_err(|()| jni::errors::Error::JavaException)
}

/// Reads through `read_byte_buffer`, which honours position and limit and
/// supports direct buffers.
pub(crate) fn read_uas_datalink_ls_universal_label(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<UniversalLabel> {
    let buf = env
        .call_method(obj, "universalLabel", "()Ljava/nio/ByteBuffer;", &[])?
        .l()?;
    let bytes = read_byte_buffer(env, &buf)?;
    let ul: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
        illegal_argument(
            env,
            format!("universalLabel must be 16 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(UniversalLabel(ul))
}

/// A byte string the Java record carries as `byte[]`.
pub(crate) fn build_uas_datalink_ls_miis_core_id<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<Vec<u8>>,
) -> jni::errors::Result<JObject<'l>> {
    Ok(match v {
        Some(b) => env.byte_array_from_slice(b)?.into(),
        None => JObject::null(),
    })
}

pub(crate) fn read_uas_datalink_ls_miis_core_id(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<Vec<u8>>> {
    let arr = env.call_method(obj, "miisCoreId", "()[B", &[])?.l()?;
    if arr.is_null() {
        return Ok(None);
    }
    let arr: JByteArray<'_> = arr.into();
    Ok(Some(env.convert_byte_array(&arr)?))
}

/// A coded enum crosses as its raw code point in a boxed `Integer`, `null`
/// when absent. A wire-unknown `Other(code)` keeps its code.
fn boxed_code<'l>(env: &mut JNIEnv<'l>, code: Option<u8>) -> jni::errors::Result<JObject<'l>> {
    match code {
        Some(c) => env
            .call_static_method(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[JValue::Int(i32::from(c))],
            )?
            .l(),
        None => Ok(JObject::null()),
    }
}

/// The code point of a coded-enum accessor `name`, range-checked to a byte.
fn read_code(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
    name: &str,
) -> jni::errors::Result<Option<u8>> {
    match read_nullable_int(env, obj, name)? {
        Some(v) => Ok(Some(checked_u8(env, i64::from(v), name)?)),
        None => Ok(None),
    }
}

pub(crate) fn build_uas_datalink_ls_icing_detected<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<IcingDetected>,
) -> jni::errors::Result<JObject<'l>> {
    boxed_code(env, v.map(|v| v.to_wire()))
}

pub(crate) fn read_uas_datalink_ls_icing_detected(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<IcingDetected>> {
    Ok(read_code(env, obj, "icingDetectedCode")?.map(IcingDetected::from_wire))
}

pub(crate) fn build_uas_datalink_ls_sensor_fov_name<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<SensorFovName>,
) -> jni::errors::Result<JObject<'l>> {
    boxed_code(env, v.map(|v| v.to_wire()))
}

pub(crate) fn read_uas_datalink_ls_sensor_fov_name(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<SensorFovName>> {
    Ok(read_code(env, obj, "sensorFovNameCode")?.map(SensorFovName::from_wire))
}

pub(crate) fn build_uas_datalink_ls_operational_mode<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<OperationalMode>,
) -> jni::errors::Result<JObject<'l>> {
    boxed_code(env, v.map(|v| v.to_wire()))
}

pub(crate) fn read_uas_datalink_ls_operational_mode(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<OperationalMode>> {
    Ok(read_code(env, obj, "operationalModeCode")?.map(OperationalMode::from_wire))
}

pub(crate) fn build_uas_datalink_ls_platform_status<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<PlatformStatus>,
) -> jni::errors::Result<JObject<'l>> {
    boxed_code(env, v.map(|v| v.to_wire()))
}

pub(crate) fn read_uas_datalink_ls_platform_status(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<PlatformStatus>> {
    Ok(read_code(env, obj, "platformStatusCode")?.map(PlatformStatus::from_wire))
}

pub(crate) fn build_uas_datalink_ls_sensor_control_mode<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<SensorControlMode>,
) -> jni::errors::Result<JObject<'l>> {
    boxed_code(env, v.map(|v| v.to_wire()))
}

pub(crate) fn read_uas_datalink_ls_sensor_control_mode(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<SensorControlMode>> {
    Ok(read_code(env, obj, "sensorControlModeCode")?.map(SensorControlMode::from_wire))
}

pub(crate) fn build_uas_datalink_ls_sentinel_tags<'l>(
    env: &mut JNIEnv<'l>,
    v: &[u32],
) -> jni::errors::Result<JObject<'l>> {
    build_long_list(env, v.iter().map(|&t| i64::from(t)))
}

pub(crate) fn read_uas_datalink_ls_sentinel_tags(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Vec<u32>> {
    read_sentinel_tags(env, obj, "UasDatalinkLs")
}

/// `Vec<(u32, ImapbSpecial)>` crosses as a `List<ImapbSpecialEntry>` of
/// `(tag, code, payload)`. `code` names the `ImapbSpecial` family; `payload`
/// is the NaN id or signal (0 for the codes that carry none).
pub(crate) fn build_uas_datalink_ls_imapb_specials<'l>(
    env: &mut JNIEnv<'l>,
    v: &[(u32, ImapbSpecial)],
) -> jni::errors::Result<JObject<'l>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for &(tag, special) in v {
        let (code, payload) = imapb_special_to_code(env, special)?;
        // 8 slots: the code string, the entry and JNI scratch per entry.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let code_str = env.new_string(code)?;
            let entry = env.new_object(
                "org/tstrans/klv/ImapbSpecialEntry",
                "(ILjava/lang/String;J)V",
                &[
                    JValue::Int(tag as i32),
                    JValue::Object(&code_str),
                    JValue::Long(payload as i64),
                ],
            )?;
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&entry)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

pub(crate) fn read_uas_datalink_ls_imapb_specials(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Vec<(u32, ImapbSpecial)>> {
    let list = env
        .call_method(obj, "imapbSpecials", "()Ljava/util/List;", &[])?
        .l()?;
    let size = env.call_method(&list, "size", "()I", &[])?.i()?;
    let mut out = Vec::with_capacity(size.max(0) as usize);
    for i in 0..size {
        let entry = env.with_local_frame(8, |env| -> jni::errors::Result<(u32, ImapbSpecial)> {
            let item = env
                .call_method(&list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            let tag_i = env.call_method(&item, "tag", "()I", &[])?.i()?;
            let tag = u32::try_from(tag_i).map_err(|_| {
                illegal_argument(
                    env,
                    format!("imapbSpecials entry tag out of u32 range: {tag_i}"),
                )
            })?;
            let code_obj = env
                .call_method(&item, "code", "()Ljava/lang/String;", &[])?
                .l()?;
            let j_str: &jni::objects::JString = (&code_obj).into();
            let code: String = env.get_string(j_str).map(Into::into)?;
            let payload = env.call_method(&item, "payload", "()J", &[])?.j()?;
            Ok((tag, imapb_special_from_code(env, &code, payload as u64)?))
        })?;
        out.push(entry);
    }
    Ok(out)
}

/// Throws `IllegalArgumentException` rather than mislabelling a variant added
/// later: `ImapbSpecial` has no tst-core wire-code helper, so this table must
/// fail loudly when it goes stale.
fn imapb_special_to_code(
    env: &mut JNIEnv<'_>,
    s: ImapbSpecial,
) -> jni::errors::Result<(&'static str, u64)> {
    Ok(match s {
        ImapbSpecial::BelowMin => ("below_min", 0),
        ImapbSpecial::AboveMax => ("above_max", 0),
        ImapbSpecial::PositiveInfinity => ("pos_infinity", 0),
        ImapbSpecial::NegativeInfinity => ("neg_infinity", 0),
        ImapbSpecial::PositiveQuietNaN { nan_id } => ("pos_quiet_nan", nan_id),
        ImapbSpecial::NegativeQuietNaN { nan_id } => ("neg_quiet_nan", nan_id),
        ImapbSpecial::PositiveSignalingNaN { signal } => ("pos_signaling_nan", signal),
        ImapbSpecial::NegativeSignalingNaN { signal } => ("neg_signaling_nan", signal),
        ImapbSpecial::UserDefined { signal } => ("user_defined", signal),
        // `#[non_exhaustive]` in tst-core: no current variant reaches here.
        _ => {
            return Err(illegal_argument(
                env,
                "unknown ImapbSpecial variant crossing the binding".to_string(),
            ));
        }
    })
}

/// A code outside the nine known ones throws `IllegalArgumentException`:
/// validate, don't drop.
fn imapb_special_from_code(
    env: &mut JNIEnv<'_>,
    code: &str,
    payload: u64,
) -> jni::errors::Result<ImapbSpecial> {
    Ok(match code {
        "below_min" => ImapbSpecial::BelowMin,
        "above_max" => ImapbSpecial::AboveMax,
        "pos_infinity" => ImapbSpecial::PositiveInfinity,
        "neg_infinity" => ImapbSpecial::NegativeInfinity,
        "pos_quiet_nan" => ImapbSpecial::PositiveQuietNaN { nan_id: payload },
        "neg_quiet_nan" => ImapbSpecial::NegativeQuietNaN { nan_id: payload },
        "pos_signaling_nan" => ImapbSpecial::PositiveSignalingNaN { signal: payload },
        "neg_signaling_nan" => ImapbSpecial::NegativeSignalingNaN { signal: payload },
        "user_defined" => ImapbSpecial::UserDefined { signal: payload },
        other => {
            return Err(illegal_argument(
                env,
                format!(
                    "unknown imapbSpecials code {other:?}; expected one of below_min, above_max, \
                     pos_infinity, neg_infinity, pos_quiet_nan, neg_quiet_nan, pos_signaling_nan, \
                     neg_signaling_nan, user_defined"
                ),
            ));
        }
    })
}

/// ST 0601 LS typed + reserved tags — mirrors `tags::TAGS` in
/// `crates/tst-core/src/klv/st0601/tags.rs` (142 entries: 1-65, 67-143). Tag 66
/// is the deprecated placeholder (permanently untyped by design — ST 0601.19
/// §8.66: "This item has been Deprecated") and 144..=255 are forward-compat;
/// both may legitimately appear in `unknown` (66 and 200 are the durable
/// unknown-tag test stand-ins used across this suite — never add them here).
/// Keep this in sync with `tags::TAGS` when new tags are typed, or a
/// caller-supplied `unknown` entry for a newly-typed tag slips past this
/// filter and is rejected downstream by the Rust encoder's own check instead
/// of being dropped per the "typed wins" policy. Mirrors tst-py's
/// `is_st0601_typed_tag`.
fn is_st0601_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=65 | 67..=143)
}

// ---------------------------------------------------------------------------
// ST 0601 — pack records
// ---------------------------------------------------------------------------

pub(crate) fn build_metadata_substream_id_uuid<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<[u8; 16]>,
) -> jni::errors::Result<JObject<'l>> {
    Ok(match v {
        Some(u) => env.byte_array_from_slice(u)?.into(),
        None => JObject::null(),
    })
}

pub(crate) fn read_metadata_substream_id_uuid(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<[u8; 16]>> {
    let arr = env.call_method(obj, "uuid", "()[B", &[])?.l()?;
    if arr.is_null() {
        return Ok(None);
    }
    let arr: JByteArray<'_> = arr.into();
    let bytes = env.convert_byte_array(&arr)?;
    let uuid: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
        illegal_argument(
            env,
            format!(
                "MetadataSubstreamId.uuid must be 16 bytes; got {}",
                bytes.len()
            ),
        )
    })?;
    Ok(Some(uuid))
}

/// The code point is a BER-OID, so it crosses as a full-range Java `long`.
pub(crate) fn build_payload_record_payload_type(
    _env: &mut JNIEnv<'_>,
    v: &PayloadType,
) -> jni::errors::Result<i64> {
    Ok(v.to_wire() as i64)
}

pub(crate) fn read_payload_record_payload_type(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<PayloadType> {
    let code = env.call_method(obj, "payloadTypeCode", "()J", &[])?.j()?;
    // Unsigned 64-bit values cross as the bit pattern of a `long`.
    Ok(PayloadType::from_wire(code as u64))
}

/// ST 0102 typed tags: 1..=14, 22, 23 and 24. An `unknown` entry whose tag a
/// typed field owns is dropped: typed wins.
fn is_st0102_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=14 | 22 | 23 | 24)
}

/// ST 0903.6 VMTI typed tags: 1..=13 and 101..=103.
fn is_st0903_vmti_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=13 | 101..=103)
}

/// ST 0903.6 VTargetPack typed tags: 1..=23 and 100..=107.
fn is_st0903_vtarget_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=23 | 100..=107)
}

/// ST 0806.4 RVT typed tags: 1..=21, no gaps.
fn is_st0806_rvt_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=21)
}

/// ST 0806.4 POI and AOI typed tags: both sets share 1..=10.
fn is_st0806_poi_aoi_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=10)
}

/// The pass-through fields every typed set carries, per record: `unknown`
/// crosses both ways (a typed tag is dropped), `field_errors` is a parser
/// diagnostic that does not cross back.
macro_rules! unknown_and_field_errors {
    ($build_unknown:ident, $read_unknown:ident, $build_errors:ident, $read_errors:ident, $typed:ident) => {
        pub(crate) fn $build_unknown<'l>(
            env: &mut JNIEnv<'l>,
            v: &[OwnedRawField],
        ) -> jni::errors::Result<JObject<'l>> {
            build_unknown_list(env, v)
        }

        pub(crate) fn $read_unknown(
            env: &mut JNIEnv<'_>,
            obj: &JObject<'_>,
        ) -> jni::errors::Result<Vec<OwnedRawField>> {
            let list = env
                .call_method(obj, "unknown", "()Ljava/util/List;", &[])?
                .l()?;
            read_unknown_list(env, &list, $typed)
        }

        pub(crate) fn $build_errors<'l>(
            env: &mut JNIEnv<'l>,
            v: &[KlvFieldError],
        ) -> jni::errors::Result<JObject<'l>> {
            build_field_errors(env, v)
        }

        /// A parser diagnostic: it does not cross back.
        pub(crate) fn $read_errors(
            _env: &mut JNIEnv<'_>,
            _obj: &JObject<'_>,
        ) -> jni::errors::Result<Vec<KlvFieldError>> {
            Ok(Vec::new())
        }
    };
}

unknown_and_field_errors!(
    build_uas_datalink_ls_unknown,
    read_uas_datalink_ls_unknown,
    build_uas_datalink_ls_field_errors,
    read_uas_datalink_ls_field_errors,
    is_st0601_typed_tag
);

/// A coded-enum field: a boxed `Integer` code point, `null` when absent; the
/// wire-unknown variant keeps its code. `$accessor` is the record component.
macro_rules! coded_enum_fields {
    ($build:ident, $read:ident, $ty:ident, $accessor:literal) => {
        pub(crate) fn $build<'l>(
            env: &mut JNIEnv<'l>,
            v: &Option<$ty>,
        ) -> jni::errors::Result<JObject<'l>> {
            boxed_code(env, v.map(|v| v.to_wire()))
        }

        pub(crate) fn $read(
            env: &mut JNIEnv<'_>,
            obj: &JObject<'_>,
        ) -> jni::errors::Result<Option<$ty>> {
            Ok(read_code(env, obj, $accessor)?.map($ty::from_wire))
        }
    };
}

/// The `sentinelTags` list of a nested ST 0806 set, each entry a `u32`.
fn read_sentinel_tags(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
    record: &str,
) -> jni::errors::Result<Vec<u32>> {
    let list = env
        .call_method(obj, "sentinelTags", "()Ljava/util/List;", &[])?
        .l()?;
    let mut out = Vec::new();
    for v in read_long_list(env, &list)? {
        let tag = u32::try_from(v).map_err(|_| {
            illegal_argument(
                env,
                format!("{record}.sentinelTags entry out of u32 range: {v}"),
            )
        })?;
        out.push(tag);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ST 0102 — SecurityLs
// ---------------------------------------------------------------------------

coded_enum_fields!(
    build_security_ls_security_classification,
    read_security_ls_security_classification,
    SecurityClassification,
    "securityClassificationCode"
);
coded_enum_fields!(
    build_security_ls_classifying_country_coding_method,
    read_security_ls_classifying_country_coding_method,
    ClassifyingCountryCodingMethod,
    "classifyingCountryCodingMethodCode"
);
coded_enum_fields!(
    build_security_ls_object_country_coding_method,
    read_security_ls_object_country_coding_method,
    ObjectCountryCodingMethod,
    "objectCountryCodingMethodCode"
);
unknown_and_field_errors!(
    build_security_ls_unknown,
    read_security_ls_unknown,
    build_security_ls_field_errors,
    read_security_ls_field_errors,
    is_st0102_typed_tag
);

// ---------------------------------------------------------------------------
// ST 0605 — PrecisionTimeStampPack
// ---------------------------------------------------------------------------

pub(crate) fn build_precision_time_stamp_pack_time_status<'l>(
    env: &mut JNIEnv<'l>,
    v: &TimeStatus,
) -> jni::errors::Result<JObject<'l>> {
    env.new_object(
        "org/tstrans/klv/TimeStatus",
        "(I)V",
        &[JValue::Int(i32::from(v.0))],
    )
}

pub(crate) fn read_precision_time_stamp_pack_time_status(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<TimeStatus> {
    let status = env
        .call_method(obj, "timeStatus", "()Lorg/tstrans/klv/TimeStatus;", &[])?
        .l()?;
    let raw = env.call_method(&status, "raw", "()I", &[])?.i()?;
    Ok(TimeStatus(checked_u8(
        env,
        i64::from(raw),
        "timeStatus.raw",
    )?))
}

// ---------------------------------------------------------------------------
// ST 0903 — VTargetPack, VmtiLs
// ---------------------------------------------------------------------------

/// The 3-byte colour crosses as the nested `VTargetPack.TargetColor` record.
pub(crate) fn build_v_target_pack_target_color<'l>(
    env: &mut JNIEnv<'l>,
    v: &Option<[u8; 3]>,
) -> jni::errors::Result<JObject<'l>> {
    match v {
        Some([r, g, b]) => env.new_object(
            "org/tstrans/klv/VTargetPack$TargetColor",
            "(III)V",
            &[
                JValue::Int(i32::from(*r)),
                JValue::Int(i32::from(*g)),
                JValue::Int(i32::from(*b)),
            ],
        ),
        None => Ok(JObject::null()),
    }
}

pub(crate) fn read_v_target_pack_target_color(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Option<[u8; 3]>> {
    let color = env
        .call_method(
            obj,
            "targetColor",
            "()Lorg/tstrans/klv/VTargetPack$TargetColor;",
            &[],
        )?
        .l()?;
    if color.is_null() {
        return Ok(None);
    }
    let mut rgb = [0u8; 3];
    for (slot, (name, label)) in rgb.iter_mut().zip([
        ("r", "targetColor.r"),
        ("g", "targetColor.g"),
        ("b", "targetColor.b"),
    ]) {
        let raw = env.call_method(&color, name, "()I", &[])?.i()?;
        *slot = checked_u8(env, i64::from(raw), label)?;
    }
    Ok(Some(rgb))
}

unknown_and_field_errors!(
    build_v_target_pack_unknown,
    read_v_target_pack_unknown,
    build_v_target_pack_field_errors,
    read_v_target_pack_field_errors,
    is_st0903_vtarget_typed_tag
);
unknown_and_field_errors!(
    build_vmti_ls_unknown,
    read_vmti_ls_unknown,
    build_vmti_ls_field_errors,
    read_vmti_ls_field_errors,
    is_st0903_vmti_typed_tag
);

// ---------------------------------------------------------------------------
// ST 0806 — RvtPoi, RvtAoi, RvtLs
// ---------------------------------------------------------------------------

coded_enum_fields!(
    build_rvt_poi_poi_type,
    read_rvt_poi_poi_type,
    RvtPoiType,
    "poiTypeCode"
);
coded_enum_fields!(
    build_rvt_aoi_aoi_type,
    read_rvt_aoi_aoi_type,
    RvtAoiType,
    "aoiTypeCode"
);

pub(crate) fn build_rvt_poi_sentinel_tags<'l>(
    env: &mut JNIEnv<'l>,
    v: &[u32],
) -> jni::errors::Result<JObject<'l>> {
    build_long_list(env, v.iter().map(|&t| i64::from(t)))
}

pub(crate) fn read_rvt_poi_sentinel_tags(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Vec<u32>> {
    read_sentinel_tags(env, obj, "RvtPoi")
}

pub(crate) fn build_rvt_aoi_sentinel_tags<'l>(
    env: &mut JNIEnv<'l>,
    v: &[u32],
) -> jni::errors::Result<JObject<'l>> {
    build_long_list(env, v.iter().map(|&t| i64::from(t)))
}

pub(crate) fn read_rvt_aoi_sentinel_tags(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<Vec<u32>> {
    read_sentinel_tags(env, obj, "RvtAoi")
}

unknown_and_field_errors!(
    build_rvt_poi_unknown,
    read_rvt_poi_unknown,
    build_rvt_poi_field_errors,
    read_rvt_poi_field_errors,
    is_st0806_poi_aoi_typed_tag
);
unknown_and_field_errors!(
    build_rvt_aoi_unknown,
    read_rvt_aoi_unknown,
    build_rvt_aoi_field_errors,
    read_rvt_aoi_field_errors,
    is_st0806_poi_aoi_typed_tag
);
unknown_and_field_errors!(
    build_rvt_ls_unknown,
    read_rvt_ls_unknown,
    build_rvt_ls_field_errors,
    read_rvt_ls_field_errors,
    is_st0806_rvt_typed_tag
);

// ---------------------------------------------------------------------------
// ST 1010 — SdccFlp
// ---------------------------------------------------------------------------

/// The matrices cross as primitive arrays.
pub(crate) fn build_sdcc_flp_std_devs<'l>(
    env: &mut JNIEnv<'l>,
    v: &[f64],
) -> jni::errors::Result<JObject<'l>> {
    build_double_array(env, v)
}

pub(crate) fn build_sdcc_flp_correlations<'l>(
    env: &mut JNIEnv<'l>,
    v: &[f64],
) -> jni::errors::Result<JObject<'l>> {
    build_double_array(env, v)
}

pub(crate) fn build_sdcc_flp_correlation_present<'l>(
    env: &mut JNIEnv<'l>,
    v: &[bool],
) -> jni::errors::Result<JObject<'l>> {
    let flags: Vec<u8> = v.iter().map(|&b| u8::from(b)).collect();
    let arr = env.new_boolean_array(flags.len() as i32)?;
    env.set_boolean_array_region(&arr, 0, &flags)?;
    Ok(arr.into())
}

fn build_double_array<'l>(env: &mut JNIEnv<'l>, v: &[f64]) -> jni::errors::Result<JObject<'l>> {
    let arr = env.new_double_array(v.len() as i32)?;
    env.set_double_array_region(&arr, 0, v)?;
    Ok(arr.into())
}
