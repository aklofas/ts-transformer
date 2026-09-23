//! JNI surface for ST 0601 UAS Datalink LS decode/encode.
//!
//! `nDecodeUasDatalink(byte[], boolean strict, boolean compliance) -> UasDatalinkLs` —
//! dispatches: `compliance=true` → `decode_strict_compliance`; else `strict=true` →
//! `decode_strict`; else `decode` (lenient). Builds the Java `UasDatalinkLs` via its
//! public mutable `Builder` (the Builder-marshalling pattern from Tasks 2–3).
//!
//! `nEncodeUasDatalinkWithPolicy(UasDatalinkLs, int policy) -> byte[]` — reads all
//! fields via accessor `call_method`s, builds a Rust `UasDatalinkLs`, calls
//! `encode_to_vec_with`. The Java 1-arg `encodeUasDatalink(record)` delegates here
//! with `policy=0` (ERROR). The 2-arg overload passes `policy=1` for INDICATOR.
//! Mirrors tst-py's `py_to_uas_datalink_ls` including 16-byte UL validation and
//! the `is_st0601_typed_tag` collision-drop.
//!
//! `nEncodeUasDatalinkStrictCompliance(UasDatalinkLs) -> byte[]` — same read path,
//! calls `encode_strict_compliance` instead.
//!
//! ### JNI local-ref capacity (CRITICAL for 147-field set)
//!
//! `build_uas_datalink` and `read_uas_datalink` both call
//! `env.ensure_local_capacity(320)` at the top. Bumped from 256 (WP-C's
//! first cut — its sizing arithmetic was self-contradictory and left
//! effectively zero margin, caught in review) to leave REAL headroom, not a
//! tight fit. With 12 String fields + ~110 Double/Long/Integer/ByteBuffer
//! fields (WP-A's 51 new fields pushed the total from 56 to 107; WP-B's 25
//! new fields + the `imapbSpecials` list pushed it to 133), the pre-WP-C
//! baseline already needed all 224 of its slots — there was little slack
//! even before WP-C's 14 new fields. Honest per-field tally of WP-C's OWN
//! additional outer-frame refs (excludes anything reclaimed inside a
//! per-item `with_local_frame`, per the table below):
//!
//! | Field | Outer-frame refs (build ≈ read) |
//! |---|---|
//! | `imageHorizon` | 5 worst-case on read (1 outer + 4 boxed `Double` getters); only 2 on build — the `Builder`'s primitive-`double` setters box nothing |
//! | `controlCommands` (list) | 1 (outer `ArrayList`; items reclaimed) |
//! | `controlCommandVerification` | 1 (outer `ArrayList` via `build_long_list`/`read_long_list`; items reclaimed) |
//! | `activeWavelengths` | 1 (same shape as `controlCommandVerification`) |
//! | `sensorFrameRate` | 1 (record only — both fields are primitives, no boxing) |
//! | `metadataSubstreamId` | 2 (`byte[]` uuid + record) |
//! | `countryCodes` | 4 (3 `String` + record) |
//! | `wavelengthsList` (list) | 1 (outer `ArrayList`; items reclaimed) |
//! | `airbaseLocations` | 9 (2 × nested `Location` @ up to 4 refs each + record) |
//! | `payloadList` | 2 (records `ArrayList` + record; items reclaimed) |
//! | `weaponsStores` (list) | 1 (outer `ArrayList`; items reclaimed) |
//! | `waypointList` (list) | 1 (outer `ArrayList`; items reclaimed) |
//! | `viewDomain` | 4 (3 × nested `ViewDomainPair` + record) |
//! | `sdccFlps` (list) | 1 (outer `ArrayList`; items reclaimed) |
//!
//! Sum: ~34 additional outer-frame refs at worst case. Of the 14 rows, 7 are
//! `ArrayList`-shaped (5 holding nested records — `controlCommands`,
//! `wavelengthsList`, `weaponsStores`, `waypointList`, `sdccFlps` — and 2
//! holding boxed `Long`s via `build_long_list`/`read_long_list` —
//! `controlCommandVerification`, `activeWavelengths`); every one of the 7
//! costs exactly 1 outer-frame ref regardless of which kind, so the 5-vs-7
//! split doesn't change the total. 224 + 34 ≈ 258 is the bare-minimum
//! requirement counting only NEW real object/array refs — it does NOT
//! count the JNI-spec fact that every `Builder` fluent-setter call also
//! returns (and, if discarded, still pins) one local ref for its own
//! `this`-returning value, an overhead shared by every pre-existing scalar
//! field too and not specific to WP-C; that mechanism is real but not
//! cheaply hand-countable field-by-field, which is exactly why 320 leaves
//! real margin rather than a computed-to-the-slot number, and why
//! `St0601PacksTest.fullyPopulatedWpcFieldsRoundTrip` exercises this
//! empirically (WP-C's own worst case: every optional WP-C field set,
//! multi-item lists on every `Vec` field, plus a separate long-list
//! stress case — not a claim that the record's other 133 pre-existing
//! fields are populated too) rather than resting on this arithmetic
//! alone. Skipping the capacity call entirely WILL crash the JVM for
//! records with many populated fields.
//!
//! WP-C's list fields (`controlCommands`, `wavelengthsList`, `weaponsStores`,
//! `waypointList`, `sdccFlps`, and `payloadList`'s nested `records`) follow the
//! VTargetPack `with_local_frame` idiom: each list item is built/read inside
//! its own local frame so per-item refs are reclaimed before the next
//! iteration, bounding live-ref growth to O(1) per item regardless of list
//! length — see `build_vmti`/`read_vmti` in `st0903.rs` for the precedent.
//! `controlCommandVerification`/`activeWavelengths`/`SdccFlpField.precedingTags`
//! read via `jutil::read_long_list`, which applies the same per-item-frame
//! discipline (added in review response — the original hand-rolled loops had
//! no per-item frame, so a caller-constructed list longer than the ambient
//! frame's spare capacity could exhaust the JNI local-ref table and abort the
//! JVM rather than throw a catchable exception).

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::sys::{jint, jlong, jobject, jstring};
use tst_core::error::KlvFieldError;
use tst_core::klv::ImapbSpecial;
use tst_core::klv::st0601::{
    AirbaseLocations, ControlCommand, CountryCodes, EncodeConfig, IcingDetected,
    ImageHorizonPixels, Location, MetadataSubstreamId, OperationalMode, OutOfRangePolicy,
    PayloadList, PayloadRecord, PayloadType, PlatformStatus, SdccFlpField, SensorControlMode,
    SensorFovName, SensorFrameRate, St0601SentinelMeaning, UasDatalinkLs, ViewDomain,
    ViewDomainPair, WavelengthRecord, Waypoint, WeaponsStore, decode as decode_lenient,
    decode_strict, decode_strict_compliance, encode_strict_compliance, encode_to_vec_with,
    st0601_sentinel_meaning,
};
use tst_core::klv::st1010::{SdccFlp, decode_sdcc_flp, encode_sdcc_flp_mode2};
use tst_core::klv::universal_label::UniversalLabel;

use crate::error::{map_klv_decode_error, map_klv_encode_error, throw_klv_decode};
use crate::jutil::{
    build_field_errors, build_long_list, build_unknown_list, checked_u8, checked_u16, checked_u32,
    checked_u64, read_byte_buffer, read_long_list, read_nullable_byte_buffer, read_nullable_double,
    read_nullable_int, read_nullable_long, read_nullable_string, read_unknown_list,
    wrap_heap_byte_buffer,
};
use tst_pipeline::binding::BindingErrorKind;

// -----------------------------------------------------------------------
// Builder class / method-descriptor constants
// -----------------------------------------------------------------------

const BUILDER_CLASS: &str = "org/tstrans/klv/UasDatalinkLs$Builder";
const BUILDER_SIG_INT: &str = "(I)Lorg/tstrans/klv/UasDatalinkLs$Builder;";
const BUILDER_SIG_LONG: &str = "(J)Lorg/tstrans/klv/UasDatalinkLs$Builder;";
const BUILDER_SIG_DBL: &str = "(D)Lorg/tstrans/klv/UasDatalinkLs$Builder;";
const BUILDER_SIG_STR: &str = "(Ljava/lang/String;)Lorg/tstrans/klv/UasDatalinkLs$Builder;";
const BUILDER_SIG_BUF: &str = "(Ljava/nio/ByteBuffer;)Lorg/tstrans/klv/UasDatalinkLs$Builder;";
const BUILDER_SIG_LIST: &str = "(Ljava/util/List;)Lorg/tstrans/klv/UasDatalinkLs$Builder;";

/// ST 0601 LS typed + reserved tags — mirrors `tags::TAGS` in
/// `crates/tst-core/src/klv/st0601/tags.rs` (142 entries as of WP-C: 1-65,
/// 67-143). Tag 66 is the deprecated placeholder (permanently untyped by
/// design — ST 0601.19 §8.66: "This item has been Deprecated") and
/// 144..=255 are forward-compat; both may legitimately appear in `unknown`
/// (66 and 200 are the durable unknown-tag test stand-ins used across this
/// suite — never add them here). WP-C (Table C1) finished the sweep from
/// 66's neighbors through 143, including Tag 102 (MULTI-INSTANCE SDCC-FLP,
/// now typed via `sdccFlps`) and Tag 115 (MULTI-INSTANCE Control Command,
/// now typed via `controlCommands`) — keep this in sync with `tags::TAGS`
/// when new tags are typed, or a caller-supplied `unknown` entry for a
/// newly-typed tag will slip past this filter and get rejected downstream
/// by the real Rust encoder's own (stricter, canonical) check instead of
/// being silently dropped here per the documented "typed wins" collision
/// policy — mirrors tst-py's `is_st0601_typed_tag` (same decision: Tag 102
/// IS included, matching tst-core's typed status for the tag).
fn is_st0601_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=65 | 67..=143)
}

// ---------------------------------------------------------------------------
// ST 0601 — Tags 34/63/77 coded enums (WP-A Table A3)
// ---------------------------------------------------------------------------
//
// These helpers name the JNI crossing (raw codepoint `Integer`, no
// Python-enum-instance step) and delegate to tst-core's own wire tables —
// `to_wire`/`from_wire`, public since Arc 2 R2. No local copy, so no
// `#[non_exhaustive]` wildcard and nothing to drift; tst-core's
// `variant_inventory` tests pin the tables themselves.

/// Extract the ST 0601.19 §8.34 wire codepoint from an `IcingDetected`.
fn icing_detected_to_code(v: IcingDetected) -> u8 {
    v.to_wire()
}

/// Inverse of [`icing_detected_to_code`].
fn icing_detected_from_code(b: u8) -> IcingDetected {
    IcingDetected::from_wire(b)
}

/// Extract the ST 0601.19 §8.63 wire codepoint from a `SensorFovName`.
fn sensor_fov_name_to_code(v: SensorFovName) -> u8 {
    v.to_wire()
}

/// Inverse of [`sensor_fov_name_to_code`].
fn sensor_fov_name_from_code(b: u8) -> SensorFovName {
    SensorFovName::from_wire(b)
}

/// Extract the ST 0601.19 §8.77 wire codepoint from an `OperationalMode`.
fn operational_mode_to_code(v: OperationalMode) -> u8 {
    v.to_wire()
}

/// Inverse of [`operational_mode_to_code`].
fn operational_mode_from_code(b: u8) -> OperationalMode {
    OperationalMode::from_wire(b)
}

// ---------------------------------------------------------------------------
// ST 0601 — Tags 125/126 coded enums (WP-B Table B2)
// ---------------------------------------------------------------------------

/// Extract the ST 0601.19 §8.125 wire codepoint from a `PlatformStatus`.
fn platform_status_to_code(v: PlatformStatus) -> u8 {
    v.to_wire()
}

/// Inverse of [`platform_status_to_code`].
fn platform_status_from_code(b: u8) -> PlatformStatus {
    PlatformStatus::from_wire(b)
}

/// Extract the ST 0601.19 §8.126 wire codepoint from a `SensorControlMode`.
fn sensor_control_mode_to_code(v: SensorControlMode) -> u8 {
    v.to_wire()
}

/// Inverse of [`sensor_control_mode_to_code`].
fn sensor_control_mode_from_code(b: u8) -> SensorControlMode {
    SensorControlMode::from_wire(b)
}

// ---------------------------------------------------------------------------
// ST 1201.5 — imapb_specials side channel (WP-B)
// ---------------------------------------------------------------------------
//
// Crossing shape (DECIDED, shared with the Python binding): a
// `List<ImapbSpecialEntry>` of `(tag: int, code: String, payload: long)`
// entries. `code` names the `ImapbSpecial` family; `payload` is the
// NaN-id/signal value (0 for the payload-less codes). Mirrors tst-py's
// `imapb_special_to_code`/`imapb_special_from_code`.

/// Translate a Rust `ImapbSpecial` to its `(code, payload)` wire-string
/// pair. Throws `IllegalArgumentException` (rather than silently
/// mislabeling) on a future non-exhaustive variant: unlike the coded enums
/// above, `ImapbSpecial` has no tst-core wire-code helper to delegate to, so
/// this table is genuinely local and must fail loudly when it goes stale.
fn imapb_special_to_code(
    env: &mut JNIEnv,
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
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                "unknown ImapbSpecial variant crossing the binding",
            );
            return Err(jni::errors::Error::JavaException);
        }
    })
}

/// Inverse of [`imapb_special_to_code`]. Throws `IllegalArgumentException`
/// for a code string outside the 9-member set (audit #6 "validate-don't-drop"
/// stance — same as the Python `imapb_special_from_code`).
fn imapb_special_from_code(
    env: &mut JNIEnv,
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
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!(
                    "unknown imapbSpecials code {other:?}; expected one of below_min, above_max, \
                     pos_infinity, neg_infinity, pos_quiet_nan, neg_quiet_nan, pos_signaling_nan, \
                     neg_signaling_nan, user_defined"
                ),
            );
            return Err(jni::errors::Error::JavaException);
        }
    })
}

/// Range-check a Java `int` value against the i8 range, then narrow. Throws
/// `IllegalArgumentException` and returns `Err(JavaException)` on overflow —
/// local to this module (mirrors `jutil::checked_u8`'s idiom) since ST 0601 has
/// exactly one i8-typed field (Tag 39, `outside_air_temp_c`).
fn checked_i8(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<i8> {
    if !(-128..=127).contains(&value) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be -128..=127, got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as i8)
}

/// Range-check a Java `int` value against the i16 range, then narrow. Local
/// to this module (same rationale as `checked_i8` above) since ST 0601 has
/// exactly one i16-typed field (Tag 141's `Waypoint.prosecution_order`).
fn checked_i16(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<i16> {
    if !(-32768..=32767).contains(&value) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be -32768..=32767, got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as i16)
}

// ---------------------------------------------------------------------------
// ST 0601 — WP-C pack & list items (Table C1), carried inside UasDatalinkLs.
// Each nested type is a plain Java record (not a Builder — small, mostly-
// mandatory field counts, matching the `CoreId`/`GeoPoint` precedent rather
// than `VTargetPack`'s many-optional-field Builder): a `build_*`/`read_*`
// pair here, one level below `build_uas_datalink`/`read_uas_datalink`.
// Follows the VTargetPack nested-struct-list pattern for the MULTI-INSTANCE
// / VLP list fields (`with_local_frame` per item — see the module doc).
// Mirrors tst-py's `convert_*`/`py_to_*` functions for the same items.
// ---------------------------------------------------------------------------

/// Box an `Option<f64>` as a `java.lang.Double`, or `null` for `None`. Needed
/// because the WP-C pack types are plain records, not Builders — there is no
/// `if let Some(v) = ... { call_method(...) }` skip available; the canonical
/// constructor always takes every argument positionally.
fn boxed_double<'local>(
    env: &mut JNIEnv<'local>,
    v: Option<f64>,
) -> jni::errors::Result<JObject<'local>> {
    match v {
        Some(v) => env.new_object("java/lang/Double", "(D)V", &[JValue::Double(v)]),
        None => Ok(JObject::null()),
    }
}

/// Box an `Option<u64>` as a `java.lang.Long` (bit-reinterpreted), or `null`
/// for `None`. Sibling of [`boxed_double`].
fn boxed_long<'local>(
    env: &mut JNIEnv<'local>,
    v: Option<u64>,
) -> jni::errors::Result<JObject<'local>> {
    match v {
        Some(v) => env.new_object("java/lang/Long", "(J)V", &[JValue::Long(v as i64)]),
        None => Ok(JObject::null()),
    }
}

/// Read a MANDATORY (non-null) `String` accessor. Sibling of
/// `jutil::read_nullable_string` for the WP-C pack fields that are always
/// present on the Rust side (no `Option` wrapper).
fn read_string(env: &mut JNIEnv, obj: &JObject, method: &str) -> jni::errors::Result<String> {
    let s_obj = env
        .call_method(obj, method, "()Ljava/lang/String;", &[])?
        .l()?;
    let j_str: &jni::objects::JString = (&s_obj).into();
    env.get_string(j_str).map(Into::into)
}

// -- Item 81: Image Horizon Pixels ------------------------------------------

/// Builds via `ImageHorizonPixels$Builder` (not the canonical constructor —
/// see the Java Javadoc: the constructor's two 4-long same-typed runs risk
/// silent transposition; the Builder's named setters remove that risk).
fn build_image_horizon(
    env: &mut JNIEnv<'_>,
    h: &ImageHorizonPixels,
) -> jni::errors::Result<jobject> {
    const BUILDER_CLASS: &str = "org/tstrans/klv/ImageHorizonPixels$Builder";
    const SIG_INT: &str = "(I)Lorg/tstrans/klv/ImageHorizonPixels$Builder;";
    const SIG_DBL: &str = "(D)Lorg/tstrans/klv/ImageHorizonPixels$Builder;";
    let b = env.new_object(BUILDER_CLASS, "()V", &[])?;
    env.call_method(&b, "x0Pct", SIG_INT, &[JValue::Int(i32::from(h.x0_pct))])?;
    env.call_method(&b, "y0Pct", SIG_INT, &[JValue::Int(i32::from(h.y0_pct))])?;
    env.call_method(&b, "x1Pct", SIG_INT, &[JValue::Int(i32::from(h.x1_pct))])?;
    env.call_method(&b, "y1Pct", SIG_INT, &[JValue::Int(i32::from(h.y1_pct))])?;
    if let Some(v) = h.start_lat_deg {
        env.call_method(&b, "startLatDeg", SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = h.start_lon_deg {
        env.call_method(&b, "startLonDeg", SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = h.end_lat_deg {
        env.call_method(&b, "endLatDeg", SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = h.end_lon_deg {
        env.call_method(&b, "endLonDeg", SIG_DBL, &[JValue::Double(v)])?;
    }
    let obj = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/ImageHorizonPixels;", &[])?
        .l()?;
    Ok(obj.into_raw())
}

fn read_image_horizon(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<ImageHorizonPixels> {
    let x0 = env.call_method(obj, "x0Pct", "()I", &[])?.i()?;
    let y0 = env.call_method(obj, "y0Pct", "()I", &[])?.i()?;
    let x1 = env.call_method(obj, "x1Pct", "()I", &[])?.i()?;
    let y1 = env.call_method(obj, "y1Pct", "()I", &[])?.i()?;
    Ok(ImageHorizonPixels {
        x0_pct: checked_u8(env, i64::from(x0), "ImageHorizonPixels.x0Pct")?,
        y0_pct: checked_u8(env, i64::from(y0), "ImageHorizonPixels.y0Pct")?,
        x1_pct: checked_u8(env, i64::from(x1), "ImageHorizonPixels.x1Pct")?,
        y1_pct: checked_u8(env, i64::from(y1), "ImageHorizonPixels.y1Pct")?,
        start_lat_deg: read_nullable_double(env, obj, "startLatDeg")?,
        start_lon_deg: read_nullable_double(env, obj, "startLonDeg")?,
        end_lat_deg: read_nullable_double(env, obj, "endLatDeg")?,
        end_lon_deg: read_nullable_double(env, obj, "endLonDeg")?,
    })
}

// -- Item 115: Control Command (MULTI-INSTANCE) ------------------------------

fn build_control_command(env: &mut JNIEnv<'_>, c: &ControlCommand) -> jni::errors::Result<jobject> {
    let command = env.new_string(&c.command)?;
    let time_us = boxed_long(env, c.time_us)?;
    let obj = env.new_object(
        "org/tstrans/klv/ControlCommand",
        "(JLjava/lang/String;Ljava/lang/Long;)V",
        &[
            JValue::Long(c.id as i64),
            JValue::Object(&command),
            JValue::Object(&time_us),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_control_command(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<ControlCommand> {
    let id = env.call_method(obj, "id", "()J", &[])?.j()?;
    let command = read_string(env, obj, "command")?;
    let time_us = read_nullable_long(env, obj, "timeUs")?;
    Ok(ControlCommand {
        id: checked_u64(env, id, "ControlCommand.id")?,
        command,
        time_us: match time_us {
            Some(v) => Some(checked_u64(env, v, "ControlCommand.timeUs")?),
            None => None,
        },
    })
}

// -- Item 127: Sensor Frame Rate Pack ----------------------------------------

fn build_sensor_frame_rate(
    env: &mut JNIEnv<'_>,
    fr: &SensorFrameRate,
) -> jni::errors::Result<jobject> {
    let obj = env.new_object(
        "org/tstrans/klv/SensorFrameRate",
        "(JJ)V",
        &[
            JValue::Long(fr.numerator as i64),
            JValue::Long(fr.denominator as i64),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_sensor_frame_rate(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<SensorFrameRate> {
    let numerator = env.call_method(obj, "numerator", "()J", &[])?.j()?;
    let denominator = env.call_method(obj, "denominator", "()J", &[])?.j()?;
    Ok(SensorFrameRate {
        numerator: checked_u64(env, numerator, "SensorFrameRate.numerator")?,
        denominator: checked_u64(env, denominator, "SensorFrameRate.denominator")?,
    })
}

// -- Item 143: Metadata Substream Id -----------------------------------------

fn build_metadata_substream_id(
    env: &mut JNIEnv<'_>,
    ms: &MetadataSubstreamId,
) -> jni::errors::Result<jobject> {
    let uuid: JObject<'_> = match ms.uuid {
        Some(u) => env.byte_array_from_slice(&u)?.into(),
        None => JObject::null(),
    };
    let obj = env.new_object(
        "org/tstrans/klv/MetadataSubstreamId",
        "(J[B)V",
        &[JValue::Long(ms.local_id as i64), JValue::Object(&uuid)],
    )?;
    Ok(obj.into_raw())
}

fn read_metadata_substream_id(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<MetadataSubstreamId> {
    let local_id = env.call_method(obj, "localId", "()J", &[])?.j()?;
    let uuid_obj = env.call_method(obj, "uuid", "()[B", &[])?.l()?;
    let uuid = if uuid_obj.is_null() {
        None
    } else {
        let arr: JByteArray<'_> = uuid_obj.into();
        let bytes = env.convert_byte_array(&arr)?;
        if bytes.len() != 16 {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!(
                    "MetadataSubstreamId.uuid must be 16 bytes; got {}",
                    bytes.len()
                ),
            );
            return Err(jni::errors::Error::JavaException);
        }
        let mut u = [0u8; 16];
        u.copy_from_slice(&bytes);
        Some(u)
    };
    Ok(MetadataSubstreamId {
        local_id: checked_u64(env, local_id, "MetadataSubstreamId.localId")?,
        uuid,
    })
}

// -- Item 122: Country Codes --------------------------------------------------

fn build_country_codes(env: &mut JNIEnv<'_>, cc: &CountryCodes) -> jni::errors::Result<jobject> {
    let overflight: JObject<'_> = match cc.overflight.as_deref() {
        Some(s) => env.new_string(s)?.into(),
        None => JObject::null(),
    };
    let operator: JObject<'_> = match cc.operator.as_deref() {
        Some(s) => env.new_string(s)?.into(),
        None => JObject::null(),
    };
    let manufacture: JObject<'_> = match cc.manufacture.as_deref() {
        Some(s) => env.new_string(s)?.into(),
        None => JObject::null(),
    };
    let obj = env.new_object(
        "org/tstrans/klv/CountryCodes",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        &[
            JValue::Long(cc.coding_method as i64),
            JValue::Object(&overflight),
            JValue::Object(&operator),
            JValue::Object(&manufacture),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_country_codes(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<CountryCodes> {
    let coding_method = env.call_method(obj, "codingMethod", "()J", &[])?.j()?;
    Ok(CountryCodes {
        coding_method: checked_u64(env, coding_method, "CountryCodes.codingMethod")?,
        overflight: read_nullable_string(env, obj, "overflight")?,
        operator: read_nullable_string(env, obj, "operator")?,
        manufacture: read_nullable_string(env, obj, "manufacture")?,
    })
}

// -- Item 128: Wavelengths List -----------------------------------------------

fn build_wavelength_record(
    env: &mut JNIEnv<'_>,
    w: &WavelengthRecord,
) -> jni::errors::Result<jobject> {
    let name = env.new_string(&w.name)?;
    let obj = env.new_object(
        "org/tstrans/klv/WavelengthRecord",
        "(JDDLjava/lang/String;)V",
        &[
            JValue::Long(w.id as i64),
            JValue::Double(w.min_nm),
            JValue::Double(w.max_nm),
            JValue::Object(&name),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_wavelength_record(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<WavelengthRecord> {
    let id = env.call_method(obj, "id", "()J", &[])?.j()?;
    let min_nm = env.call_method(obj, "minNm", "()D", &[])?.d()?;
    let max_nm = env.call_method(obj, "maxNm", "()D", &[])?.d()?;
    let name = read_string(env, obj, "name")?;
    Ok(WavelengthRecord {
        id: checked_u64(env, id, "WavelengthRecord.id")?,
        min_nm,
        max_nm,
        name,
    })
}

// -- Location (shared by Item 130 and Item 141) -------------------------------

fn build_location(env: &mut JNIEnv<'_>, loc: &Location) -> jni::errors::Result<jobject> {
    let lat = boxed_double(env, loc.lat_deg)?;
    let lon = boxed_double(env, loc.lon_deg)?;
    let hae = boxed_double(env, loc.hae_m)?;
    let obj = env.new_object(
        "org/tstrans/klv/Location",
        "(Ljava/lang/Double;Ljava/lang/Double;Ljava/lang/Double;)V",
        &[
            JValue::Object(&lat),
            JValue::Object(&lon),
            JValue::Object(&hae),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_location(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<Location> {
    Ok(Location {
        lat_deg: read_nullable_double(env, obj, "latDeg")?,
        lon_deg: read_nullable_double(env, obj, "lonDeg")?,
        hae_m: read_nullable_double(env, obj, "haeM")?,
    })
}

// -- Item 130: Airbase Locations ----------------------------------------------

fn build_airbase_locations(
    env: &mut JNIEnv<'_>,
    al: &AirbaseLocations,
) -> jni::errors::Result<jobject> {
    let take_off: JObject<'_> = match al.take_off {
        Some(loc) => unsafe { JObject::from_raw(build_location(env, &loc)?) },
        None => JObject::null(),
    };
    let recovery: JObject<'_> = match al.recovery {
        Some(loc) => unsafe { JObject::from_raw(build_location(env, &loc)?) },
        None => JObject::null(),
    };
    let obj = env.new_object(
        "org/tstrans/klv/AirbaseLocations",
        "(Lorg/tstrans/klv/Location;Lorg/tstrans/klv/Location;)V",
        &[JValue::Object(&take_off), JValue::Object(&recovery)],
    )?;
    Ok(obj.into_raw())
}

fn read_airbase_locations(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<AirbaseLocations> {
    let take_off_obj = env
        .call_method(obj, "takeOff", "()Lorg/tstrans/klv/Location;", &[])?
        .l()?;
    let take_off = if take_off_obj.is_null() {
        None
    } else {
        Some(read_location(env, &take_off_obj)?)
    };
    let recovery_obj = env
        .call_method(obj, "recovery", "()Lorg/tstrans/klv/Location;", &[])?
        .l()?;
    let recovery = if recovery_obj.is_null() {
        None
    } else {
        Some(read_location(env, &recovery_obj)?)
    };
    Ok(AirbaseLocations { take_off, recovery })
}

// -- Item 138: Payload List ----------------------------------------------------
//
// Delegates to tst-core's `PayloadType::to_wire`/`from_wire` (public since
// Arc 2 R2). Unlike `IcingDetected`'s narrow wire byte, `PayloadType::Other`
// carries the type code's full BER-OID `u64` range, so the Java crossing
// (`PayloadRecord.payloadTypeCode`) is a `long`, not an `int`.

fn payload_type_to_code(v: PayloadType) -> u64 {
    v.to_wire()
}

fn payload_type_from_code(code: u64) -> PayloadType {
    PayloadType::from_wire(code)
}

fn build_payload_record(env: &mut JNIEnv<'_>, r: &PayloadRecord) -> jni::errors::Result<jobject> {
    let name = env.new_string(&r.name)?;
    let obj = env.new_object(
        "org/tstrans/klv/PayloadRecord",
        "(JJLjava/lang/String;)V",
        &[
            JValue::Long(r.id as i64),
            JValue::Long(payload_type_to_code(r.payload_type) as i64),
            JValue::Object(&name),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_payload_record(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<PayloadRecord> {
    let id = env.call_method(obj, "id", "()J", &[])?.j()?;
    let type_code = env.call_method(obj, "payloadTypeCode", "()J", &[])?.j()?;
    let name = read_string(env, obj, "name")?;
    Ok(PayloadRecord {
        id: checked_u64(env, id, "PayloadRecord.id")?,
        payload_type: payload_type_from_code(checked_u64(
            env,
            type_code,
            "PayloadRecord.payloadTypeCode",
        )?),
        name,
    })
}

fn build_payload_list(env: &mut JNIEnv<'_>, pl: &PayloadList) -> jni::errors::Result<jobject> {
    let records_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for r in &pl.records {
        // 16 slots: covers PayloadRecord's id+typeCode+name refs + JNI scratch.
        env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
            let raw = build_payload_record(inner_env, r)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &records_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    let obj = env.new_object(
        "org/tstrans/klv/PayloadList",
        "(JLjava/util/List;)V",
        &[JValue::Long(pl.count as i64), JValue::Object(&records_list)],
    )?;
    Ok(obj.into_raw())
}

fn read_payload_list(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<PayloadList> {
    let count = env.call_method(obj, "count", "()J", &[])?.j()?;
    let records_obj = env
        .call_method(obj, "records", "()Ljava/util/List;", &[])?
        .l()?;
    let size = env.call_method(&records_obj, "size", "()I", &[])?.i()?;
    let mut records = Vec::new();
    for i in 0..size {
        env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
            let item = inner_env
                .call_method(
                    &records_obj,
                    "get",
                    "(I)Ljava/lang/Object;",
                    &[JValue::Int(i)],
                )?
                .l()?;
            records.push(read_payload_record(inner_env, &item)?);
            Ok(())
        })?;
    }
    Ok(PayloadList {
        count: checked_u64(env, count, "PayloadList.count")?,
        records,
    })
}

// -- Item 140: Weapons Stores ---------------------------------------------------

/// Builds via `WeaponsStore$Builder` (not the canonical constructor — see
/// the Java Javadoc: the constructor's four consecutive `long` id arguments
/// risk silent transposition; the Builder's named setters remove that risk).
fn build_weapons_store(env: &mut JNIEnv<'_>, ws: &WeaponsStore) -> jni::errors::Result<jobject> {
    const BUILDER_CLASS: &str = "org/tstrans/klv/WeaponsStore$Builder";
    const SIG_LONG: &str = "(J)Lorg/tstrans/klv/WeaponsStore$Builder;";
    let weapon_type = env.new_string(&ws.weapon_type)?;
    let b = env.new_object(BUILDER_CLASS, "()V", &[])?;
    env.call_method(
        &b,
        "stationId",
        SIG_LONG,
        &[JValue::Long(ws.station_id as i64)],
    )?;
    env.call_method(
        &b,
        "hardpointId",
        SIG_LONG,
        &[JValue::Long(ws.hardpoint_id as i64)],
    )?;
    env.call_method(
        &b,
        "carriageId",
        SIG_LONG,
        &[JValue::Long(ws.carriage_id as i64)],
    )?;
    env.call_method(&b, "storeId", SIG_LONG, &[JValue::Long(ws.store_id as i64)])?;
    env.call_method(
        &b,
        "statusRaw",
        SIG_LONG,
        &[JValue::Long(ws.status_raw as i64)],
    )?;
    env.call_method(
        &b,
        "weaponType",
        "(Ljava/lang/String;)Lorg/tstrans/klv/WeaponsStore$Builder;",
        &[JValue::Object(&weapon_type)],
    )?;
    let obj = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/WeaponsStore;", &[])?
        .l()?;
    Ok(obj.into_raw())
}

fn read_weapons_store(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<WeaponsStore> {
    let station_id = env.call_method(obj, "stationId", "()J", &[])?.j()?;
    let hardpoint_id = env.call_method(obj, "hardpointId", "()J", &[])?.j()?;
    let carriage_id = env.call_method(obj, "carriageId", "()J", &[])?.j()?;
    let store_id = env.call_method(obj, "storeId", "()J", &[])?.j()?;
    let status_raw = env.call_method(obj, "statusRaw", "()J", &[])?.j()?;
    let weapon_type = read_string(env, obj, "weaponType")?;
    Ok(WeaponsStore {
        station_id: checked_u64(env, station_id, "WeaponsStore.stationId")?,
        hardpoint_id: checked_u64(env, hardpoint_id, "WeaponsStore.hardpointId")?,
        carriage_id: checked_u64(env, carriage_id, "WeaponsStore.carriageId")?,
        store_id: checked_u64(env, store_id, "WeaponsStore.storeId")?,
        status_raw: checked_u64(env, status_raw, "WeaponsStore.statusRaw")?,
        weapon_type,
    })
}

// -- Item 141: Waypoint List ------------------------------------------------------

fn build_waypoint(env: &mut JNIEnv<'_>, wp: &Waypoint) -> jni::errors::Result<jobject> {
    let info = boxed_long(env, wp.info)?;
    let location: JObject<'_> = match wp.location {
        Some(loc) => unsafe { JObject::from_raw(build_location(env, &loc)?) },
        None => JObject::null(),
    };
    let obj = env.new_object(
        "org/tstrans/klv/Waypoint",
        "(JILjava/lang/Long;Lorg/tstrans/klv/Location;)V",
        &[
            JValue::Long(wp.id as i64),
            JValue::Int(i32::from(wp.prosecution_order)),
            JValue::Object(&info),
            JValue::Object(&location),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_waypoint(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<Waypoint> {
    let id = env.call_method(obj, "id", "()J", &[])?.j()?;
    let prosecution_order = env.call_method(obj, "prosecutionOrder", "()I", &[])?.i()?;
    let info = read_nullable_long(env, obj, "info")?;
    let location_obj = env
        .call_method(obj, "location", "()Lorg/tstrans/klv/Location;", &[])?
        .l()?;
    let location = if location_obj.is_null() {
        None
    } else {
        Some(read_location(env, &location_obj)?)
    };
    Ok(Waypoint {
        id: checked_u64(env, id, "Waypoint.id")?,
        prosecution_order: checked_i16(
            env,
            i64::from(prosecution_order),
            "Waypoint.prosecutionOrder",
        )?,
        info: match info {
            Some(v) => Some(checked_u64(env, v, "Waypoint.info")?),
            None => None,
        },
        location,
    })
}

// -- Item 142: View Domain -----------------------------------------------------

fn build_view_domain_pair(
    env: &mut JNIEnv<'_>,
    p: &ViewDomainPair,
) -> jni::errors::Result<jobject> {
    let obj = env.new_object(
        "org/tstrans/klv/ViewDomainPair",
        "(DD)V",
        &[JValue::Double(p.start_deg), JValue::Double(p.range_deg)],
    )?;
    Ok(obj.into_raw())
}

fn read_view_domain_pair(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<ViewDomainPair> {
    let start_deg = env.call_method(obj, "startDeg", "()D", &[])?.d()?;
    let range_deg = env.call_method(obj, "rangeDeg", "()D", &[])?.d()?;
    Ok(ViewDomainPair {
        start_deg,
        range_deg,
    })
}

fn build_view_domain(env: &mut JNIEnv<'_>, vd: &ViewDomain) -> jni::errors::Result<jobject> {
    let azimuth: JObject<'_> = match vd.azimuth {
        Some(p) => unsafe { JObject::from_raw(build_view_domain_pair(env, &p)?) },
        None => JObject::null(),
    };
    let elevation: JObject<'_> = match vd.elevation {
        Some(p) => unsafe { JObject::from_raw(build_view_domain_pair(env, &p)?) },
        None => JObject::null(),
    };
    let roll: JObject<'_> = match vd.roll {
        Some(p) => unsafe { JObject::from_raw(build_view_domain_pair(env, &p)?) },
        None => JObject::null(),
    };
    let obj = env.new_object(
        "org/tstrans/klv/ViewDomain",
        "(Lorg/tstrans/klv/ViewDomainPair;Lorg/tstrans/klv/ViewDomainPair;Lorg/tstrans/klv/ViewDomainPair;)V",
        &[
            JValue::Object(&azimuth),
            JValue::Object(&elevation),
            JValue::Object(&roll),
        ],
    )?;
    Ok(obj.into_raw())
}

fn read_view_domain(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<ViewDomain> {
    let azimuth_obj = env
        .call_method(obj, "azimuth", "()Lorg/tstrans/klv/ViewDomainPair;", &[])?
        .l()?;
    let azimuth = if azimuth_obj.is_null() {
        None
    } else {
        Some(read_view_domain_pair(env, &azimuth_obj)?)
    };
    let elevation_obj = env
        .call_method(obj, "elevation", "()Lorg/tstrans/klv/ViewDomainPair;", &[])?
        .l()?;
    let elevation = if elevation_obj.is_null() {
        None
    } else {
        Some(read_view_domain_pair(env, &elevation_obj)?)
    };
    let roll_obj = env
        .call_method(obj, "roll", "()Lorg/tstrans/klv/ViewDomainPair;", &[])?
        .l()?;
    let roll = if roll_obj.is_null() {
        None
    } else {
        Some(read_view_domain_pair(env, &roll_obj)?)
    };
    Ok(ViewDomain {
        azimuth,
        elevation,
        roll,
    })
}

// -- Item 102: SDCC-FLP (MULTI-INSTANCE positional capture) ---------------------

fn build_sdcc_flp_field(env: &mut JNIEnv<'_>, f: &SdccFlpField) -> jni::errors::Result<jobject> {
    let preceding = build_long_list(env, f.preceding_tags.iter().map(|&t| i64::from(t)))?;
    let bytes =
        wrap_heap_byte_buffer(env, &f.bytes).map_err(|()| jni::errors::Error::JavaException)?;
    let obj = env.new_object(
        "org/tstrans/klv/SdccFlpField",
        "(Ljava/util/List;Ljava/nio/ByteBuffer;)V",
        &[JValue::Object(&preceding), JValue::Object(&bytes)],
    )?;
    Ok(obj.into_raw())
}

fn read_sdcc_flp_field(
    env: &mut JNIEnv<'_>,
    obj: &JObject<'_>,
) -> jni::errors::Result<SdccFlpField> {
    let preceding_obj = env
        .call_method(obj, "precedingTags", "()Ljava/util/List;", &[])?
        .l()?;
    let mut preceding_tags = Vec::new();
    for v in read_long_list(env, &preceding_obj)? {
        preceding_tags.push(checked_u32(env, v, "SdccFlpField.precedingTags")?);
    }
    let bytes_obj = env
        .call_method(obj, "bytes", "()Ljava/nio/ByteBuffer;", &[])?
        .l()?;
    let bytes = read_byte_buffer(env, &bytes_obj)?;
    Ok(SdccFlpField {
        preceding_tags,
        bytes,
    })
}

// -----------------------------------------------------------------------
// Decode entry point
// -----------------------------------------------------------------------

/// `org.tstrans.klv.Klv.nDecodeUasDatalink(byte[], boolean strict, boolean compliance)`
///
/// Decodes a full ST 0601 record (full buffer including the 16-byte UL).
/// Dispatches: compliance → `decode_strict_compliance`; strict → `decode_strict`;
/// else → lenient `decode`. On success, builds and returns a Java `UasDatalinkLs`
/// via its `Builder`. On failure, throws a `KlvDecodeException` and returns null.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nDecodeUasDatalink<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    buf: JByteArray<'local>,
    strict: jni::sys::jboolean,
    compliance: jni::sys::jboolean,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let bytes = match env.convert_byte_array(&buf) {
            Ok(b) => b,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nDecodeUasDatalink: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let result = if compliance != 0 {
            decode_strict_compliance(&bytes)
        } else if strict != 0 {
            decode_strict(&bytes)
        } else {
            decode_lenient(&bytes)
        };
        match result {
            Ok(rec) => match build_uas_datalink(env, &rec) {
                Ok(raw) => raw,
                Err(_) => JObject::null().into_raw(),
            },
            Err(e) => {
                map_klv_decode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

// -----------------------------------------------------------------------
// Encode entry points
// -----------------------------------------------------------------------

/// `org.tstrans.klv.Klv.nEncodeUasDatalinkWithPolicy(UasDatalinkLs, int policy) -> byte[]`
///
/// Reads all fields from the Java `UasDatalinkLs` record, builds a Rust
/// `UasDatalinkLs`, then calls [`encode_to_vec_with`] using an
/// [`OutOfRangePolicy`] mapped from the `policy` int:
/// - `0` → [`OutOfRangePolicy::Error`] (throws on any out-of-range value)
/// - `1` → [`OutOfRangePolicy::Indicator`] (emits the spec's Out-of-Range
///   special for eligible linear-range tags: 6, 7, 50, 51, 52, 79, 80,
///   90–93; separately, WP-B's 14 IMAPB tags — 96, 103-105, 109,
///   112-114, 117-120, 132, 134 — get their own ST 1201.5 BelowMin/AboveMax
///   special instead of the INT_MIN sentinel)
///
/// Any other ordinal throws `java.lang.IllegalArgumentException` — this
/// signals enum drift between the Java and Rust layers.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeUasDatalinkWithPolicy<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
    policy: jint,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Exact-ordinal validation: out-of-range means enum drift between
        // the Java and Rust layers — fail loudly.
        let oor_policy = match policy {
            0 => OutOfRangePolicy::Error,
            1 => OutOfRangePolicy::Indicator,
            other => {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("unknown OutOfRangePolicy ordinal {other}"),
                );
                return JObject::null().into_raw();
            }
        };
        let mut opts = EncodeConfig::default();
        opts.out_of_range_policy = oor_policy;
        match read_uas_datalink(env, &record) {
            Ok(rust_rec) => match encode_to_vec_with(&rust_rec, &opts) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeUasDatalinkWithPolicy: byte_array_from_slice failed: {e}"
                            ),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, &e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeUasDatalinkWithPolicy: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.nEncodeUasDatalinkStrictCompliance(UasDatalinkLs) -> byte[]`
///
/// Reads all fields from the Java `UasDatalinkLs` record, builds a Rust
/// `UasDatalinkLs`, calls `encode_strict_compliance`. Enforces mandatory-tag
/// presence (Tag 2 / Tag 65 / Tag 1) and structural ordering. Mirrors tst-py's
/// `encode_uas_datalink_strict_compliance`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nEncodeUasDatalinkStrictCompliance<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    record: JObject<'local>,
) -> jobject {
    crate::panic::jni_catch(
        &mut env,
        std::ptr::null_mut(),
        |env| match read_uas_datalink(env, &record) {
            Ok(rust_rec) => match encode_strict_compliance(&rust_rec) {
                Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                    Ok(arr) => arr.into_raw(),
                    Err(e) => {
                        let _ = env.throw_new(
                            "java/lang/RuntimeException",
                            format!(
                                "nEncodeUasDatalinkStrictCompliance: byte_array_from_slice failed: {e}"
                            ),
                        );
                        JObject::null().into_raw()
                    }
                },
                Err(e) => {
                    map_klv_encode_error(env, &e);
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                if !env.exception_check().unwrap_or(false) {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("nEncodeUasDatalinkStrictCompliance: field read failed: {e}"),
                    );
                }
                JObject::null().into_raw()
            }
        },
    )
}

// -----------------------------------------------------------------------
// Sentinel-meaning lookup
// -----------------------------------------------------------------------

/// `org.tstrans.klv.Klv.nSt0601SentinelMeaning(long tag) -> String | null`
///
/// Delegates to [`st0601_sentinel_meaning`] (the ST 0601.19 §7.5 special-value
/// assignments), mapping the [`St0601SentinelMeaning`] variants to the same
/// strings tst-py returns: `OutOfRange` → `"out_of_range"`, `Reserved` →
/// `"reserved"`, `NotAvailable` → `"not_available"`, no assignment → null.
/// Total over its input: a tag outside the u32 range simply has no assigned
/// meaning (null) — the only failure path is JNI string allocation.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_nSt0601SentinelMeaning<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    tag: jlong,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let meaning = u32::try_from(tag).ok().and_then(st0601_sentinel_meaning);
        let name = match meaning {
            Some(St0601SentinelMeaning::OutOfRange) => "out_of_range",
            Some(St0601SentinelMeaning::Reserved) => "reserved",
            Some(St0601SentinelMeaning::NotAvailable) => "not_available",
            // `#[non_exhaustive]` in tst-core: no current variant reaches the
            // wildcard; a future variant surfaces as null until mirrored here.
            Some(_) | None => return std::ptr::null_mut(),
        };
        match env.new_string(name) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("nSt0601SentinelMeaning: new_string failed: {e}"),
                );
                std::ptr::null_mut()
            }
        }
    })
}

// -----------------------------------------------------------------------
// Rust → Java builder (decode path)
// -----------------------------------------------------------------------

/// Build a `org.tstrans.klv.UasDatalinkLs` Java record from a Rust `UasDatalinkLs`
/// via the public mutable `Builder`. Mirrors `convert_uas_datalink_ls` in tst-py.
///
/// ### Local-ref capacity (MANDATORY)
///
/// Calls `env.ensure_local_capacity(320)` at the top. The pre-WP-C baseline
/// (133 fields: 12 Strings, ~83 Doubles, ~11 ByteBuffers, ~14 Integers, ~7
/// Longs, an `imapbSpecials` list) already needed all 224 of its slots; WP-C
/// adds ~34 more outer-frame refs across its 14 pack/list fields — see the
/// module doc's per-field tally table for the honest breakdown. 224 + 34 ≈
/// 258 is the bare-minimum requirement; 320 leaves real margin rather than a
/// tight fit (the first-cut 256 was later found to leave effectively zero
/// margin). Each WP-C list field's own items are built inside a per-item
/// `with_local_frame`, so only the item COUNT — not the item length —
/// affects this top-level budget.
fn build_uas_datalink(env: &mut JNIEnv<'_>, r: &UasDatalinkLs) -> jni::errors::Result<jobject> {
    // CRITICAL: must be called before any new_string / new_object below.
    // 320 slots covers 147 fields + builder + lists + JNI scratch with real margin.
    env.ensure_local_capacity(320)?;

    let b = env.new_object(BUILDER_CLASS, "()V", &[])?;

    // --- universal_label: UniversalLabel([u8;16]) → heap ByteBuffer (non-optional) ---
    {
        let ul_buf = wrap_heap_byte_buffer(env, &r.universal_label.0)
            .map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "universalLabel",
            BUILDER_SIG_BUF,
            &[JValue::Object(&ul_buf)],
        )?;
    }

    // --- declared_version: u8 → int (non-optional) ---
    env.call_method(
        &b,
        "declaredVersion",
        BUILDER_SIG_INT,
        &[JValue::Int(i32::from(r.declared_version))],
    )?;

    // --- Identity: Optional<String> fields ---
    if let Some(ref v) = r.mission_id {
        let j = env.new_string(v)?;
        env.call_method(&b, "missionId", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    if let Some(ref v) = r.platform_tail_number {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "platformTailNumber",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    if let Some(ref v) = r.platform_designation {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "platformDesignation",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    if let Some(ref v) = r.image_source_sensor {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "imageSourceSensor",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    if let Some(ref v) = r.image_coordinate_system {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "imageCoordinateSystem",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    if let Some(ref v) = r.platform_call_sign {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "platformCallSign",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // --- Optional<u8> → int ---
    if let Some(v) = r.uas_ls_version {
        env.call_method(
            &b,
            "uasLsVersion",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // --- Optional<u64> → long ---
    if let Some(v) = r.timestamp_us {
        env.call_method(
            &b,
            "timestampUs",
            BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }

    // --- Platform state: Optional<f64> → double ---
    if let Some(v) = r.platform_heading_deg {
        env.call_method(
            &b,
            "platformHeadingDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_pitch_deg {
        env.call_method(
            &b,
            "platformPitchDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_roll_deg {
        env.call_method(&b, "platformRollDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.platform_true_airspeed {
        env.call_method(
            &b,
            "platformTrueAirspeed",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_indicated_airspeed {
        env.call_method(
            &b,
            "platformIndicatedAirspeed",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_pitch_full_deg {
        env.call_method(
            &b,
            "platformPitchFullDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_roll_full_deg {
        env.call_method(
            &b,
            "platformRollFullDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_angle_of_attack_deg {
        env.call_method(
            &b,
            "platformAngleOfAttackDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // --- Sensor pose & position: Optional<f64> → double ---
    if let Some(v) = r.sensor_lat_deg {
        env.call_method(&b, "sensorLatDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_lon_deg {
        env.call_method(&b, "sensorLonDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_alt_m {
        env.call_method(&b, "sensorAltM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_ellipsoid_height_m {
        env.call_method(
            &b,
            "sensorEllipsoidHeightM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_hfov_deg {
        env.call_method(&b, "sensorHfovDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_vfov_deg {
        env.call_method(&b, "sensorVfovDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_rel_az_deg {
        env.call_method(&b, "sensorRelAzDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_rel_el_deg {
        env.call_method(&b, "sensorRelElDeg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_rel_roll_deg {
        env.call_method(
            &b,
            "sensorRelRollDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // --- Ranging & frame center ---
    if let Some(v) = r.slant_range_m {
        env.call_method(&b, "slantRangeM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.target_width_m {
        env.call_method(&b, "targetWidthM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.frame_center_lat_deg {
        env.call_method(
            &b,
            "frameCenterLatDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.frame_center_lon_deg {
        env.call_method(
            &b,
            "frameCenterLonDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.frame_center_elev_m {
        env.call_method(
            &b,
            "frameCenterElevM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.frame_center_ellipsoid_height_m {
        env.call_method(
            &b,
            "frameCenterEllipsoidHeightM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // --- Image corner offsets (tags 26–33) ---
    if let Some(v) = r.corner_lat_offset_p1_deg {
        env.call_method(
            &b,
            "cornerLatOffsetP1Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lon_offset_p1_deg {
        env.call_method(
            &b,
            "cornerLonOffsetP1Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lat_offset_p2_deg {
        env.call_method(
            &b,
            "cornerLatOffsetP2Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lon_offset_p2_deg {
        env.call_method(
            &b,
            "cornerLonOffsetP2Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lat_offset_p3_deg {
        env.call_method(
            &b,
            "cornerLatOffsetP3Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lon_offset_p3_deg {
        env.call_method(
            &b,
            "cornerLonOffsetP3Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lat_offset_p4_deg {
        env.call_method(
            &b,
            "cornerLatOffsetP4Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.corner_lon_offset_p4_deg {
        env.call_method(
            &b,
            "cornerLonOffsetP4Deg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // --- Image corners full lat/lon (tags 82–89) ---
    if let Some(v) = r.corner_lat_p1_deg {
        env.call_method(&b, "cornerLatP1Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lon_p1_deg {
        env.call_method(&b, "cornerLonP1Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lat_p2_deg {
        env.call_method(&b, "cornerLatP2Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lon_p2_deg {
        env.call_method(&b, "cornerLonP2Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lat_p3_deg {
        env.call_method(&b, "cornerLatP3Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lon_p3_deg {
        env.call_method(&b, "cornerLonP3Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lat_p4_deg {
        env.call_method(&b, "cornerLatP4Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.corner_lon_p4_deg {
        env.call_method(&b, "cornerLonP4Deg", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }

    // --- Misc ---
    if let Some(v) = r.generic_flag_data {
        env.call_method(
            &b,
            "genericFlagData",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }

    // Tag 48 — securityLocalSet (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = r.security_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "securityLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // Tag 74 — vmti (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = r.vmti {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(&b, "vmti", BUILDER_SIG_BUF, &[JValue::Object(&buf)])?;
    }

    // Tag 94 — miisCoreId (Vec<u8> → byte[])
    if let Some(ref bs) = r.miis_core_id {
        let arr = env.byte_array_from_slice(bs)?;
        env.call_method(
            &b,
            "miisCoreId",
            "([B)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&arr)],
        )?;
    }

    // --- WP-A Table A1: ranged f64 fields (tags 35-93 subset) → double ---
    if let Some(v) = r.target_location_lat_deg {
        env.call_method(
            &b,
            "targetLocationLatDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_location_lon_deg {
        env.call_method(
            &b,
            "targetLocationLonDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_location_elev_m {
        env.call_method(
            &b,
            "targetLocationElevM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_track_gate_width_px {
        env.call_method(
            &b,
            "targetTrackGateWidthPx",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_track_gate_height_px {
        env.call_method(
            &b,
            "targetTrackGateHeightPx",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_error_ce90_m {
        env.call_method(
            &b,
            "targetErrorCe90M",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.target_error_le90_m {
        env.call_method(
            &b,
            "targetErrorLe90M",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.wind_direction_deg {
        env.call_method(
            &b,
            "windDirectionDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.wind_speed {
        env.call_method(&b, "windSpeed", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.static_pressure_mbar {
        env.call_method(
            &b,
            "staticPressureMbar",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.density_altitude_m {
        env.call_method(
            &b,
            "densityAltitudeM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.differential_pressure_mbar {
        env.call_method(
            &b,
            "differentialPressureMbar",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.airfield_barometric_pressure_mbar {
        env.call_method(
            &b,
            "airfieldBarometricPressureMbar",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.airfield_elevation_m {
        env.call_method(
            &b,
            "airfieldElevationM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.relative_humidity_pct {
        env.call_method(
            &b,
            "relativeHumidityPct",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_vertical_speed {
        env.call_method(
            &b,
            "platformVerticalSpeed",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_sideslip_deg {
        env.call_method(
            &b,
            "platformSideslipDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_ground_speed {
        env.call_method(
            &b,
            "platformGroundSpeed",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.ground_range_m {
        env.call_method(&b, "groundRangeM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.platform_fuel_remaining_kg {
        env.call_method(
            &b,
            "platformFuelRemainingKg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_magnetic_heading_deg {
        env.call_method(
            &b,
            "platformMagneticHeadingDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_angle_of_attack_full_deg {
        env.call_method(
            &b,
            "platformAngleOfAttackFullDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_sideslip_full_deg {
        env.call_method(
            &b,
            "platformSideslipFullDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_lat_deg {
        env.call_method(
            &b,
            "alternatePlatformLatDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_lon_deg {
        env.call_method(
            &b,
            "alternatePlatformLonDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_alt_m {
        env.call_method(
            &b,
            "alternatePlatformAltM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_heading_deg {
        env.call_method(
            &b,
            "alternatePlatformHeadingDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_ellipsoid_height_m {
        env.call_method(
            &b,
            "alternatePlatformEllipsoidHeightM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_north_velocity {
        env.call_method(
            &b,
            "sensorNorthVelocity",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_east_velocity {
        env.call_method(
            &b,
            "sensorEastVelocity",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }

    // --- WP-B Table B1: IMAPB f64 fields (tags 96, 103-105, 109, 112-114, 117-120, 132, 134) → double ---
    if let Some(v) = r.target_width_extended_m {
        env.call_method(
            &b,
            "targetWidthExtendedM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.density_altitude_extended_m {
        env.call_method(
            &b,
            "densityAltitudeExtendedM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_ellipsoid_height_extended_m {
        env.call_method(
            &b,
            "sensorEllipsoidHeightExtendedM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.alternate_platform_ellipsoid_height_extended_m {
        env.call_method(
            &b,
            "alternatePlatformEllipsoidHeightExtendedM",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.range_to_recovery_km {
        env.call_method(
            &b,
            "rangeToRecoveryKm",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.platform_course_angle_deg {
        env.call_method(
            &b,
            "platformCourseAngleDeg",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.altitude_agl_m {
        env.call_method(&b, "altitudeAglM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.radar_altimeter_m {
        env.call_method(&b, "radarAltimeterM", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }
    if let Some(v) = r.sensor_azimuth_rate_dps {
        env.call_method(
            &b,
            "sensorAzimuthRateDps",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_elevation_rate_dps {
        env.call_method(
            &b,
            "sensorElevationRateDps",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.sensor_roll_rate_dps {
        env.call_method(
            &b,
            "sensorRollRateDps",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.mi_storage_percent_full {
        env.call_method(
            &b,
            "miStoragePercentFull",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.transmission_frequency_mhz {
        env.call_method(
            &b,
            "transmissionFrequencyMhz",
            BUILDER_SIG_DBL,
            &[JValue::Double(v)],
        )?;
    }
    if let Some(v) = r.zoom_percentage {
        env.call_method(&b, "zoomPercentage", BUILDER_SIG_DBL, &[JValue::Double(v)])?;
    }

    // --- WP-B Table B2: var-length int/enum fields (tags 110-139) ---
    // Tag 110 — timeAirborneS (u32 → long)
    if let Some(v) = r.time_airborne_s {
        env.call_method(
            &b,
            "timeAirborneS",
            BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(v))],
        )?;
    }
    // Tag 111 — propulsionUnitSpeedRpm (u32 → long)
    if let Some(v) = r.propulsion_unit_speed_rpm {
        env.call_method(
            &b,
            "propulsionUnitSpeedRpm",
            BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(v))],
        )?;
    }
    // Tag 123 — navsatsInView (u8 → int)
    if let Some(v) = r.navsats_in_view {
        env.call_method(
            &b,
            "navsatsInView",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 124 — positioningMethodSource (u8 bitfield → int)
    if let Some(v) = r.positioning_method_source {
        env.call_method(
            &b,
            "positioningMethodSource",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 125 — platformStatusCode (raw codepoint → int)
    if let Some(v) = r.platform_status {
        env.call_method(
            &b,
            "platformStatusCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(platform_status_to_code(v)))],
        )?;
    }
    // Tag 126 — sensorControlModeCode (raw codepoint → int)
    if let Some(v) = r.sensor_control_mode {
        env.call_method(
            &b,
            "sensorControlModeCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(sensor_control_mode_to_code(v)))],
        )?;
    }
    // Tag 131 — takeOffTimeUs (u64 → long; mirrors timestampUs's `as i64` cast)
    if let Some(v) = r.take_off_time_us {
        env.call_method(
            &b,
            "takeOffTimeUs",
            BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }
    // Tag 133 — miStorageCapacityGb (u32 → long)
    if let Some(v) = r.mi_storage_capacity_gb {
        env.call_method(
            &b,
            "miStorageCapacityGb",
            BUILDER_SIG_LONG,
            &[JValue::Long(i64::from(v))],
        )?;
    }
    // Tag 136 — leapSeconds (i32 → int)
    if let Some(v) = r.leap_seconds {
        env.call_method(&b, "leapSeconds", BUILDER_SIG_INT, &[JValue::Int(v)])?;
    }
    // Tag 137 — correctionOffsetUs (i64 → long)
    if let Some(v) = r.correction_offset_us {
        env.call_method(
            &b,
            "correctionOffsetUs",
            BUILDER_SIG_LONG,
            &[JValue::Long(v)],
        )?;
    }
    // Tag 139 — activePayloads (Vec<u8> → heap ByteBuffer)
    if let Some(ref bs) = r.active_payloads {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "activePayloads",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // --- WP-A Table A4: named nested-set raw byte fields → heap ByteBuffer ---
    // Tag 73 — rvt
    if let Some(ref bs) = r.rvt {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(&b, "rvt", BUILDER_SIG_BUF, &[JValue::Object(&buf)])?;
    }
    // Tag 95 — sarMiLocalSet
    if let Some(ref bs) = r.sar_mi_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "sarMiLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }
    // Tag 97 — rangeImageLocalSet
    if let Some(ref bs) = r.range_image_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "rangeImageLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }
    // Tag 98 — geoRegistrationLocalSet
    if let Some(ref bs) = r.geo_registration_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "geoRegistrationLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }
    // Tag 99 — compositeImagingLocalSet
    if let Some(ref bs) = r.composite_imaging_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "compositeImagingLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }
    // Tag 100 — segmentLocalSet
    if let Some(ref bs) = r.segment_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "segmentLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }
    // Tag 101 — amendLocalSet
    if let Some(ref bs) = r.amend_local_set {
        let buf = wrap_heap_byte_buffer(env, bs).map_err(|()| jni::errors::Error::JavaException)?;
        env.call_method(
            &b,
            "amendLocalSet",
            BUILDER_SIG_BUF,
            &[JValue::Object(&buf)],
        )?;
    }

    // --- WP-A Table A2: raw/simple scalar + string fields ---
    // Tag 39 — outsideAirTempC (Option<i8> → Integer; safe widening, no narrowing here)
    if let Some(v) = r.outside_air_temp_c {
        env.call_method(
            &b,
            "outsideAirTempC",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 60 — weaponLoad (Option<u16> → Integer)
    if let Some(v) = r.weapon_load {
        env.call_method(
            &b,
            "weaponLoad",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 61 — weaponFired (Option<u8> → Integer)
    if let Some(v) = r.weapon_fired {
        env.call_method(
            &b,
            "weaponFired",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 62 — laserPrfCode (Option<u16> → Integer)
    if let Some(v) = r.laser_prf_code {
        env.call_method(
            &b,
            "laserPrfCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(v))],
        )?;
    }
    // Tag 70 — alternatePlatformName (Option<String>)
    if let Some(ref v) = r.alternate_platform_name {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "alternatePlatformName",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 72 — eventStartTimeUs (Option<u64> → Long; mirrors timestampUs's `as i64` cast)
    if let Some(v) = r.event_start_time_us {
        env.call_method(
            &b,
            "eventStartTimeUs",
            BUILDER_SIG_LONG,
            &[JValue::Long(v as i64)],
        )?;
    }
    // Tag 106 — streamDesignator (Option<String>)
    if let Some(ref v) = r.stream_designator {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "streamDesignator",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 107 — operationalBase (Option<String>)
    if let Some(ref v) = r.operational_base {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "operationalBase",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 108 — broadcastSource (Option<String>)
    if let Some(ref v) = r.broadcast_source {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "broadcastSource",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }
    // Tag 129 — targetId (Option<String>)
    if let Some(ref v) = r.target_id {
        let j = env.new_string(v)?;
        env.call_method(&b, "targetId", BUILDER_SIG_STR, &[JValue::Object(&j)])?;
    }
    // Tag 135 — communicationsMethod (Option<String>)
    if let Some(ref v) = r.communications_method {
        let j = env.new_string(v)?;
        env.call_method(
            &b,
            "communicationsMethod",
            BUILDER_SIG_STR,
            &[JValue::Object(&j)],
        )?;
    }

    // --- WP-A Table A3: coded enums (tags 34/63/77) → raw-codepoint Integer ---
    // Tag 34 — icingDetectedCode
    if let Some(v) = r.icing_detected {
        env.call_method(
            &b,
            "icingDetectedCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(icing_detected_to_code(v)))],
        )?;
    }
    // Tag 63 — sensorFovNameCode
    if let Some(v) = r.sensor_fov_name {
        env.call_method(
            &b,
            "sensorFovNameCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(sensor_fov_name_to_code(v)))],
        )?;
    }
    // Tag 77 — operationalModeCode
    if let Some(v) = r.operational_mode {
        env.call_method(
            &b,
            "operationalModeCode",
            BUILDER_SIG_INT,
            &[JValue::Int(i32::from(operational_mode_to_code(v)))],
        )?;
    }

    // --- WP-C Table C1: pack & list items ---

    // Item 81 — imageHorizon
    if let Some(ref h) = r.image_horizon {
        let raw = build_image_horizon(env, h)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "imageHorizon",
            "(Lorg/tstrans/klv/ImageHorizonPixels;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 115 — controlCommands (MULTI-INSTANCE; always set, even if empty).
    // Each item is built inside its own local frame (VTargetPack idiom) so
    // per-item refs are reclaimed before the next iteration.
    let control_commands_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for c in &r.control_commands {
        env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
            let raw = build_control_command(inner_env, c)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &control_commands_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    env.call_method(
        &b,
        "controlCommands",
        BUILDER_SIG_LIST,
        &[JValue::Object(&control_commands_list)],
    )?;

    // Item 116 — controlCommandVerification (Option<Vec<u64>>)
    if let Some(ref ids) = r.control_command_verification {
        let list = build_long_list(env, ids.iter().map(|&v| v as i64))?;
        env.call_method(
            &b,
            "controlCommandVerification",
            BUILDER_SIG_LIST,
            &[JValue::Object(&list)],
        )?;
    }

    // Item 121 — activeWavelengths (Option<Vec<u64>>)
    if let Some(ref ids) = r.active_wavelengths {
        let list = build_long_list(env, ids.iter().map(|&v| v as i64))?;
        env.call_method(
            &b,
            "activeWavelengths",
            BUILDER_SIG_LIST,
            &[JValue::Object(&list)],
        )?;
    }

    // Item 127 — sensorFrameRate
    if let Some(ref fr) = r.sensor_frame_rate {
        let raw = build_sensor_frame_rate(env, fr)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "sensorFrameRate",
            "(Lorg/tstrans/klv/SensorFrameRate;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 143 — metadataSubstreamId
    if let Some(ref ms) = r.metadata_substream_id {
        let raw = build_metadata_substream_id(env, ms)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "metadataSubstreamId",
            "(Lorg/tstrans/klv/MetadataSubstreamId;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 122 — countryCodes
    if let Some(ref cc) = r.country_codes {
        let raw = build_country_codes(env, cc)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "countryCodes",
            "(Lorg/tstrans/klv/CountryCodes;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 128 — wavelengthsList (Option<Vec<WavelengthRecord>>)
    if let Some(ref list) = r.wavelengths_list {
        let jlist = env.new_object("java/util/ArrayList", "()V", &[])?;
        for w in list {
            env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                let raw = build_wavelength_record(inner_env, w)?;
                let obj = unsafe { JObject::from_raw(raw) };
                inner_env.call_method(
                    &jlist,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[JValue::Object(&obj)],
                )?;
                Ok(())
            })?;
        }
        env.call_method(
            &b,
            "wavelengthsList",
            BUILDER_SIG_LIST,
            &[JValue::Object(&jlist)],
        )?;
    }

    // Item 130 — airbaseLocations
    if let Some(ref al) = r.airbase_locations {
        let raw = build_airbase_locations(env, al)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "airbaseLocations",
            "(Lorg/tstrans/klv/AirbaseLocations;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 138 — payloadList (its own `records` list is built inside
    // `build_payload_list`'s per-item local frames)
    if let Some(ref pl) = r.payload_list {
        let raw = build_payload_list(env, pl)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "payloadList",
            "(Lorg/tstrans/klv/PayloadList;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 140 — weaponsStores (Option<Vec<WeaponsStore>>)
    if let Some(ref list) = r.weapons_stores {
        let jlist = env.new_object("java/util/ArrayList", "()V", &[])?;
        for ws in list {
            env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                let raw = build_weapons_store(inner_env, ws)?;
                let obj = unsafe { JObject::from_raw(raw) };
                inner_env.call_method(
                    &jlist,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[JValue::Object(&obj)],
                )?;
                Ok(())
            })?;
        }
        env.call_method(
            &b,
            "weaponsStores",
            BUILDER_SIG_LIST,
            &[JValue::Object(&jlist)],
        )?;
    }

    // Item 141 — waypointList (Option<Vec<Waypoint>>)
    if let Some(ref list) = r.waypoint_list {
        let jlist = env.new_object("java/util/ArrayList", "()V", &[])?;
        for wp in list {
            env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                let raw = build_waypoint(inner_env, wp)?;
                let obj = unsafe { JObject::from_raw(raw) };
                inner_env.call_method(
                    &jlist,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[JValue::Object(&obj)],
                )?;
                Ok(())
            })?;
        }
        env.call_method(
            &b,
            "waypointList",
            BUILDER_SIG_LIST,
            &[JValue::Object(&jlist)],
        )?;
    }

    // Item 142 — viewDomain
    if let Some(ref vd) = r.view_domain {
        let raw = build_view_domain(env, vd)?;
        let obj = unsafe { JObject::from_raw(raw) };
        env.call_method(
            &b,
            "viewDomain",
            "(Lorg/tstrans/klv/ViewDomain;)Lorg/tstrans/klv/UasDatalinkLs$Builder;",
            &[JValue::Object(&obj)],
        )?;
    }

    // Item 102 — sdccFlps (MULTI-INSTANCE; always set, even if empty)
    let sdcc_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for f in &r.sdcc_flps {
        env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
            let raw = build_sdcc_flp_field(inner_env, f)?;
            let obj = unsafe { JObject::from_raw(raw) };
            inner_env.call_method(
                &sdcc_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    env.call_method(
        &b,
        "sdccFlps",
        BUILDER_SIG_LIST,
        &[JValue::Object(&sdcc_list)],
    )?;

    // --- fieldErrors — always set (even if empty) ---
    let fe_list = build_field_errors(env, &r.field_errors)?;
    env.call_method(
        &b,
        "fieldErrors",
        BUILDER_SIG_LIST,
        &[JValue::Object(&fe_list)],
    )?;

    // --- sentinelTags — always set (even if empty) ---
    let sentinel_list = build_long_list(env, r.sentinel_tags.iter().map(|&t| t as i64))?;
    env.call_method(
        &b,
        "sentinelTags",
        BUILDER_SIG_LIST,
        &[JValue::Object(&sentinel_list)],
    )?;

    // --- imapbSpecials: Vec<(u32, ImapbSpecial)> → List<ImapbSpecialEntry>, always set ---
    let specials_list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for &(tag, special) in &r.imapb_specials {
        let (code, payload) = imapb_special_to_code(env, special)?;
        // 8 slots: covers the code String + entry object refs + JNI scratch per entry.
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
                &specials_list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&entry)],
            )?;
            Ok(())
        })?;
    }
    env.call_method(
        &b,
        "imapbSpecials",
        BUILDER_SIG_LIST,
        &[JValue::Object(&specials_list)],
    )?;

    // --- unknown — always set (even if empty) ---
    let unk_list = build_unknown_list(env, &r.unknown)?;
    env.call_method(
        &b,
        "unknown",
        BUILDER_SIG_LIST,
        &[JValue::Object(&unk_list)],
    )?;

    // build() → UasDatalinkLs
    let built = env
        .call_method(&b, "build", "()Lorg/tstrans/klv/UasDatalinkLs;", &[])?
        .l()?;
    Ok(built.into_raw())
}

// -----------------------------------------------------------------------
// Java → Rust reader (encode path)
// -----------------------------------------------------------------------

/// Read all fields from a Java `UasDatalinkLs` record into a Rust `UasDatalinkLs`.
/// Mirrors tst-py's `py_to_uas_datalink_ls` including:
/// - 16-byte UL validation (raises RuntimeException on wrong length)
/// - `is_st0601_typed_tag` collision-drop on `unknown`
/// - `field_errors` not round-tripped (decoder-only diagnostic)
///
/// ### Local-ref capacity (MANDATORY, added WP-C)
///
/// Calls `env.ensure_local_capacity(320)` at the top — this function had NO
/// such call before WP-C (pre-existing debt found by the B6 review): every
/// `read_nullable_*` accessor call mints a local ref that lives until this
/// function returns, and the WP-C pack fields (each a nested-record read,
/// several with their own list-of-records reads) multiply that pressure
/// further. Sized to match `build_uas_datalink`'s budget — see that
/// function's doc and the module doc's per-field tally table for the
/// field-count rationale (320, not the first-cut 256 — see the module doc).
#[allow(clippy::field_reassign_with_default)]
fn read_uas_datalink(
    env: &mut JNIEnv<'_>,
    rec: &JObject<'_>,
) -> jni::errors::Result<UasDatalinkLs> {
    // CRITICAL: must be called before any call_method / get_string below.
    env.ensure_local_capacity(320)?;

    let mut r = UasDatalinkLs::default();

    // --- universal_label: ByteBuffer → UniversalLabel([u8;16]) ---
    // Use read_byte_buffer to honour position/limit and support direct buffers.
    {
        let bb_obj = env
            .call_method(rec, "universalLabel", "()Ljava/nio/ByteBuffer;", &[])?
            .l()?;
        let ul_bytes = read_byte_buffer(env, &bb_obj)?;
        if ul_bytes.len() != 16 {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                format!("universalLabel must be 16 bytes, got {}", ul_bytes.len()),
            );
            return Err(jni::errors::Error::JavaException);
        }
        let mut ul = [0u8; 16];
        ul.copy_from_slice(&ul_bytes);
        r.universal_label = UniversalLabel(ul);
    }

    // --- declared_version: int → u8 (non-optional, primitive) ---
    {
        let v = env.call_method(rec, "declaredVersion", "()I", &[])?.i()?;
        r.declared_version = checked_u8(env, i64::from(v), "declaredVersion")?;
    }

    // --- Identity: nullable String fields ---
    r.mission_id = read_nullable_string(env, rec, "missionId")?;
    r.platform_tail_number = read_nullable_string(env, rec, "platformTailNumber")?;
    r.platform_designation = read_nullable_string(env, rec, "platformDesignation")?;
    r.image_source_sensor = read_nullable_string(env, rec, "imageSourceSensor")?;
    r.image_coordinate_system = read_nullable_string(env, rec, "imageCoordinateSystem")?;
    r.platform_call_sign = read_nullable_string(env, rec, "platformCallSign")?;

    // --- uasLsVersion: nullable Integer → Option<u8> ---
    if let Some(v) = read_nullable_int(env, rec, "uasLsVersion")? {
        r.uas_ls_version = Some(checked_u8(env, i64::from(v), "uasLsVersion")?);
    }

    // --- timestampUs: nullable Long → Option<u64> ---
    if let Some(v) = read_nullable_long(env, rec, "timestampUs")? {
        r.timestamp_us = Some(v as u64);
    }

    // --- Platform state: nullable Double → Option<f64> ---
    if let Some(v) = read_nullable_double(env, rec, "platformHeadingDeg")? {
        r.platform_heading_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformPitchDeg")? {
        r.platform_pitch_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformRollDeg")? {
        r.platform_roll_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformTrueAirspeed")? {
        r.platform_true_airspeed = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformIndicatedAirspeed")? {
        r.platform_indicated_airspeed = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformPitchFullDeg")? {
        r.platform_pitch_full_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformRollFullDeg")? {
        r.platform_roll_full_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformAngleOfAttackDeg")? {
        r.platform_angle_of_attack_deg = Some(v);
    }

    // --- Sensor pose & position ---
    if let Some(v) = read_nullable_double(env, rec, "sensorLatDeg")? {
        r.sensor_lat_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorLonDeg")? {
        r.sensor_lon_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorAltM")? {
        r.sensor_alt_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorEllipsoidHeightM")? {
        r.sensor_ellipsoid_height_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorHfovDeg")? {
        r.sensor_hfov_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorVfovDeg")? {
        r.sensor_vfov_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorRelAzDeg")? {
        r.sensor_rel_az_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorRelElDeg")? {
        r.sensor_rel_el_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorRelRollDeg")? {
        r.sensor_rel_roll_deg = Some(v);
    }

    // --- Ranging & frame center ---
    if let Some(v) = read_nullable_double(env, rec, "slantRangeM")? {
        r.slant_range_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetWidthM")? {
        r.target_width_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "frameCenterLatDeg")? {
        r.frame_center_lat_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "frameCenterLonDeg")? {
        r.frame_center_lon_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "frameCenterElevM")? {
        r.frame_center_elev_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "frameCenterEllipsoidHeightM")? {
        r.frame_center_ellipsoid_height_m = Some(v);
    }

    // --- Image corner offsets (tags 26–33) ---
    if let Some(v) = read_nullable_double(env, rec, "cornerLatOffsetP1Deg")? {
        r.corner_lat_offset_p1_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonOffsetP1Deg")? {
        r.corner_lon_offset_p1_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatOffsetP2Deg")? {
        r.corner_lat_offset_p2_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonOffsetP2Deg")? {
        r.corner_lon_offset_p2_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatOffsetP3Deg")? {
        r.corner_lat_offset_p3_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonOffsetP3Deg")? {
        r.corner_lon_offset_p3_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatOffsetP4Deg")? {
        r.corner_lat_offset_p4_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonOffsetP4Deg")? {
        r.corner_lon_offset_p4_deg = Some(v);
    }

    // --- Image corners full lat/lon (tags 82–89) ---
    if let Some(v) = read_nullable_double(env, rec, "cornerLatP1Deg")? {
        r.corner_lat_p1_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonP1Deg")? {
        r.corner_lon_p1_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatP2Deg")? {
        r.corner_lat_p2_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonP2Deg")? {
        r.corner_lon_p2_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatP3Deg")? {
        r.corner_lat_p3_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonP3Deg")? {
        r.corner_lon_p3_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLatP4Deg")? {
        r.corner_lat_p4_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "cornerLonP4Deg")? {
        r.corner_lon_p4_deg = Some(v);
    }

    // --- Misc ---
    if let Some(v) = read_nullable_int(env, rec, "genericFlagData")? {
        r.generic_flag_data = Some(checked_u8(env, i64::from(v), "genericFlagData")?);
    }
    r.security_local_set = read_nullable_byte_buffer(env, rec, "securityLocalSet")?;
    r.vmti = read_nullable_byte_buffer(env, rec, "vmti")?;

    // --- miisCoreId: nullable byte[] → Option<Vec<u8>> ---
    {
        let arr_val = env.call_method(rec, "miisCoreId", "()[B", &[])?.l()?;
        if !arr_val.is_null() {
            let arr: jni::objects::JByteArray<'_> = arr_val.into();
            r.miis_core_id = Some(env.convert_byte_array(&arr)?);
        }
    }

    // --- WP-A Table A1: ranged f64 fields (tags 35-93 subset) ---
    if let Some(v) = read_nullable_double(env, rec, "targetLocationLatDeg")? {
        r.target_location_lat_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetLocationLonDeg")? {
        r.target_location_lon_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetLocationElevM")? {
        r.target_location_elev_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetTrackGateWidthPx")? {
        r.target_track_gate_width_px = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetTrackGateHeightPx")? {
        r.target_track_gate_height_px = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetErrorCe90M")? {
        r.target_error_ce90_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "targetErrorLe90M")? {
        r.target_error_le90_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "windDirectionDeg")? {
        r.wind_direction_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "windSpeed")? {
        r.wind_speed = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "staticPressureMbar")? {
        r.static_pressure_mbar = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "densityAltitudeM")? {
        r.density_altitude_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "differentialPressureMbar")? {
        r.differential_pressure_mbar = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "airfieldBarometricPressureMbar")? {
        r.airfield_barometric_pressure_mbar = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "airfieldElevationM")? {
        r.airfield_elevation_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "relativeHumidityPct")? {
        r.relative_humidity_pct = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformVerticalSpeed")? {
        r.platform_vertical_speed = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformSideslipDeg")? {
        r.platform_sideslip_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformGroundSpeed")? {
        r.platform_ground_speed = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "groundRangeM")? {
        r.ground_range_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformFuelRemainingKg")? {
        r.platform_fuel_remaining_kg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformMagneticHeadingDeg")? {
        r.platform_magnetic_heading_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformAngleOfAttackFullDeg")? {
        r.platform_angle_of_attack_full_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformSideslipFullDeg")? {
        r.platform_sideslip_full_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformLatDeg")? {
        r.alternate_platform_lat_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformLonDeg")? {
        r.alternate_platform_lon_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformAltM")? {
        r.alternate_platform_alt_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformHeadingDeg")? {
        r.alternate_platform_heading_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformEllipsoidHeightM")? {
        r.alternate_platform_ellipsoid_height_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorNorthVelocity")? {
        r.sensor_north_velocity = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorEastVelocity")? {
        r.sensor_east_velocity = Some(v);
    }

    // --- WP-B Table B1: IMAPB f64 fields ---
    if let Some(v) = read_nullable_double(env, rec, "targetWidthExtendedM")? {
        r.target_width_extended_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "densityAltitudeExtendedM")? {
        r.density_altitude_extended_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorEllipsoidHeightExtendedM")? {
        r.sensor_ellipsoid_height_extended_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "alternatePlatformEllipsoidHeightExtendedM")? {
        r.alternate_platform_ellipsoid_height_extended_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "rangeToRecoveryKm")? {
        r.range_to_recovery_km = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "platformCourseAngleDeg")? {
        r.platform_course_angle_deg = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "altitudeAglM")? {
        r.altitude_agl_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "radarAltimeterM")? {
        r.radar_altimeter_m = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorAzimuthRateDps")? {
        r.sensor_azimuth_rate_dps = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorElevationRateDps")? {
        r.sensor_elevation_rate_dps = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "sensorRollRateDps")? {
        r.sensor_roll_rate_dps = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "miStoragePercentFull")? {
        r.mi_storage_percent_full = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "transmissionFrequencyMhz")? {
        r.transmission_frequency_mhz = Some(v);
    }
    if let Some(v) = read_nullable_double(env, rec, "zoomPercentage")? {
        r.zoom_percentage = Some(v);
    }

    // --- WP-B Table B2: var-length int/enum fields ---
    // Tag 110 — timeAirborneS: nullable Long → Option<u32>
    if let Some(v) = read_nullable_long(env, rec, "timeAirborneS")? {
        r.time_airborne_s = Some(checked_u32(env, v, "timeAirborneS")?);
    }
    // Tag 111 — propulsionUnitSpeedRpm: nullable Long → Option<u32>
    if let Some(v) = read_nullable_long(env, rec, "propulsionUnitSpeedRpm")? {
        r.propulsion_unit_speed_rpm = Some(checked_u32(env, v, "propulsionUnitSpeedRpm")?);
    }
    // Tag 123 — navsatsInView: nullable Integer → Option<u8>
    if let Some(v) = read_nullable_int(env, rec, "navsatsInView")? {
        r.navsats_in_view = Some(checked_u8(env, i64::from(v), "navsatsInView")?);
    }
    // Tag 124 — positioningMethodSource: nullable Integer → Option<u8>
    if let Some(v) = read_nullable_int(env, rec, "positioningMethodSource")? {
        r.positioning_method_source =
            Some(checked_u8(env, i64::from(v), "positioningMethodSource")?);
    }
    // Tag 125 — platformStatusCode: nullable Integer raw code → Option<PlatformStatus>
    if let Some(v) = read_nullable_int(env, rec, "platformStatusCode")? {
        let c = checked_u8(env, i64::from(v), "platformStatusCode")?;
        r.platform_status = Some(platform_status_from_code(c));
    }
    // Tag 126 — sensorControlModeCode: nullable Integer raw code → Option<SensorControlMode>
    if let Some(v) = read_nullable_int(env, rec, "sensorControlModeCode")? {
        let c = checked_u8(env, i64::from(v), "sensorControlModeCode")?;
        r.sensor_control_mode = Some(sensor_control_mode_from_code(c));
    }
    // Tag 131 — takeOffTimeUs: nullable Long → Option<u64> (mirrors timestampUs's `as u64` cast)
    if let Some(v) = read_nullable_long(env, rec, "takeOffTimeUs")? {
        r.take_off_time_us = Some(v as u64);
    }
    // Tag 133 — miStorageCapacityGb: nullable Long → Option<u32>
    if let Some(v) = read_nullable_long(env, rec, "miStorageCapacityGb")? {
        r.mi_storage_capacity_gb = Some(checked_u32(env, v, "miStorageCapacityGb")?);
    }
    // Tag 136 — leapSeconds: nullable Integer → Option<i32> (Java int == Rust i32, no narrowing)
    if let Some(v) = read_nullable_int(env, rec, "leapSeconds")? {
        r.leap_seconds = Some(v);
    }
    // Tag 137 — correctionOffsetUs: nullable Long → Option<i64> (Java long == Rust i64, no narrowing)
    if let Some(v) = read_nullable_long(env, rec, "correctionOffsetUs")? {
        r.correction_offset_us = Some(v);
    }
    // Tag 139 — activePayloads: nullable ByteBuffer → Option<Vec<u8>>
    r.active_payloads = read_nullable_byte_buffer(env, rec, "activePayloads")?;

    // --- WP-A Table A4: named nested-set raw byte fields ---
    r.rvt = read_nullable_byte_buffer(env, rec, "rvt")?;
    r.sar_mi_local_set = read_nullable_byte_buffer(env, rec, "sarMiLocalSet")?;
    r.range_image_local_set = read_nullable_byte_buffer(env, rec, "rangeImageLocalSet")?;
    r.geo_registration_local_set = read_nullable_byte_buffer(env, rec, "geoRegistrationLocalSet")?;
    r.composite_imaging_local_set =
        read_nullable_byte_buffer(env, rec, "compositeImagingLocalSet")?;
    r.segment_local_set = read_nullable_byte_buffer(env, rec, "segmentLocalSet")?;
    r.amend_local_set = read_nullable_byte_buffer(env, rec, "amendLocalSet")?;

    // --- WP-A Table A2: raw/simple scalar + string fields ---
    // Tag 39 — outsideAirTempC: nullable Integer → Option<i8>
    if let Some(v) = read_nullable_int(env, rec, "outsideAirTempC")? {
        r.outside_air_temp_c = Some(checked_i8(env, i64::from(v), "outsideAirTempC")?);
    }
    // Tag 60 — weaponLoad: nullable Integer → Option<u16>
    if let Some(v) = read_nullable_int(env, rec, "weaponLoad")? {
        r.weapon_load = Some(checked_u16(env, i64::from(v), "weaponLoad")?);
    }
    // Tag 61 — weaponFired: nullable Integer → Option<u8>
    if let Some(v) = read_nullable_int(env, rec, "weaponFired")? {
        r.weapon_fired = Some(checked_u8(env, i64::from(v), "weaponFired")?);
    }
    // Tag 62 — laserPrfCode: nullable Integer → Option<u16>
    if let Some(v) = read_nullable_int(env, rec, "laserPrfCode")? {
        r.laser_prf_code = Some(checked_u16(env, i64::from(v), "laserPrfCode")?);
    }
    r.alternate_platform_name = read_nullable_string(env, rec, "alternatePlatformName")?;
    // Tag 72 — eventStartTimeUs: nullable Long → Option<u64> (mirrors timestampUs's `as u64` cast)
    if let Some(v) = read_nullable_long(env, rec, "eventStartTimeUs")? {
        r.event_start_time_us = Some(v as u64);
    }
    r.stream_designator = read_nullable_string(env, rec, "streamDesignator")?;
    r.operational_base = read_nullable_string(env, rec, "operationalBase")?;
    r.broadcast_source = read_nullable_string(env, rec, "broadcastSource")?;
    r.target_id = read_nullable_string(env, rec, "targetId")?;
    r.communications_method = read_nullable_string(env, rec, "communicationsMethod")?;

    // --- WP-A Table A3: coded enums (tags 34/63/77) — nullable Integer raw code ---
    if let Some(v) = read_nullable_int(env, rec, "icingDetectedCode")? {
        let c = checked_u8(env, i64::from(v), "icingDetectedCode")?;
        r.icing_detected = Some(icing_detected_from_code(c));
    }
    if let Some(v) = read_nullable_int(env, rec, "sensorFovNameCode")? {
        let c = checked_u8(env, i64::from(v), "sensorFovNameCode")?;
        r.sensor_fov_name = Some(sensor_fov_name_from_code(c));
    }
    if let Some(v) = read_nullable_int(env, rec, "operationalModeCode")? {
        let c = checked_u8(env, i64::from(v), "operationalModeCode")?;
        r.operational_mode = Some(operational_mode_from_code(c));
    }

    // --- WP-C Table C1: pack & list items ---

    // Item 81 — imageHorizon
    {
        let obj = env
            .call_method(
                rec,
                "imageHorizon",
                "()Lorg/tstrans/klv/ImageHorizonPixels;",
                &[],
            )?
            .l()?;
        if !obj.is_null() {
            r.image_horizon = Some(read_image_horizon(env, &obj)?);
        }
    }

    // Item 115 — controlCommands (List<ControlCommand>, MULTI-INSTANCE)
    {
        let list_obj = env
            .call_method(rec, "controlCommands", "()Ljava/util/List;", &[])?
            .l()?;
        let size = env.call_method(&list_obj, "size", "()I", &[])?.i()?;
        for i in 0..size {
            env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                let item = inner_env
                    .call_method(&list_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                r.control_commands
                    .push(read_control_command(inner_env, &item)?);
                Ok(())
            })?;
        }
    }

    // Item 116 — controlCommandVerification (nullable List<Long>)
    {
        let obj = env
            .call_method(rec, "controlCommandVerification", "()Ljava/util/List;", &[])?
            .l()?;
        if !obj.is_null() {
            let mut ids = Vec::new();
            for v in read_long_list(env, &obj)? {
                ids.push(checked_u64(env, v, "controlCommandVerification")?);
            }
            r.control_command_verification = Some(ids);
        }
    }

    // Item 121 — activeWavelengths (nullable List<Long>)
    {
        let obj = env
            .call_method(rec, "activeWavelengths", "()Ljava/util/List;", &[])?
            .l()?;
        if !obj.is_null() {
            let mut ids = Vec::new();
            for v in read_long_list(env, &obj)? {
                ids.push(checked_u64(env, v, "activeWavelengths")?);
            }
            r.active_wavelengths = Some(ids);
        }
    }

    // Item 127 — sensorFrameRate
    {
        let obj = env
            .call_method(
                rec,
                "sensorFrameRate",
                "()Lorg/tstrans/klv/SensorFrameRate;",
                &[],
            )?
            .l()?;
        if !obj.is_null() {
            r.sensor_frame_rate = Some(read_sensor_frame_rate(env, &obj)?);
        }
    }

    // Item 143 — metadataSubstreamId
    {
        let obj = env
            .call_method(
                rec,
                "metadataSubstreamId",
                "()Lorg/tstrans/klv/MetadataSubstreamId;",
                &[],
            )?
            .l()?;
        if !obj.is_null() {
            r.metadata_substream_id = Some(read_metadata_substream_id(env, &obj)?);
        }
    }

    // Item 122 — countryCodes
    {
        let obj = env
            .call_method(rec, "countryCodes", "()Lorg/tstrans/klv/CountryCodes;", &[])?
            .l()?;
        if !obj.is_null() {
            r.country_codes = Some(read_country_codes(env, &obj)?);
        }
    }

    // Item 128 — wavelengthsList (nullable List<WavelengthRecord>)
    {
        let list_obj = env
            .call_method(rec, "wavelengthsList", "()Ljava/util/List;", &[])?
            .l()?;
        if !list_obj.is_null() {
            let size = env.call_method(&list_obj, "size", "()I", &[])?.i()?;
            let mut list = Vec::new();
            for i in 0..size {
                env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                    let item = inner_env
                        .call_method(&list_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                        .l()?;
                    list.push(read_wavelength_record(inner_env, &item)?);
                    Ok(())
                })?;
            }
            r.wavelengths_list = Some(list);
        }
    }

    // Item 130 — airbaseLocations
    {
        let obj = env
            .call_method(
                rec,
                "airbaseLocations",
                "()Lorg/tstrans/klv/AirbaseLocations;",
                &[],
            )?
            .l()?;
        if !obj.is_null() {
            r.airbase_locations = Some(read_airbase_locations(env, &obj)?);
        }
    }

    // Item 138 — payloadList (its own `records` list is read inside
    // `read_payload_list`'s per-item local frames)
    {
        let obj = env
            .call_method(rec, "payloadList", "()Lorg/tstrans/klv/PayloadList;", &[])?
            .l()?;
        if !obj.is_null() {
            r.payload_list = Some(read_payload_list(env, &obj)?);
        }
    }

    // Item 140 — weaponsStores (nullable List<WeaponsStore>)
    {
        let list_obj = env
            .call_method(rec, "weaponsStores", "()Ljava/util/List;", &[])?
            .l()?;
        if !list_obj.is_null() {
            let size = env.call_method(&list_obj, "size", "()I", &[])?.i()?;
            let mut list = Vec::new();
            for i in 0..size {
                env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                    let item = inner_env
                        .call_method(&list_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                        .l()?;
                    list.push(read_weapons_store(inner_env, &item)?);
                    Ok(())
                })?;
            }
            r.weapons_stores = Some(list);
        }
    }

    // Item 141 — waypointList (nullable List<Waypoint>)
    {
        let list_obj = env
            .call_method(rec, "waypointList", "()Ljava/util/List;", &[])?
            .l()?;
        if !list_obj.is_null() {
            let size = env.call_method(&list_obj, "size", "()I", &[])?.i()?;
            let mut list = Vec::new();
            for i in 0..size {
                env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                    let item = inner_env
                        .call_method(&list_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                        .l()?;
                    list.push(read_waypoint(inner_env, &item)?);
                    Ok(())
                })?;
            }
            r.waypoint_list = Some(list);
        }
    }

    // Item 142 — viewDomain
    {
        let obj = env
            .call_method(rec, "viewDomain", "()Lorg/tstrans/klv/ViewDomain;", &[])?
            .l()?;
        if !obj.is_null() {
            r.view_domain = Some(read_view_domain(env, &obj)?);
        }
    }

    // Item 102 — sdccFlps (List<SdccFlpField>, MULTI-INSTANCE, always set)
    {
        let list_obj = env
            .call_method(rec, "sdccFlps", "()Ljava/util/List;", &[])?
            .l()?;
        let size = env.call_method(&list_obj, "size", "()I", &[])?.i()?;
        for i in 0..size {
            env.with_local_frame(16, |inner_env| -> jni::errors::Result<()> {
                let item = inner_env
                    .call_method(&list_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                r.sdcc_flps.push(read_sdcc_flp_field(inner_env, &item)?);
                Ok(())
            })?;
        }
    }

    // --- unknown: List<KlvUnknownField> with is_st0601_typed_tag collision-drop ---
    {
        let unk_obj = env
            .call_method(rec, "unknown", "()Ljava/util/List;", &[])?
            .l()?;
        r.unknown = read_unknown_list(env, &unk_obj, is_st0601_typed_tag)?;
    }

    // --- sentinelTags: List<Long> → Vec<u32> ---
    {
        let st_obj = env
            .call_method(rec, "sentinelTags", "()Ljava/util/List;", &[])?
            .l()?;
        let size = env.call_method(&st_obj, "size", "()I", &[])?.i()?;
        for i in 0..size {
            let item = env
                .call_method(&st_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            let v = env.call_method(&item, "longValue", "()J", &[])?.j()?;
            match u32::try_from(v) {
                Ok(tag) => r.sentinel_tags.push(tag),
                Err(_) => {
                    let _ = env.throw_new(
                        "java/lang/IllegalArgumentException",
                        format!("sentinelTags entry out of u32 range: {v}"),
                    );
                    return Err(jni::errors::Error::JavaException);
                }
            }
        }
    }

    // --- imapbSpecials: List<ImapbSpecialEntry> → Vec<(u32, ImapbSpecial)> ---
    {
        let is_obj = env
            .call_method(rec, "imapbSpecials", "()Ljava/util/List;", &[])?
            .l()?;
        let size = env.call_method(&is_obj, "size", "()I", &[])?.i()?;
        for i in 0..size {
            let item = env
                .call_method(&is_obj, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            let tag_i = env.call_method(&item, "tag", "()I", &[])?.i()?;
            let tag = match u32::try_from(tag_i) {
                Ok(t) => t,
                Err(_) => {
                    let _ = env.throw_new(
                        "java/lang/IllegalArgumentException",
                        format!("imapbSpecials entry tag out of u32 range: {tag_i}"),
                    );
                    return Err(jni::errors::Error::JavaException);
                }
            };
            let code_obj = env
                .call_method(&item, "code", "()Ljava/lang/String;", &[])?
                .l()?;
            let j_str: &jni::objects::JString = (&code_obj).into();
            let code: String = env.get_string(j_str).map(Into::into)?;
            let payload_j = env.call_method(&item, "payload", "()J", &[])?.j()?;
            let special = imapb_special_from_code(env, &code, payload_j as u64)?;
            r.imapb_specials.push((tag, special));
        }
    }

    // field_errors is a decoder-only diagnostic; not round-tripped (mirrors tst-py).
    Ok(r)
}

/// Thin public wrapper so `st1204::validateMismmsNative` can call the shared
/// reader without duplicating it. All the real logic lives in `read_uas_datalink`.
pub fn read_uas_datalink_for_validate(
    env: &mut JNIEnv<'_>,
    rec: &JObject<'_>,
) -> jni::errors::Result<UasDatalinkLs> {
    read_uas_datalink(env, rec)
}

// ---------------------------------------------------------------------------
// klv::st1010 SDCC-FLP — general-purpose, standalone entry points. NOT
// ST 0601-specific (see the `SdccFlp`/`SdccFlpField` module docs) — kept in
// this file per the WP-C task brief rather than a new module, since the only
// caller-visible surface is `Klv.decodeSdccFlp`/`Klv.encodeSdccFlpMode2`.
// ---------------------------------------------------------------------------

/// Map a standalone Rust `KlvFieldError` — e.g. from `decode_sdcc_flp`, which
/// returns one directly rather than embedding it in a `KlvDecodeError` — to a
/// `KlvDecodeException`. Mirrors `map_klv_decode_error`'s
/// `FieldError(_) => "MALFORMED_BYTES"` bucket: this function's caller sees
/// exactly one field-level failure with no surrounding local-set context, so
/// every variant folds to that bucket except the substrate-framing one.
/// Mirrors tst-py's `klv_field_error_to_pyerr`.
fn map_sdcc_field_error(env: &mut JNIEnv, e: &KlvFieldError) {
    let msg = e.to_string();
    let kind = match e {
        KlvFieldError::TruncatedField { .. } => BindingErrorKind::KlvDecodeTruncatedSet,
        _ => BindingErrorKind::KlvDecodeMalformedBytes,
    };
    throw_klv_decode(env, kind, &msg);
}

/// Build a `org.tstrans.klv.SdccFlp` Java record from a decoded Rust `SdccFlp`.
fn build_sdcc_flp(env: &mut JNIEnv<'_>, m: &SdccFlp) -> jni::errors::Result<jobject> {
    let std_devs_arr = env.new_double_array(m.std_devs.len() as i32)?;
    env.set_double_array_region(&std_devs_arr, 0, &m.std_devs)?;
    let correlations_arr = env.new_double_array(m.correlations.len() as i32)?;
    env.set_double_array_region(&correlations_arr, 0, &m.correlations)?;
    let presence: Vec<u8> = m.correlation_present.iter().map(|&b| u8::from(b)).collect();
    let presence_arr = env.new_boolean_array(presence.len() as i32)?;
    env.set_boolean_array_region(&presence_arr, 0, &presence)?;
    let obj = env.new_object(
        "org/tstrans/klv/SdccFlp",
        "(J[D[D[Z)V",
        &[
            JValue::Long(m.matrix_size as i64),
            JValue::Object(&std_devs_arr),
            JValue::Object(&correlations_arr),
            JValue::Object(&presence_arr),
        ],
    )?;
    Ok(obj.into_raw())
}

/// Read a Java `double[]` into a `Vec<f64>`.
fn read_double_array(
    env: &mut JNIEnv,
    arr: &jni::objects::JDoubleArray,
) -> jni::errors::Result<Vec<f64>> {
    let len = env.get_array_length(arr)?;
    let mut buf = vec![0.0f64; len as usize];
    env.get_double_array_region(arr, 0, &mut buf)?;
    Ok(buf)
}

/// `org.tstrans.klv.Klv.decodeSdccFlpNative(byte[]) -> SdccFlp`
///
/// Decodes a MISB ST 1010.3 SDCC-FLP pack (Mode 1 and Mode 2). `buf` is the
/// pack bytes starting at Element 1 (Matrix Size) — no outer TLV framing and
/// no leading Universal Label. General-purpose: not ST 0601-specific.
/// Mirrors tst-py's `decode_sdcc_flp(buf)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_decodeSdccFlpNative<'local>(
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
                    format!("decodeSdccFlpNative: byte[] read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match decode_sdcc_flp(&bytes) {
            Ok(m) => build_sdcc_flp(env, &m).unwrap_or_else(|_| JObject::null().into_raw()),
            Err(e) => {
                map_sdcc_field_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// `org.tstrans.klv.Klv.encodeSdccFlpMode2Native(double[], double[], int) -> byte[]`
///
/// Encodes a Mode-2 SDCC-FLP: standard deviations as IEEE binary32,
/// correlations as ST 1201 IMAPB(-1, 1, `clen`). Sparse mode + Bit Vector are
/// chosen automatically when zero-correlations make it pay. Mirrors tst-py's
/// `encode_sdcc_flp_mode2(std_devs, correlations, clen)`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_klv_Klv_encodeSdccFlpMode2Native<'local>(
    mut env: JNIEnv<'local>,
    _c: JClass<'local>,
    std_devs: jni::objects::JDoubleArray<'local>,
    correlations: jni::objects::JDoubleArray<'local>,
    clen: jint,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let std_devs_vec = match read_double_array(env, &std_devs) {
            Ok(v) => v,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("encodeSdccFlpMode2Native: stdDevs read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        let correlations_vec = match read_double_array(env, &correlations) {
            Ok(v) => v,
            Err(e) => {
                let _ = env.throw_new(
                    "java/lang/RuntimeException",
                    format!("encodeSdccFlpMode2Native: correlations read failed: {e}"),
                );
                return JObject::null().into_raw();
            }
        };
        match encode_sdcc_flp_mode2(&std_devs_vec, &correlations_vec, clen as usize) {
            Ok(bytes) => match env.byte_array_from_slice(&bytes) {
                Ok(arr) => arr.into_raw(),
                Err(e) => {
                    let _ = env.throw_new(
                        "java/lang/RuntimeException",
                        format!("encodeSdccFlpMode2Native: byte_array_from_slice failed: {e}"),
                    );
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                map_klv_encode_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

#[cfg(test)]
mod wire_inventory {
    //! Every tst-core wire-code variant crosses the JNI boundary as the
    //! codepoint tst-core itself defines, for every variant in `ALL`
    //! (Arc 2 R2 — replaces the hand-copied tables and their unreachable
    //! wildcard arms). The compile-time pin is tst-core's `variant_inventory`;
    //! this is
    //! the runtime half, which is all a binding crate can have (matching a
    //! foreign `#[non_exhaustive]` enum without a wildcard is E0004).
    use tst_core::klv::st0601::{
        IcingDetected, OperationalMode, PayloadType, PlatformStatus, SensorControlMode,
        SensorFovName,
    };

    #[test]
    fn every_st0601_coded_enum_variant_round_trips_through_the_jni_codepoint() {
        for v in IcingDetected::ALL {
            assert_eq!(
                super::icing_detected_from_code(super::icing_detected_to_code(*v)),
                *v
            );
        }
        for v in SensorFovName::ALL {
            assert_eq!(
                super::sensor_fov_name_from_code(super::sensor_fov_name_to_code(*v)),
                *v
            );
        }
        for v in OperationalMode::ALL {
            assert_eq!(
                super::operational_mode_from_code(super::operational_mode_to_code(*v)),
                *v
            );
        }
        for v in PlatformStatus::ALL {
            assert_eq!(
                super::platform_status_from_code(super::platform_status_to_code(*v)),
                *v
            );
        }
        for v in SensorControlMode::ALL {
            assert_eq!(
                super::sensor_control_mode_from_code(super::sensor_control_mode_to_code(*v)),
                *v
            );
        }
        for v in PayloadType::ALL {
            assert_eq!(
                super::payload_type_from_code(super::payload_type_to_code(*v)),
                *v
            );
        }
    }

    /// The JNI codepoint IS tst-core's wire codepoint — not merely a
    /// self-consistent local table. Without this a pair of mirrored local
    /// tables could agree with each other and disagree with the wire.
    #[test]
    fn the_jni_codepoint_is_tst_cores_wire_codepoint() {
        for v in IcingDetected::ALL {
            assert_eq!(super::icing_detected_to_code(*v), v.to_wire());
        }
        for v in SensorFovName::ALL {
            assert_eq!(super::sensor_fov_name_to_code(*v), v.to_wire());
        }
        for v in OperationalMode::ALL {
            assert_eq!(super::operational_mode_to_code(*v), v.to_wire());
        }
        for v in PlatformStatus::ALL {
            assert_eq!(super::platform_status_to_code(*v), v.to_wire());
        }
        for v in SensorControlMode::ALL {
            assert_eq!(super::sensor_control_mode_to_code(*v), v.to_wire());
        }
        for v in PayloadType::ALL {
            assert_eq!(super::payload_type_to_code(*v), v.to_wire());
        }
    }
}
