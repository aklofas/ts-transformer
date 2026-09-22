//! PyO3 wrappers for `tst_core::klv::*` typed sets.
//!
//! Translation strategy: each Rust `*Ls` / `*Pack` struct is converted
//! to an instance of a Python-side dataclass under `tstrans.klv.*` via
//! per-set translator functions (`convert_uas_datalink`, etc.). Decode
//! entry points are `#[pyfunction]`s that map `KlvDecodeError` to
//! `tstrans.exceptions.KlvError` via `make_klv_error`.
//!
//! Covers ST 0601 / ST 0102 / ST 0605 / ST 0903 decode and encode
//! with field-error surfacing on the decode path.
//!
//! `#![allow(...)]` mirrors the pattern in `errors.rs` and `mpegts.rs` —
//! PyO3 0.22 + Rust 2024 macro expansions trip these lints. Hand-
//! written code in this module has no unsafe blocks.
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use tst_core::error::CotError;
use tst_core::error::KlvDecodeError;
use tst_core::error::KlvFieldError as RustKlvFieldError;
use tst_core::error::KlvPatchError;
use tst_core::klv::ImapbSpecial as RustImapbSpecial;
use tst_core::klv::UniversalLabel;
use tst_core::klv::pack::OwnedRawField;
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod as RustClsCountry, ObjectCountryCodingMethod as RustObjCountry,
    SecurityClassification as RustSecCls, SecurityLs, decode as decode_st0102_lenient,
    decode_strict as decode_st0102_strict,
    encode_strict_compliance as encode_st0102_strict_compliance, encode_to_vec as encode_st0102,
};
use tst_core::klv::st0601::{
    AirbaseLocations as RustAirbaseLocations, ControlCommand as RustControlCommand,
    CountryCodes as RustCountryCodes, EncodeConfig as St0601EncodeConfig,
    IcingDetected as RustIcingDetected, ImageHorizonPixels as RustImageHorizonPixels,
    Location as RustLocation, MetadataSubstreamId as RustMetadataSubstreamId,
    MismmsViolation as RustMismmsViolation, OperationalMode as RustOperationalMode,
    OutOfRangePolicy as RustOutOfRangePolicy, PayloadList as RustPayloadList,
    PayloadRecord as RustPayloadRecord, PayloadType as RustPayloadType,
    PlatformStatus as RustPlatformStatus, SdccFlpField as RustSdccFlpField,
    SensorControlMode as RustSensorControlMode, SensorFovName as RustSensorFovName,
    SensorFrameRate as RustSensorFrameRate, St0601SentinelMeaning, UasDatalinkLs,
    ViewDomain as RustViewDomain, ViewDomainPair as RustViewDomainPair,
    WavelengthRecord as RustWavelengthRecord, Waypoint as RustWaypoint,
    WeaponsStore as RustWeaponsStore, decode as decode_st0601_lenient,
    decode_strict as decode_st0601_strict,
    decode_strict_compliance as decode_st0601_strict_compliance,
    encode_strict_compliance as encode_st0601_strict_compliance,
    encode_to_vec_with as encode_st0601_with, patch as patch_st0601,
    st0601_sentinel_meaning as rust_st0601_sentinel_meaning,
    validate_mismms as rust_validate_mismms,
};
use tst_core::klv::st0605::{
    PrecisionTimeStampPack, TimeStatus as RustTimeStatus, decode as decode_st0605,
    encode as encode_st0605,
};
use tst_core::klv::st0805::{
    CotConfig as RustCotConfig, platform_position_xml as st0805_platform_position_xml,
    platform_uid as st0805_platform_uid,
    sensor_point_of_interest_xml as st0805_sensor_point_of_interest_xml, spi_uid as st0805_spi_uid,
};
use tst_core::klv::st0806::{
    RvtAoi as RustRvtAoi, RvtAoiType as RustRvtAoiType, RvtLs as RustRvtLs, RvtPoi as RustRvtPoi,
    RvtPoiType as RustRvtPoiType, RvtUserData as RustRvtUserData, decode as decode_st0806,
    decode_standalone as decode_st0806_standalone, encode_to_vec as encode_st0806,
    encode_to_vec_standalone as encode_st0806_standalone,
};
use tst_core::klv::st0903::{
    VTargetPack as RustVTargetPack, VmtiLs, decode as decode_st0903_lenient,
    decode_strict as decode_st0903_strict,
    encode_standalone_strict_compliance as encode_st0903_standalone_strict_compliance,
    encode_strict_compliance as encode_st0903_strict_compliance, encode_to_vec as encode_st0903,
    encode_to_vec_standalone as encode_st0903_standalone,
};
use tst_core::klv::st1010::{
    SdccFlp as RustSdccFlp, decode_sdcc_flp as decode_st1010_sdcc_flp,
    encode_sdcc_flp_mode2 as encode_st1010_sdcc_flp_mode2,
};
use tst_core::klv::st1204::{
    CoreId as RustCoreId, IdType as RustIdType, St1204Error, decode as decode_st1204,
    encode_to_vec as encode_st1204,
};

use crate::errors::{klv_encode_error_to_pyerr, make_klv_error};

// ---------------------------------------------------------------------------
// KlvDecodeError → KlvError mapping
// ---------------------------------------------------------------------------

/// Map a Rust `KlvDecodeError` to a Python `KlvError` instance. Covers
/// every Rust variant; `KlvDecodeError` carries the non-exhaustive
/// attribute so the default arm routes to `INTERNAL` and we'll add new
/// explicit arms as new Rust variants surface.
pub(crate) fn klv_decode_error_to_pyerr(py: Python<'_>, e: KlvDecodeError) -> PyErr {
    let msg = format!("{e}");
    let kind = tst_pipeline::binding::kind::kind_of_klv_decode(&e).name();
    make_klv_error(py, kind, &msg)
}

// ---------------------------------------------------------------------------
// Standalone KlvFieldError → KlvError mapping
// ---------------------------------------------------------------------------

/// Map a standalone Rust `KlvFieldError` — e.g. from `decode_sdcc_flp`,
/// which returns one directly rather than embedding it in a
/// `KlvDecodeError` — to a Python `KlvError` instance. Mirrors
/// `klv_decode_error_to_pyerr`'s `FieldError(_) => "MALFORMED_BYTES"`
/// bucket: this function's caller sees exactly one field-level failure
/// with no surrounding local-set context, so every variant folds to
/// that same bucket except the substrate-framing one.
fn klv_field_error_to_pyerr(py: Python<'_>, e: RustKlvFieldError) -> PyErr {
    let msg = format!("{e}");
    let kind = tst_pipeline::binding::kind::kind_of_klv_field(&e).name();
    make_klv_error(py, kind, &msg)
}

// ---------------------------------------------------------------------------
// St1204Error → KlvError mapping
// ---------------------------------------------------------------------------

/// Map a Rust `St1204Error` to a Python `KlvError` instance.
fn st1204_error_to_pyerr(py: Python<'_>, e: St1204Error) -> PyErr {
    let msg = format!("{e}");
    let kind = match &e {
        St1204Error::Truncated => "TRUNCATED_SET",
        St1204Error::UnsupportedVersion(_) => "MALFORMED_BYTES",
        St1204Error::ReservedBitsSet => "MALFORMED_BYTES",
        St1204Error::InvalidUsage => "MALFORMED_BYTES",
        St1204Error::TrailingBytes => "MALFORMED_BYTES",
        _ => "INTERNAL",
    };
    make_klv_error(py, kind, &msg)
}

// ---------------------------------------------------------------------------
// ST 0605 — Precision Time Stamp Pack
// ---------------------------------------------------------------------------

/// Translate a Rust `PrecisionTimeStampPack` to a Python
/// `tstrans.klv.PrecisionTimeStampPack` dataclass instance.
fn convert_precision_timestamp_pack(
    py: Python<'_>,
    pack: &PrecisionTimeStampPack,
) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let time_status_cls = klv_mod.getattr(intern!(py, "TimeStatus"))?;
    let pack_cls = klv_mod.getattr(intern!(py, "PrecisionTimeStampPack"))?;

    let ts_kwargs = PyDict::new_bound(py);
    ts_kwargs.set_item("raw", pack.time_status.0)?;
    let time_status_py = time_status_cls.call((), Some(&ts_kwargs))?;

    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("time_status", time_status_py)?;
    kwargs.set_item("timestamp_us", pack.timestamp_us)?;
    Ok(pack_cls.call((), Some(&kwargs))?.unbind())
}

/// Decode an ST 0605 §7 Precision Time Stamp Pack. `buf` is the full
/// 26-byte wire-format pack (16-byte UL + 1-byte BER length + 1-byte
/// TimeStatus + 8-byte BE microsecond timestamp).
#[pyfunction]
#[pyo3(name = "decode_precision_timestamp")]
fn decode_precision_timestamp_py(py: Python<'_>, buf: &[u8]) -> PyResult<PyObject> {
    // Audit #11 / GIL-release decision: NOT wrapped in `py.allow_threads`.
    // The Rust `decode_st0605` call is ~1us for the 26-byte spec pack;
    // GIL transition overhead exceeds the decode time for any realistic
    // payload size at this entry point. See `bindings/python/tests/
    // test_gil_release.py` and the workspace reference memo
    // `reference_pyo3_allow_threads_pattern.md` for the empirical
    // breakeven point (~50us per call).
    match decode_st0605(buf) {
        Ok(pack) => convert_precision_timestamp_pack(py, &pack),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers — KlvFieldError + OwnedRawField translators
// ---------------------------------------------------------------------------

/// Translate a Rust `KlvFieldError` to a Python `KlvFieldError` dataclass
/// instance. Variant→`KlvFieldErrorKind` mapping is exhaustive over the
/// current Rust enum; the wildcard arm covers future non-exhaustive
/// attribute additions by routing to `INVALID_LENGTH` (consumers should
/// treat unrecognized kinds as best-effort diagnostic only).
fn convert_field_error(py: Python<'_>, fe: &RustKlvFieldError) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let kind_enum = klv_mod.getattr(intern!(py, "KlvFieldErrorKind"))?;
    let cls = klv_mod.getattr(intern!(py, "KlvFieldError"))?;
    let (variant, tag): (&str, u32) = match fe {
        RustKlvFieldError::OutOfRange { tag, .. } => ("OUT_OF_RANGE", *tag),
        RustKlvFieldError::InvalidUtf8 { tag } => ("INVALID_UTF8", *tag),
        RustKlvFieldError::InvalidLength { tag, .. } => ("INVALID_LENGTH", *tag),
        RustKlvFieldError::InvalidUtf16 { tag } => ("INVALID_UTF16", *tag),
        RustKlvFieldError::InvalidCodepoint { tag, .. } => ("INVALID_CODEPOINT", *tag),
        RustKlvFieldError::TruncatedField { tag } => ("TRUNCATED_FIELD", *tag),
        RustKlvFieldError::UnsupportedImapbLength { .. } => ("UNSUPPORTED_IMAPB_LENGTH", 0),
        RustKlvFieldError::InvalidImapbParams { .. } => ("INVALID_IMAPB_PARAMS", 0),
        _ => ("INVALID_LENGTH", 0),
    };
    let kind_value = kind_enum.getattr(variant)?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("kind", kind_value)?;
    kwargs.set_item("tag", tag)?;
    kwargs.set_item("message", format!("{fe}"))?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Translate a list of `KlvFieldError`s to a Python tuple of
/// `KlvFieldError` dataclass instances.
fn convert_field_errors(py: Python<'_>, errs: &[RustKlvFieldError]) -> PyResult<PyObject> {
    let items: Vec<PyObject> = errs
        .iter()
        .map(|e| convert_field_error(py, e))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyTuple::new_bound(py, items).unbind().into())
}

/// Translate a list of `OwnedRawField` to a Python tuple of `(tag, bytes)`
/// 2-tuples. The shape matches `SecurityLs.unknown` / `UasDatalinkLs.unknown`
/// in the Python typed-set dataclasses.
fn convert_unknown(py: Python<'_>, unknown: &[OwnedRawField]) -> PyResult<PyObject> {
    let items: Vec<PyObject> = unknown
        .iter()
        .map(|f| {
            pyo3::types::PyTuple::new_bound(
                py,
                &[
                    f.tag.into_py(py),
                    pyo3::types::PyBytes::new_bound(py, &f.value).into_py(py),
                ],
            )
            .unbind()
            .into()
        })
        .collect();
    Ok(pyo3::types::PyTuple::new_bound(py, items).unbind().into())
}

/// Inverse of `convert_unknown`: extracts the `unknown` field from a
/// Python typed-set dataclass into a `Vec<OwnedRawField>` for the
/// Rust struct. Each entry must be a 2-tuple `(int, bytes)`; malformed
/// shapes raise `TypeError` / `ValueError` rather than silently
/// corrupting the Rust side (audit #6's "validate-don't-drop" stance).
///
/// `is_typed_tag` is the per-set predicate identifying tags the
/// encoder's typed table covers. When a Python-supplied `unknown` entry
/// collides with a typed tag, the entry is silently dropped — the typed
/// field wins. Three reasons:
///
/// 1. Real decode never produces such an entry (the decoder routes
///    typed tags to typed fields, not to `unknown`), so this only
///    affects user-hand-constructed records.
/// 2. ST 0601's encoder errors with `ReservedTagInUnknown` on the same
///    collision pattern; filtering here keeps the four sets consistent
///    (the others' encoders would otherwise emit duplicate TLVs).
/// 3. "Drop on collision" produces deterministic, valid wire output
///    rather than failing the round-trip — matches the audit #5
///    "deterministic precedence" requirement.
fn py_to_unknown(
    p: &Bound<'_, PyAny>,
    is_typed_tag: impl Fn(u32) -> bool,
) -> PyResult<Vec<OwnedRawField>> {
    let py = p.py();
    let unknown_obj = p.getattr(intern!(py, "unknown"))?;
    let mut out = Vec::new();
    for item in unknown_obj.iter()? {
        let item = item?;
        // Each entry must be a 2-tuple. We rely on tuple-shaped extraction
        // via `(u32, Vec<u8>)` — PyO3 enforces the 2-arity + element types.
        let (tag, value): (u32, Vec<u8>) = item.extract().map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "unknown TLV must be a (int, bytes) 2-tuple: {e}"
            ))
        })?;
        if is_typed_tag(tag) {
            // Collision: typed field wins; drop the unknown entry.
            continue;
        }
        out.push(OwnedRawField { tag, value });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-set typed-tag predicates — narrow inventories duplicated locally
// (a few u8 constants) rather than threading internal Rust APIs out of
// `tst-core`. Each predicate mirrors the typed inventory listed in the
// corresponding `encode.rs`.
// ---------------------------------------------------------------------------

/// ST 0102 LS typed tags: 1..=14 + 22 + 23 + 24.
fn is_st0102_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=14 | 22 | 23 | 24)
}

/// ST 0601 LS typed + reserved tags — mirrors `tags::TAGS` in
/// `crates/tst-core/src/klv/st0601/tags.rs` (142 entries as of WP-C:
/// 1-65, 67-143). Tag 66 is the deprecated placeholder (permanently
/// untyped by design — ST 0601.19 §8.66: "This item has been
/// Deprecated") and 144..=255 are forward-compat; both may legitimately
/// appear in `unknown` (66 and 200 are the durable unknown-tag test
/// stand-ins used across this suite — never add them here). WP-C
/// (Table C1) finished the sweep from 66's neighbors through 143,
/// including Tag 102 (MULTI-INSTANCE SDCC-FLP, now typed via
/// `sdcc_flps`) and Tag 115 (MULTI-INSTANCE Control Command, now typed
/// via `control_commands`) — keep this in sync with `tags::TAGS` when
/// new tags are typed, or a caller-supplied `unknown` entry for a
/// newly-typed tag will slip past this filter and get rejected
/// downstream by the real Rust encoder's own (stricter, canonical)
/// check instead of being silently dropped here per the documented
/// "typed wins" collision policy.
fn is_st0601_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=65 | 67..=143)
}

/// ST 0903.6 VMTI LS typed tags: 1 (Checksum), 2..=13, 101..=103.
/// Tag 7 is deprecated in v6; treat it as typed so a colliding user
/// `unknown` entry doesn't sneak it back in.
fn is_st0903_vmti_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=13 | 101..=103)
}

/// ST 0903.6 VTargetPack typed tags: 1..=23 + 100..=107.
fn is_st0903_vtarget_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=23 | 100..=107)
}

/// ST 0806.4 RVT LS top-level typed tags: 1..=21 (Table 8-1 — no gaps;
/// mirrors `crates/tst-core/src/klv/st0806/tags.rs::RVT_TAGS`).
fn is_st0806_rvt_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=21)
}

/// ST 0806.4 POI (Table 8-2) / AOI (Table 8-3) typed tags — both nested
/// sets share the same 1..=10 tag universe, so one predicate covers both.
fn is_st0806_poi_aoi_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=10)
}

// ---------------------------------------------------------------------------
// ST 0102 — Security Metadata LS
// ---------------------------------------------------------------------------

fn convert_security_classification(py: Python<'_>, v: RustSecCls) -> PyResult<PyObject> {
    match v {
        RustSecCls::Unknown(b) => Ok(b.into_py(py)),
        known => {
            let klv_mod = py.import_bound("tstrans.klv")?;
            let cls = klv_mod.getattr(intern!(py, "SecurityClassification"))?;
            Ok(cls.call1((known.to_u8(),))?.unbind())
        }
    }
}

fn convert_classifying_country(py: Python<'_>, v: RustClsCountry) -> PyResult<PyObject> {
    match v {
        RustClsCountry::Unknown(b) => Ok(b.into_py(py)),
        known => {
            let klv_mod = py.import_bound("tstrans.klv")?;
            let cls = klv_mod.getattr(intern!(py, "ClassifyingCountryCodingMethod"))?;
            Ok(cls.call1((known.to_u8(),))?.unbind())
        }
    }
}

fn convert_object_country(py: Python<'_>, v: RustObjCountry) -> PyResult<PyObject> {
    match v {
        RustObjCountry::Unknown(b) => Ok(b.into_py(py)),
        known => {
            let klv_mod = py.import_bound("tstrans.klv")?;
            let cls = klv_mod.getattr(intern!(py, "ObjectCountryCodingMethod"))?;
            Ok(cls.call1((known.to_u8(),))?.unbind())
        }
    }
}

fn convert_security_ls(py: Python<'_>, sec: &SecurityLs) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SecurityLs"))?;
    let kwargs = PyDict::new_bound(py);

    if let Some(v) = sec.security_classification {
        kwargs.set_item(
            "security_classification",
            convert_security_classification(py, v)?,
        )?;
    }
    if let Some(v) = sec.classifying_country_coding_method {
        kwargs.set_item(
            "classifying_country_coding_method",
            convert_classifying_country(py, v)?,
        )?;
    }
    if let Some(v) = &sec.classifying_country {
        kwargs.set_item("classifying_country", v.as_str())?;
    }
    if let Some(v) = sec.object_country_coding_method {
        kwargs.set_item(
            "object_country_coding_method",
            convert_object_country(py, v)?,
        )?;
    }
    if let Some(v) = &sec.object_country_codes {
        kwargs.set_item("object_country_codes", v.as_str())?;
    }
    if let Some(v) = sec.version {
        kwargs.set_item("version", v)?;
    }
    if let Some(v) = &sec.sci_shi_info {
        kwargs.set_item("sci_shi_info", v.as_str())?;
    }
    if let Some(v) = &sec.caveats {
        kwargs.set_item("caveats", v.as_str())?;
    }
    if let Some(v) = &sec.releasing_instructions {
        kwargs.set_item("releasing_instructions", v.as_str())?;
    }
    if let Some(v) = &sec.classified_by {
        kwargs.set_item("classified_by", v.as_str())?;
    }
    if let Some(v) = &sec.derived_from {
        kwargs.set_item("derived_from", v.as_str())?;
    }
    if let Some(v) = &sec.classification_reason {
        kwargs.set_item("classification_reason", v.as_str())?;
    }
    if let Some(v) = &sec.declassification_date {
        kwargs.set_item("declassification_date", v.as_str())?;
    }
    if let Some(v) = &sec.classification_marking_system {
        kwargs.set_item("classification_marking_system", v.as_str())?;
    }
    if let Some(v) = &sec.classification_comments {
        kwargs.set_item("classification_comments", v.as_str())?;
    }
    if let Some(v) = &sec.classifying_country_coding_method_version_date {
        kwargs.set_item("classifying_country_coding_method_version_date", v.as_str())?;
    }
    if let Some(v) = &sec.object_country_coding_method_version_date {
        kwargs.set_item("object_country_coding_method_version_date", v.as_str())?;
    }
    kwargs.set_item("unknown", convert_unknown(py, &sec.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &sec.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Decode an ST 0102 Security LS. `buf` is **body-only** (no UL / outer
/// BER length wrapper). With `strict=True`, rejects missing required
/// tags + unknown codepoints + non-canonical BER + malformed UTF-16.
#[pyfunction]
#[pyo3(name = "decode_security", signature = (buf, *, strict = false))]
fn decode_security_py(py: Python<'_>, buf: &[u8], strict: bool) -> PyResult<PyObject> {
    // Audit #11 / GIL-release decision: NOT wrapped — same rationale as
    // `decode_precision_timestamp_py`. ST 0102 records are typically
    // 20-200 bytes, well under the GIL-transition breakeven.
    let result = if strict {
        decode_st0102_strict(buf)
    } else {
        decode_st0102_lenient(buf)
    };
    match result {
        Ok(sec) => convert_security_ls(py, &sec),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// ST 0102 — Python → Rust inverse translator + encode entry point
// ---------------------------------------------------------------------------

/// Extract a u8 codepoint from a Python field that may be either an
/// `enum.Enum` instance (with `.value: int`) or a raw `int` (Unknown
/// codepoint pass-through). Mirrors the asymmetric forward translator
/// (`convert_security_classification` / `convert_classifying_country` /
/// `convert_object_country`) which emits a typed enum for known
/// codepoints and a raw int for `Unknown(b)`.
fn enum_field_to_u8(p: &Bound<'_, PyAny>) -> PyResult<u8> {
    if let Ok(value_attr) = p.getattr("value") {
        // enum.Enum instance — pull `.value`
        value_attr.extract()
    } else {
        // raw int — extract directly
        p.extract()
    }
}

/// Sibling of [`enum_field_to_u8`] for a coded enum whose wire codepoint
/// is a BER-OID (unbounded, not a narrow bitfield) rather than a single
/// byte — only `PayloadType` (Item 138) needs this width so far.
fn enum_field_to_u64(p: &Bound<'_, PyAny>) -> PyResult<u64> {
    if let Ok(value_attr) = p.getattr("value") {
        value_attr.extract()
    } else {
        p.extract()
    }
}

/// Inverse of `convert_security_ls`: extracts every field from a Python
/// `tstrans.klv.SecurityLs` dataclass into a Rust `SecurityLs`.
///
/// `field_errors` is a parser-only diagnostic and is not round-tripped.
///
/// `unknown` IS round-tripped: forward-compat TLVs the decoder preserved
/// are forwarded into the encoder so `decode -> encode -> decode` is
/// lossless (audit #5). Entries whose tag collides with a typed field
/// (see `is_st0102_typed_tag`) are silently dropped — typed wins.
fn py_to_security_ls(p: &Bound<'_, PyAny>) -> PyResult<SecurityLs> {
    let mut r = SecurityLs::default();
    let py = p.py();

    // 3 typed-enum fields: Python sends either an enum instance OR a raw int
    // (Unknown codepoint). Use enum_field_to_u8 to coerce both shapes.
    let sc_obj = p.getattr(intern!(py, "security_classification"))?;
    if !sc_obj.is_none() {
        r.security_classification = Some(RustSecCls::from_u8(enum_field_to_u8(&sc_obj)?));
    }
    let cc_obj = p.getattr(intern!(py, "classifying_country_coding_method"))?;
    if !cc_obj.is_none() {
        r.classifying_country_coding_method =
            Some(RustClsCountry::from_u8(enum_field_to_u8(&cc_obj)?));
    }
    let oc_obj = p.getattr(intern!(py, "object_country_coding_method"))?;
    if !oc_obj.is_none() {
        r.object_country_coding_method = Some(RustObjCountry::from_u8(enum_field_to_u8(&oc_obj)?));
    }

    // Optional<String> fields (15 of them) + Optional<u16> for version.
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }
    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }

    os!(classifying_country);
    os!(object_country_codes);
    op!(version, u16);
    os!(sci_shi_info);
    os!(caveats);
    os!(releasing_instructions);
    os!(classified_by);
    os!(derived_from);
    os!(classification_reason);
    os!(declassification_date);
    os!(classification_marking_system);
    os!(classification_comments);
    os!(classifying_country_coding_method_version_date);
    os!(object_country_coding_method_version_date);

    r.unknown = py_to_unknown(p, is_st0102_typed_tag)?;

    Ok(r)
}

/// Encode a Python `SecurityLs` to wire bytes (lenient — emits only the
/// populated fields, no mandatory-tag enforcement). Returns `bytes`
/// containing the ST 0102 body (no outer UL / BER length wrapper).
#[pyfunction]
#[pyo3(name = "encode_security")]
fn encode_security_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_security_ls(record)?;
    let bytes = encode_st0102(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0605 — Python → Rust inverse translator + encode entry point
// ---------------------------------------------------------------------------

/// Inverse of `convert_precision_timestamp_pack`.
fn py_to_precision_timestamp_pack(p: &Bound<'_, PyAny>) -> PyResult<PrecisionTimeStampPack> {
    let py = p.py();
    let ts_obj = p.getattr(intern!(py, "time_status"))?;
    let raw: u8 = ts_obj.getattr(intern!(py, "raw"))?.extract()?;
    let timestamp_us: u64 = p.getattr(intern!(py, "timestamp_us"))?.extract()?;
    Ok(PrecisionTimeStampPack {
        time_status: RustTimeStatus(raw),
        timestamp_us,
    })
}

/// Encode a Python `PrecisionTimeStampPack` to the 26-byte wire form
/// (16-byte UL + 1-byte BER length + 1-byte TimeStatus + 8-byte BE
/// microsecond timestamp). Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_precision_timestamp")]
fn encode_precision_timestamp_py(py: Python<'_>, pack: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_pack = py_to_precision_timestamp_pack(pack)?;
    let bytes = encode_st0605(&rust_pack);
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0903 — VTargetPack translator (entry points further down)
// ---------------------------------------------------------------------------

/// Translate a Rust `VTargetPack` to a Python `tstrans.klv.VTargetPack`
/// dataclass instance.
fn convert_vtarget_pack(py: Python<'_>, p: &RustVTargetPack) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "VTargetPack"))?;
    let kwargs = PyDict::new_bound(py);

    kwargs.set_item("target_id", p.target_id)?;

    macro_rules! opt_set {
        ($key:expr, $val:expr) => {
            if let Some(v) = $val {
                kwargs.set_item($key, v)?;
            }
        };
    }
    // Optional<Vec<u8>> — emit as Python `bytes` (not list[int]); the
    // Python dataclass fields are typed `bytes | None`. Matches the
    // `ob!` macro in `convert_uas_datalink_ls`.
    macro_rules! opt_set_ref {
        ($key:expr, $val:expr) => {
            if let Some(v) = $val.as_ref() {
                kwargs.set_item($key, pyo3::types::PyBytes::new_bound(py, v.as_slice()))?;
            }
        };
    }

    opt_set!("centroid_pixel", p.centroid_pixel);
    opt_set!("bbox_top_left_pixel", p.bbox_top_left_pixel);
    opt_set!("bbox_bottom_right_pixel", p.bbox_bottom_right_pixel);
    opt_set!("priority", p.priority);
    opt_set!("confidence_level", p.confidence_level);
    opt_set!("history", p.history);
    opt_set!("percentage_of_target_pixels", p.percentage_of_target_pixels);

    if let Some([r, g, b]) = p.target_color {
        kwargs.set_item(
            "target_color",
            pyo3::types::PyTuple::new_bound(py, [r, g, b]),
        )?;
    }

    opt_set!("target_intensity", p.target_intensity);
    opt_set!("centroid_lat_offset", p.centroid_lat_offset);
    opt_set!("centroid_lon_offset", p.centroid_lon_offset);
    opt_set!("centroid_hae", p.centroid_hae);
    opt_set!("bbox_top_left_lat_offset", p.bbox_top_left_lat_offset);
    opt_set!("bbox_top_left_lon_offset", p.bbox_top_left_lon_offset);
    opt_set!(
        "bbox_bottom_right_lat_offset",
        p.bbox_bottom_right_lat_offset
    );
    opt_set!(
        "bbox_bottom_right_lon_offset",
        p.bbox_bottom_right_lon_offset
    );
    opt_set_ref!("target_location", p.target_location);
    opt_set_ref!("geospatial_contour_series", p.geospatial_contour_series);
    opt_set!("centroid_pix_row", p.centroid_pix_row);
    opt_set!("centroid_pix_col", p.centroid_pix_col);
    opt_set!("algorithm_id", p.algorithm_id);
    opt_set!("detection_status", p.detection_status);
    opt_set_ref!("vmask", p.vmask);
    opt_set_ref!("vtracker", p.vtracker);
    opt_set_ref!("vchip", p.vchip);
    opt_set_ref!("vchip_series", p.vchip_series);
    opt_set_ref!("vobject_series", p.vobject_series);

    kwargs.set_item("unknown", convert_unknown(py, &p.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &p.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

// ---------------------------------------------------------------------------
// ST 0903 — VmtiLs entry point
// ---------------------------------------------------------------------------

/// Translate a Rust `VmtiLs` to a Python `tstrans.klv.VmtiLs`
/// dataclass instance.
fn convert_vmti_ls(py: Python<'_>, v: &VmtiLs) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "VmtiLs"))?;
    let kwargs = PyDict::new_bound(py);

    if let Some(c) = v.checksum {
        kwargs.set_item("checksum", c)?;
    }
    if let Some(t) = v.precision_time_stamp {
        kwargs.set_item("precision_time_stamp", t)?;
    }
    if let Some(s) = &v.vmti_system_name {
        kwargs.set_item("vmti_system_name", s.as_str())?;
    }
    if let Some(n) = v.version_number {
        kwargs.set_item("version_number", n)?;
    }
    if let Some(n) = v.total_targets_in_frame {
        kwargs.set_item("total_targets_in_frame", n)?;
    }
    if let Some(n) = v.num_targets_reported {
        kwargs.set_item("num_targets_reported", n)?;
    }
    if let Some(n) = v.frame_width {
        kwargs.set_item("frame_width", n)?;
    }
    if let Some(n) = v.frame_height {
        kwargs.set_item("frame_height", n)?;
    }
    if let Some(s) = &v.source_sensor {
        kwargs.set_item("source_sensor", s.as_str())?;
    }
    if let Some(f) = v.horizontal_fov {
        kwargs.set_item("horizontal_fov", f)?;
    }
    if let Some(f) = v.vertical_fov {
        kwargs.set_item("vertical_fov", f)?;
    }
    if let Some(m) = v.miis_id.as_ref() {
        kwargs.set_item("miis_id", pyo3::types::PyBytes::new_bound(py, m.as_slice()))?;
    }
    if let Some(b) = v.algorithm_series.as_ref() {
        kwargs.set_item(
            "algorithm_series",
            pyo3::types::PyBytes::new_bound(py, b.as_slice()),
        )?;
    }
    if let Some(b) = v.ontology_series.as_ref() {
        kwargs.set_item(
            "ontology_series",
            pyo3::types::PyBytes::new_bound(py, b.as_slice()),
        )?;
    }
    let targets: Vec<PyObject> = v
        .targets
        .iter()
        .map(|t| convert_vtarget_pack(py, t))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item("targets", pyo3::types::PyTuple::new_bound(py, targets))?;
    kwargs.set_item("unknown", convert_unknown(py, &v.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &v.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Decode an ST 0903 VMTI LS. `buf` is **body-only** (no UL / outer
/// BER length wrapper). With `strict=True`, rejects missing required
/// tags per ST 0903.6 §6 Table 1, duplicate tags, malformed UTF-8.
#[pyfunction]
#[pyo3(name = "decode_vmti", signature = (buf, *, strict = false))]
fn decode_vmti_py(py: Python<'_>, buf: &[u8], strict: bool) -> PyResult<PyObject> {
    // Audit #11 / GIL-release decision: NOT wrapped — same rationale as
    // `decode_precision_timestamp_py`. VMTI records can be large with
    // many targets, but the per-target convert step (which builds Py
    // objects) keeps cumulative GIL hold low even for big records.
    // If a future profile shows VMTI workloads benefiting from release,
    // re-evaluate at that point.
    let result = if strict {
        decode_st0903_strict(buf)
    } else {
        decode_st0903_lenient(buf)
    };
    match result {
        Ok(vmti) => convert_vmti_ls(py, &vmti),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// ST 0903 — Python → Rust inverse translators + encode entry points
// ---------------------------------------------------------------------------

/// Inverse of `convert_vtarget_pack`.
///
/// `field_errors` is a parser-only diagnostic and is not round-tripped.
///
/// `unknown` IS round-tripped (audit #5): forward-compat TLVs preserved
/// by the VTargetPack decoder flow back into the encoder. Entries whose
/// tag collides with a typed field (see `is_st0903_vtarget_typed_tag`)
/// are silently dropped — typed wins.
#[allow(clippy::cognitive_complexity)]
fn py_to_vtarget_pack(p: &Bound<'_, PyAny>) -> PyResult<RustVTargetPack> {
    let mut r = RustVTargetPack::default();
    let py = p.py();

    // BER-OID `target_id` (mandatory, no Tag).
    r.target_id = p.getattr(intern!(py, "target_id"))?.extract::<u64>()?;

    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    macro_rules! ob {
        ($field:ident) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<Vec<u8>>>()?
            {
                r.$field = Some(v);
            }
        };
    }

    op!(centroid_pixel, u64);
    op!(bbox_top_left_pixel, u64);
    op!(bbox_bottom_right_pixel, u64);
    op!(priority, u8);
    op!(confidence_level, u8);
    op!(history, u16);
    op!(percentage_of_target_pixels, u8);

    // target_color: Optional<tuple[int, int, int]> → Option<[u8; 3]>.
    // `None` is a valid value — the field is simply absent from the LS.
    // A non-None value with the wrong tuple length is a caller bug; raise
    // instead of silently dropping (audit #6).
    let tc = p.getattr(intern!(py, "target_color"))?;
    if !tc.is_none() {
        let arr: Vec<u8> = tc.extract()?;
        if arr.len() != 3 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "target_color must be a 3-tuple, got length {}",
                arr.len()
            )));
        }
        r.target_color = Some([arr[0], arr[1], arr[2]]);
    }

    op!(target_intensity, u32);
    op!(centroid_lat_offset, f64);
    op!(centroid_lon_offset, f64);
    op!(centroid_hae, f64);
    op!(bbox_top_left_lat_offset, f64);
    op!(bbox_top_left_lon_offset, f64);
    op!(bbox_bottom_right_lat_offset, f64);
    op!(bbox_bottom_right_lon_offset, f64);
    ob!(target_location);
    ob!(geospatial_contour_series);
    op!(centroid_pix_row, u64);
    op!(centroid_pix_col, u64);
    op!(algorithm_id, u32);
    op!(detection_status, u8);
    ob!(vmask);
    ob!(vtracker);
    ob!(vchip);
    ob!(vchip_series);
    ob!(vobject_series);

    r.unknown = py_to_unknown(p, is_st0903_vtarget_typed_tag)?;

    Ok(r)
}

/// Inverse of `convert_vmti_ls`.
///
/// `field_errors` is parser-only and is not round-tripped.
///
/// `unknown` IS round-tripped (audit #5): forward-compat TLVs preserved
/// by the VMTI LS decoder flow back into the encoder. Entries whose tag
/// collides with a typed field (see `is_st0903_vmti_typed_tag`) are
/// silently dropped — typed wins.
fn py_to_vmti_ls(p: &Bound<'_, PyAny>) -> PyResult<VmtiLs> {
    let mut r = VmtiLs::default();
    let py = p.py();

    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }
    macro_rules! ob {
        ($field:ident) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<Vec<u8>>>()?
            {
                r.$field = Some(v);
            }
        };
    }

    // Note: `checksum` is intentionally ignored by `encode_standalone`
    // (it computes a fresh substrate checksum from the framing) and
    // dropped by `encode` (embedded-VMTI per ST 0903.6-120). We still
    // populate the Rust field for symmetry — encoders consult their own
    // policy.
    op!(checksum, u16);
    op!(precision_time_stamp, u64);
    os!(vmti_system_name);
    op!(version_number, u16);
    op!(total_targets_in_frame, u32);
    op!(num_targets_reported, u32);
    op!(frame_width, u32);
    op!(frame_height, u32);
    os!(source_sensor);
    op!(horizontal_fov, f64);
    op!(vertical_fov, f64);
    ob!(miis_id);
    ob!(algorithm_series);
    ob!(ontology_series);

    // targets: tuple[VTargetPack, ...] → Vec<VTargetPack>. The Python
    // dataclass uses a tuple (frozen + hashable); iterate generically.
    let targets_obj = p.getattr(intern!(py, "targets"))?;
    for t in targets_obj.iter()? {
        let t = t?;
        r.targets.push(py_to_vtarget_pack(&t)?);
    }

    r.unknown = py_to_unknown(p, is_st0903_vmti_typed_tag)?;

    Ok(r)
}

/// Encode a Python `VmtiLs` to wire bytes — VMTI LS **body only** (no UL
/// / outer BER length wrapper / no Tag 1 checksum per ST 0903.6-120).
/// Use for embedded-VMTI carried inside MPEG-TS via tst-pipeline. Returns
/// `bytes`.
#[pyfunction]
#[pyo3(name = "encode_vmti")]
fn encode_vmti_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_vmti_ls(record)?;
    let bytes = encode_st0903(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `VmtiLs` as a **standalone-VMTI** wire record:
/// `[VMTI_LS_UL:16][outer BER length][body][Tag 1 checkSum TLV]` per
/// ST 0903.4-17 / ST 0903.6-119. The Tag 1 checksum is computed from
/// the assembled framing; any value in `VmtiLs.checksum` is ignored.
/// Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_vmti_standalone")]
fn encode_vmti_standalone_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_vmti_ls(record)?;
    let bytes =
        encode_st0903_standalone(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `SecurityLs` to wire bytes with strict compliance per
/// ST 0102.12 — requires Tags 1 (Security Classification), 2 (Country
/// Coding Method), 3 (Classifying Country), 12 (Object Country Coding
/// Method), 13 (Object Country Codes), and 22 (Version). Raises
/// `KlvEncodeError(MISSING_MANDATORY_ITEM)` if any required tag is absent.
#[pyfunction]
#[pyo3(name = "encode_security_strict_compliance")]
fn encode_security_strict_compliance_py(
    py: Python<'_>,
    record: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    let rust_rec = py_to_security_ls(record)?;
    let bytes =
        encode_st0102_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `VmtiLs` to wire bytes with strict compliance for
/// **embedded** VMTI carriage — requires Tags 4 (version) and 6
/// (num_targets_reported); all VTargetPacks must be non-empty and have
/// unique target_ids. Raises `KlvEncodeError` on validation failure.
#[pyfunction]
#[pyo3(name = "encode_vmti_strict_compliance")]
fn encode_vmti_strict_compliance_py(
    py: Python<'_>,
    record: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    let rust_rec = py_to_vmti_ls(record)?;
    let bytes =
        encode_st0903_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `VmtiLs` as a **standalone** wire record with strict
/// compliance — all checks from `encode_vmti_strict_compliance` plus
/// standalone-required Tags 2 (precision_time_stamp), 11 (horizontal_fov),
/// 12 (vertical_fov), 13 (miis_id), and forbids per-pack offset tags
/// (10/11/13/14/15/16). Raises `KlvEncodeError` on validation failure.
#[pyfunction]
#[pyo3(name = "encode_vmti_standalone_strict_compliance")]
fn encode_vmti_standalone_strict_compliance_py(
    py: Python<'_>,
    record: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    let rust_rec = py_to_vmti_ls(record)?;
    let bytes = encode_st0903_standalone_strict_compliance(&rust_rec)
        .map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0601 — Tags 34/63/77 coded enums (WP-A Table A3)
// ---------------------------------------------------------------------------
//
// `IcingDetected::to_wire`/`from_wire` (and the SensorFovName /
// OperationalMode equivalents) are `pub(crate)`-scoped to tst-core, so the
// tiny wire-code tables are duplicated locally here — same rationale as the
// `is_st0601_typed_tag`-family predicates above (narrow inventories kept
// local rather than threading internal Rust APIs out of tst-core).

/// Translate a Rust `IcingDetected` to the matching Python
/// `tstrans.klv.IcingDetected` enum instance for known codepoints, or a
/// raw `int` for `Other(code)` (wire-unknown, round-trips byte-exact) —
/// mirrors the ST 0102 `SecurityClassification::Unknown(b)` asymmetry.
fn convert_icing_detected(py: Python<'_>, v: RustIcingDetected) -> PyResult<PyObject> {
    let code = match v {
        RustIcingDetected::DetectorOff => 0u8,
        RustIcingDetected::NoIcingDetected => 1,
        RustIcingDetected::IcingDetected => 2,
        RustIcingDetected::Other(b) => return Ok(b.into_py(py)),
        // #[non_exhaustive] in tst-core: a wildcard is required even though
        // every current variant is covered above.
        _ => {
            return Err(PyValueError::new_err(
                "unknown IcingDetected variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "IcingDetected"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_icing_detected`.
fn icing_detected_from_wire(b: u8) -> RustIcingDetected {
    match b {
        0 => RustIcingDetected::DetectorOff,
        1 => RustIcingDetected::NoIcingDetected,
        2 => RustIcingDetected::IcingDetected,
        other => RustIcingDetected::Other(other),
    }
}

/// Translate a Rust `SensorFovName` to the matching Python
/// `tstrans.klv.SensorFovName` enum instance for known codepoints, or a
/// raw `int` for `Other(code)`.
fn convert_sensor_fov_name(py: Python<'_>, v: RustSensorFovName) -> PyResult<PyObject> {
    let code = match v {
        RustSensorFovName::Ultranarrow => 0u8,
        RustSensorFovName::Narrow => 1,
        RustSensorFovName::Medium => 2,
        RustSensorFovName::Wide => 3,
        RustSensorFovName::Ultrawide => 4,
        RustSensorFovName::NarrowMedium => 5,
        RustSensorFovName::TwoXUltranarrow => 6,
        RustSensorFovName::FourXUltranarrow => 7,
        RustSensorFovName::ContinuousZoom => 8,
        RustSensorFovName::Other(b) => return Ok(b.into_py(py)),
        _ => {
            return Err(PyValueError::new_err(
                "unknown SensorFovName variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SensorFovName"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_sensor_fov_name`.
fn sensor_fov_name_from_wire(b: u8) -> RustSensorFovName {
    match b {
        0 => RustSensorFovName::Ultranarrow,
        1 => RustSensorFovName::Narrow,
        2 => RustSensorFovName::Medium,
        3 => RustSensorFovName::Wide,
        4 => RustSensorFovName::Ultrawide,
        5 => RustSensorFovName::NarrowMedium,
        6 => RustSensorFovName::TwoXUltranarrow,
        7 => RustSensorFovName::FourXUltranarrow,
        8 => RustSensorFovName::ContinuousZoom,
        other => RustSensorFovName::Other(other),
    }
}

/// Translate a Rust `OperationalMode` to the matching Python
/// `tstrans.klv.OperationalMode` enum instance for known codepoints, or a
/// raw `int` for `Other(code)`.
fn convert_operational_mode(py: Python<'_>, v: RustOperationalMode) -> PyResult<PyObject> {
    let code = match v {
        RustOperationalMode::OtherMode => 0u8,
        RustOperationalMode::Operational => 1,
        RustOperationalMode::Training => 2,
        RustOperationalMode::Exercise => 3,
        RustOperationalMode::Maintenance => 4,
        RustOperationalMode::Test => 5,
        RustOperationalMode::Other(b) => return Ok(b.into_py(py)),
        _ => {
            return Err(PyValueError::new_err(
                "unknown OperationalMode variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "OperationalMode"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_operational_mode`.
fn operational_mode_from_wire(b: u8) -> RustOperationalMode {
    match b {
        0 => RustOperationalMode::OtherMode,
        1 => RustOperationalMode::Operational,
        2 => RustOperationalMode::Training,
        3 => RustOperationalMode::Exercise,
        4 => RustOperationalMode::Maintenance,
        5 => RustOperationalMode::Test,
        other => RustOperationalMode::Other(other),
    }
}

// ---------------------------------------------------------------------------
// ST 0601 — Tags 125/126 coded enums (WP-B Table B2)
// ---------------------------------------------------------------------------
//
// Same pattern as the Table A3 enums above: `PlatformStatus`/
// `SensorControlMode::to_wire`/`from_wire` are `pub(crate)`-scoped to
// tst-core, so the wire-code tables are duplicated locally here.

/// Translate a Rust `PlatformStatus` to the matching Python
/// `tstrans.klv.PlatformStatus` enum instance for known codepoints, or a
/// raw `int` for `Other(code)`.
fn convert_platform_status(py: Python<'_>, v: RustPlatformStatus) -> PyResult<PyObject> {
    let code = match v {
        RustPlatformStatus::Active => 0u8,
        RustPlatformStatus::PreFlight => 1,
        RustPlatformStatus::PreFlightTaxiing => 2,
        RustPlatformStatus::RunUp => 3,
        RustPlatformStatus::TakeOff => 4,
        RustPlatformStatus::Ingress => 5,
        RustPlatformStatus::ManualOperation => 6,
        RustPlatformStatus::AutomatedOrbit => 7,
        RustPlatformStatus::Transitioning => 8,
        RustPlatformStatus::Egress => 9,
        RustPlatformStatus::Landing => 10,
        RustPlatformStatus::LandedTaxiing => 11,
        RustPlatformStatus::LandedParked => 12,
        RustPlatformStatus::Other(b) => return Ok(b.into_py(py)),
        _ => {
            return Err(PyValueError::new_err(
                "unknown PlatformStatus variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "PlatformStatus"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_platform_status`.
fn platform_status_from_wire(b: u8) -> RustPlatformStatus {
    match b {
        0 => RustPlatformStatus::Active,
        1 => RustPlatformStatus::PreFlight,
        2 => RustPlatformStatus::PreFlightTaxiing,
        3 => RustPlatformStatus::RunUp,
        4 => RustPlatformStatus::TakeOff,
        5 => RustPlatformStatus::Ingress,
        6 => RustPlatformStatus::ManualOperation,
        7 => RustPlatformStatus::AutomatedOrbit,
        8 => RustPlatformStatus::Transitioning,
        9 => RustPlatformStatus::Egress,
        10 => RustPlatformStatus::Landing,
        11 => RustPlatformStatus::LandedTaxiing,
        12 => RustPlatformStatus::LandedParked,
        other => RustPlatformStatus::Other(other),
    }
}

/// Translate a Rust `SensorControlMode` to the matching Python
/// `tstrans.klv.SensorControlMode` enum instance for known codepoints, or a
/// raw `int` for `Other(code)`.
fn convert_sensor_control_mode(py: Python<'_>, v: RustSensorControlMode) -> PyResult<PyObject> {
    let code = match v {
        RustSensorControlMode::Off => 0u8,
        RustSensorControlMode::HomePosition => 1,
        RustSensorControlMode::Uncontrolled => 2,
        RustSensorControlMode::ManualControl => 3,
        RustSensorControlMode::Calibrating => 4,
        RustSensorControlMode::AutoHoldingPosition => 5,
        RustSensorControlMode::AutoTracking => 6,
        RustSensorControlMode::Other(b) => return Ok(b.into_py(py)),
        _ => {
            return Err(PyValueError::new_err(
                "unknown SensorControlMode variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SensorControlMode"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_sensor_control_mode`.
fn sensor_control_mode_from_wire(b: u8) -> RustSensorControlMode {
    match b {
        0 => RustSensorControlMode::Off,
        1 => RustSensorControlMode::HomePosition,
        2 => RustSensorControlMode::Uncontrolled,
        3 => RustSensorControlMode::ManualControl,
        4 => RustSensorControlMode::Calibrating,
        5 => RustSensorControlMode::AutoHoldingPosition,
        6 => RustSensorControlMode::AutoTracking,
        other => RustSensorControlMode::Other(other),
    }
}

// ---------------------------------------------------------------------------
// ST 1201.5 — imapb_specials side channel (WP-B)
// ---------------------------------------------------------------------------
//
// Crossing shape (DECIDED, shared with the JVM binding): a tuple of
// `(tag: int, code: str, payload: int)` triples. `code` names the
// `ImapbSpecial` family; `payload` is the NaN-id/signal value (0 for the
// payload-less BelowMin/AboveMax/Infinity codes).

/// Translate a Rust `ImapbSpecial` to its `(code, payload)` wire-string
/// pair for the `imapb_specials` crossing. Errors (rather than silently
/// mislabeling) on a future non-exhaustive variant — same stance as
/// `convert_platform_status`/`convert_sensor_control_mode` above.
fn imapb_special_to_code(s: RustImapbSpecial) -> PyResult<(&'static str, u64)> {
    Ok(match s {
        RustImapbSpecial::BelowMin => ("below_min", 0),
        RustImapbSpecial::AboveMax => ("above_max", 0),
        RustImapbSpecial::PositiveInfinity => ("pos_infinity", 0),
        RustImapbSpecial::NegativeInfinity => ("neg_infinity", 0),
        RustImapbSpecial::PositiveQuietNaN { nan_id } => ("pos_quiet_nan", nan_id),
        RustImapbSpecial::NegativeQuietNaN { nan_id } => ("neg_quiet_nan", nan_id),
        RustImapbSpecial::PositiveSignalingNaN { signal } => ("pos_signaling_nan", signal),
        RustImapbSpecial::NegativeSignalingNaN { signal } => ("neg_signaling_nan", signal),
        RustImapbSpecial::UserDefined { signal } => ("user_defined", signal),
        // #[non_exhaustive] in tst-core: no current variant reaches here.
        _ => {
            return Err(PyValueError::new_err(
                "unknown ImapbSpecial variant crossing the binding",
            ));
        }
    })
}

/// Inverse of `imapb_special_to_code`. Raises `ValueError` for a code
/// string outside the 9-member set (audit #6 "validate-don't-drop" —
/// same stance as `py_to_vtarget_pack`'s `target_color` length check).
fn imapb_special_from_code(code: &str, payload: u64) -> PyResult<RustImapbSpecial> {
    Ok(match code {
        "below_min" => RustImapbSpecial::BelowMin,
        "above_max" => RustImapbSpecial::AboveMax,
        "pos_infinity" => RustImapbSpecial::PositiveInfinity,
        "neg_infinity" => RustImapbSpecial::NegativeInfinity,
        "pos_quiet_nan" => RustImapbSpecial::PositiveQuietNaN { nan_id: payload },
        "neg_quiet_nan" => RustImapbSpecial::NegativeQuietNaN { nan_id: payload },
        "pos_signaling_nan" => RustImapbSpecial::PositiveSignalingNaN { signal: payload },
        "neg_signaling_nan" => RustImapbSpecial::NegativeSignalingNaN { signal: payload },
        "user_defined" => RustImapbSpecial::UserDefined { signal: payload },
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown imapb_specials code {other:?}; expected one of below_min, above_max, \
                 pos_infinity, neg_infinity, pos_quiet_nan, neg_quiet_nan, pos_signaling_nan, \
                 neg_signaling_nan, user_defined"
            )));
        }
    })
}

// ---------------------------------------------------------------------------
// ST 0601 — WP-C pack & list items (Table C1), carried inside
// UasDatalinkLs. Follows the VTargetPack nested-struct pattern: a
// dataclass in klv.py + a `convert_*`/`py_to_*` pair here.
// ---------------------------------------------------------------------------

/// Item 81: Image Horizon Pixels.
fn convert_image_horizon(py: Python<'_>, h: &RustImageHorizonPixels) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "ImageHorizonPixels"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("x0_pct", h.x0_pct)?;
    kwargs.set_item("y0_pct", h.y0_pct)?;
    kwargs.set_item("x1_pct", h.x1_pct)?;
    kwargs.set_item("y1_pct", h.y1_pct)?;
    if let Some(v) = h.start_lat_deg {
        kwargs.set_item("start_lat_deg", v)?;
    }
    if let Some(v) = h.start_lon_deg {
        kwargs.set_item("start_lon_deg", v)?;
    }
    if let Some(v) = h.end_lat_deg {
        kwargs.set_item("end_lat_deg", v)?;
    }
    if let Some(v) = h.end_lon_deg {
        kwargs.set_item("end_lon_deg", v)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_image_horizon(p: &Bound<'_, PyAny>) -> PyResult<RustImageHorizonPixels> {
    let py = p.py();
    Ok(RustImageHorizonPixels {
        x0_pct: p.getattr(intern!(py, "x0_pct"))?.extract()?,
        y0_pct: p.getattr(intern!(py, "y0_pct"))?.extract()?,
        x1_pct: p.getattr(intern!(py, "x1_pct"))?.extract()?,
        y1_pct: p.getattr(intern!(py, "y1_pct"))?.extract()?,
        start_lat_deg: p.getattr(intern!(py, "start_lat_deg"))?.extract()?,
        start_lon_deg: p.getattr(intern!(py, "start_lon_deg"))?.extract()?,
        end_lat_deg: p.getattr(intern!(py, "end_lat_deg"))?.extract()?,
        end_lon_deg: p.getattr(intern!(py, "end_lon_deg"))?.extract()?,
    })
}

/// Item 115: Control Command — MULTI-INSTANCE (`UasDatalinkLs.control_commands`).
fn convert_control_command(py: Python<'_>, c: &RustControlCommand) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "ControlCommand"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("id", c.id)?;
    kwargs.set_item("command", c.command.as_str())?;
    if let Some(t) = c.time_us {
        kwargs.set_item("time_us", t)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_control_command(p: &Bound<'_, PyAny>) -> PyResult<RustControlCommand> {
    let py = p.py();
    Ok(RustControlCommand {
        id: p.getattr(intern!(py, "id"))?.extract()?,
        command: p.getattr(intern!(py, "command"))?.extract()?,
        time_us: p.getattr(intern!(py, "time_us"))?.extract()?,
    })
}

/// Item 127: Sensor Frame Rate Pack.
fn convert_sensor_frame_rate(py: Python<'_>, fr: &RustSensorFrameRate) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SensorFrameRate"))?;
    Ok(cls.call1((fr.numerator, fr.denominator))?.unbind())
}

fn py_to_sensor_frame_rate(p: &Bound<'_, PyAny>) -> PyResult<RustSensorFrameRate> {
    let py = p.py();
    Ok(RustSensorFrameRate {
        numerator: p.getattr(intern!(py, "numerator"))?.extract()?,
        denominator: p.getattr(intern!(py, "denominator"))?.extract()?,
    })
}

/// Item 143: Metadata Substream Id.
fn convert_metadata_substream_id(
    py: Python<'_>,
    ms: &RustMetadataSubstreamId,
) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "MetadataSubstreamId"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("local_id", ms.local_id)?;
    if let Some(uuid) = ms.uuid {
        kwargs.set_item("uuid", pyo3::types::PyBytes::new_bound(py, &uuid))?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_metadata_substream_id(p: &Bound<'_, PyAny>) -> PyResult<RustMetadataSubstreamId> {
    let py = p.py();
    let local_id = p.getattr(intern!(py, "local_id"))?.extract()?;
    let uuid_obj = p.getattr(intern!(py, "uuid"))?;
    let uuid = if uuid_obj.is_none() {
        None
    } else {
        let bytes: Vec<u8> = uuid_obj.extract()?;
        if bytes.len() != 16 {
            return Err(PyValueError::new_err(format!(
                "MetadataSubstreamId.uuid must be 16 bytes, got {}",
                bytes.len()
            )));
        }
        let mut u = [0u8; 16];
        u.copy_from_slice(&bytes);
        Some(u)
    };
    Ok(RustMetadataSubstreamId { local_id, uuid })
}

/// Item 122: Country Codes.
fn convert_country_codes(py: Python<'_>, cc: &RustCountryCodes) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "CountryCodes"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("coding_method", cc.coding_method)?;
    if let Some(s) = cc.overflight.as_deref() {
        kwargs.set_item("overflight", s)?;
    }
    if let Some(s) = cc.operator.as_deref() {
        kwargs.set_item("operator", s)?;
    }
    if let Some(s) = cc.manufacture.as_deref() {
        kwargs.set_item("manufacture", s)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_country_codes(p: &Bound<'_, PyAny>) -> PyResult<RustCountryCodes> {
    let py = p.py();
    Ok(RustCountryCodes {
        coding_method: p.getattr(intern!(py, "coding_method"))?.extract()?,
        overflight: p.getattr(intern!(py, "overflight"))?.extract()?,
        operator: p.getattr(intern!(py, "operator"))?.extract()?,
        manufacture: p.getattr(intern!(py, "manufacture"))?.extract()?,
    })
}

/// One record of Item 128, Wavelengths List.
fn convert_wavelength_record(py: Python<'_>, w: &RustWavelengthRecord) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "WavelengthRecord"))?;
    Ok(cls
        .call1((w.id, w.min_nm, w.max_nm, w.name.as_str()))?
        .unbind())
}

fn py_to_wavelength_record(p: &Bound<'_, PyAny>) -> PyResult<RustWavelengthRecord> {
    let py = p.py();
    Ok(RustWavelengthRecord {
        id: p.getattr(intern!(py, "id"))?.extract()?,
        min_nm: p.getattr(intern!(py, "min_nm"))?.extract()?,
        max_nm: p.getattr(intern!(py, "max_nm"))?.extract()?,
        name: p.getattr(intern!(py, "name"))?.extract()?,
    })
}

/// Shared by Item 130 (Airbase Locations) and Item 141 (Waypoint List).
fn convert_location(py: Python<'_>, loc: &RustLocation) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "Location"))?;
    let kwargs = PyDict::new_bound(py);
    if let Some(v) = loc.lat_deg {
        kwargs.set_item("lat_deg", v)?;
    }
    if let Some(v) = loc.lon_deg {
        kwargs.set_item("lon_deg", v)?;
    }
    if let Some(v) = loc.hae_m {
        kwargs.set_item("hae_m", v)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_location(p: &Bound<'_, PyAny>) -> PyResult<RustLocation> {
    let py = p.py();
    Ok(RustLocation {
        lat_deg: p.getattr(intern!(py, "lat_deg"))?.extract()?,
        lon_deg: p.getattr(intern!(py, "lon_deg"))?.extract()?,
        hae_m: p.getattr(intern!(py, "hae_m"))?.extract()?,
    })
}

/// Item 130: Airbase Locations.
fn convert_airbase_locations(py: Python<'_>, al: &RustAirbaseLocations) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "AirbaseLocations"))?;
    let kwargs = PyDict::new_bound(py);
    if let Some(loc) = al.take_off {
        kwargs.set_item("take_off", convert_location(py, &loc)?)?;
    }
    if let Some(loc) = al.recovery {
        kwargs.set_item("recovery", convert_location(py, &loc)?)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_airbase_locations(p: &Bound<'_, PyAny>) -> PyResult<RustAirbaseLocations> {
    let py = p.py();
    let take_off_obj = p.getattr(intern!(py, "take_off"))?;
    let take_off = if take_off_obj.is_none() {
        None
    } else {
        Some(py_to_location(&take_off_obj)?)
    };
    let recovery_obj = p.getattr(intern!(py, "recovery"))?;
    let recovery = if recovery_obj.is_none() {
        None
    } else {
        Some(py_to_location(&recovery_obj)?)
    };
    Ok(RustAirbaseLocations { take_off, recovery })
}

// `PayloadType::to_wire`/`from_wire` are `pub(crate)`-scoped to tst-core
// (same rationale as the WP-A coded-enum comment above
// `convert_icing_detected`) — the tiny wire-code table is duplicated
// locally here.

/// Item 138 §Table 17 Payload Type — codepoint enum. Wire-unknown codes
/// surface as a raw `int` (`Other(code)`), same asymmetric pattern as
/// `IcingDetected`.
fn convert_payload_type(py: Python<'_>, v: RustPayloadType) -> PyResult<PyObject> {
    let code = match v {
        RustPayloadType::ElectroOptical => 0u64,
        RustPayloadType::Lidar => 1,
        RustPayloadType::Radar => 2,
        RustPayloadType::Sigint => 3,
        RustPayloadType::Sar => 4,
        RustPayloadType::Other(c) => return Ok(c.into_py(py)),
        // #[non_exhaustive] in tst-core: a wildcard is required even
        // though every current variant is covered above.
        _ => {
            return Err(PyValueError::new_err(
                "unknown PayloadType variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "PayloadType"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_payload_type`.
fn payload_type_from_wire(code: u64) -> RustPayloadType {
    match code {
        0 => RustPayloadType::ElectroOptical,
        1 => RustPayloadType::Lidar,
        2 => RustPayloadType::Radar,
        3 => RustPayloadType::Sigint,
        4 => RustPayloadType::Sar,
        other => RustPayloadType::Other(other),
    }
}

/// One record of Item 138, Payload List.
fn convert_payload_record(py: Python<'_>, r: &RustPayloadRecord) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "PayloadRecord"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("id", r.id)?;
    kwargs.set_item("payload_type", convert_payload_type(py, r.payload_type)?)?;
    kwargs.set_item("name", r.name.as_str())?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_payload_record(p: &Bound<'_, PyAny>) -> PyResult<RustPayloadRecord> {
    let py = p.py();
    let id = p.getattr(intern!(py, "id"))?.extract()?;
    let payload_type_obj = p.getattr(intern!(py, "payload_type"))?;
    let payload_type = payload_type_from_wire(enum_field_to_u64(&payload_type_obj)?);
    let name = p.getattr(intern!(py, "name"))?.extract()?;
    Ok(RustPayloadRecord {
        id,
        payload_type,
        name,
    })
}

/// Item 138: Payload List.
fn convert_payload_list(py: Python<'_>, pl: &RustPayloadList) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "PayloadList"))?;
    let records: Vec<PyObject> = pl
        .records
        .iter()
        .map(|r| convert_payload_record(py, r))
        .collect::<PyResult<Vec<_>>>()?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("count", pl.count)?;
    kwargs.set_item("records", pyo3::types::PyTuple::new_bound(py, records))?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_payload_list(p: &Bound<'_, PyAny>) -> PyResult<RustPayloadList> {
    let py = p.py();
    let count = p.getattr(intern!(py, "count"))?.extract()?;
    let mut records = Vec::new();
    for r in p.getattr(intern!(py, "records"))?.iter()? {
        records.push(py_to_payload_record(&r?)?);
    }
    Ok(RustPayloadList { count, records })
}

/// One record of Item 140, Weapons Stores. `general_status`/`fuze_enabled`/
/// `laser_enabled`/`target_enabled`/`weapon_armed` are `@property`
/// accessors on the Python side (see `WeaponsStore` in klv.py) computed
/// from `status_raw` — only the raw field crosses the binding.
fn convert_weapons_store(py: Python<'_>, ws: &RustWeaponsStore) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "WeaponsStore"))?;
    Ok(cls
        .call1((
            ws.station_id,
            ws.hardpoint_id,
            ws.carriage_id,
            ws.store_id,
            ws.status_raw,
            ws.weapon_type.as_str(),
        ))?
        .unbind())
}

fn py_to_weapons_store(p: &Bound<'_, PyAny>) -> PyResult<RustWeaponsStore> {
    let py = p.py();
    Ok(RustWeaponsStore {
        station_id: p.getattr(intern!(py, "station_id"))?.extract()?,
        hardpoint_id: p.getattr(intern!(py, "hardpoint_id"))?.extract()?,
        carriage_id: p.getattr(intern!(py, "carriage_id"))?.extract()?,
        store_id: p.getattr(intern!(py, "store_id"))?.extract()?,
        status_raw: p.getattr(intern!(py, "status_raw"))?.extract()?,
        weapon_type: p.getattr(intern!(py, "weapon_type"))?.extract()?,
    })
}

/// One record of Item 141, Waypoint List.
fn convert_waypoint(py: Python<'_>, wp: &RustWaypoint) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "Waypoint"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("id", wp.id)?;
    kwargs.set_item("prosecution_order", wp.prosecution_order)?;
    if let Some(v) = wp.info {
        kwargs.set_item("info", v)?;
    }
    if let Some(loc) = wp.location {
        kwargs.set_item("location", convert_location(py, &loc)?)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_waypoint(p: &Bound<'_, PyAny>) -> PyResult<RustWaypoint> {
    let py = p.py();
    let id = p.getattr(intern!(py, "id"))?.extract()?;
    let prosecution_order = p.getattr(intern!(py, "prosecution_order"))?.extract()?;
    let info = p.getattr(intern!(py, "info"))?.extract()?;
    let location_obj = p.getattr(intern!(py, "location"))?;
    let location = if location_obj.is_none() {
        None
    } else {
        Some(py_to_location(&location_obj)?)
    };
    Ok(RustWaypoint {
        id,
        prosecution_order,
        info,
        location,
    })
}

/// One `(start, range)` pair of Item 142, View Domain.
fn convert_view_domain_pair(py: Python<'_>, p: &RustViewDomainPair) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "ViewDomainPair"))?;
    Ok(cls.call1((p.start_deg, p.range_deg))?.unbind())
}

fn py_to_view_domain_pair(p: &Bound<'_, PyAny>) -> PyResult<RustViewDomainPair> {
    let py = p.py();
    Ok(RustViewDomainPair {
        start_deg: p.getattr(intern!(py, "start_deg"))?.extract()?,
        range_deg: p.getattr(intern!(py, "range_deg"))?.extract()?,
    })
}

/// Item 142: View Domain.
fn convert_view_domain(py: Python<'_>, vd: &RustViewDomain) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "ViewDomain"))?;
    let kwargs = PyDict::new_bound(py);
    if let Some(p) = vd.azimuth {
        kwargs.set_item("azimuth", convert_view_domain_pair(py, &p)?)?;
    }
    if let Some(p) = vd.elevation {
        kwargs.set_item("elevation", convert_view_domain_pair(py, &p)?)?;
    }
    if let Some(p) = vd.roll {
        kwargs.set_item("roll", convert_view_domain_pair(py, &p)?)?;
    }
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_view_domain(p: &Bound<'_, PyAny>) -> PyResult<RustViewDomain> {
    let py = p.py();
    macro_rules! opt_pair {
        ($field:ident) => {{
            let obj = p.getattr(intern!(py, stringify!($field)))?;
            if obj.is_none() {
                None
            } else {
                Some(py_to_view_domain_pair(&obj)?)
            }
        }};
    }
    Ok(RustViewDomain {
        azimuth: opt_pair!(azimuth),
        elevation: opt_pair!(elevation),
        roll: opt_pair!(roll),
    })
}

/// One captured ST 0601 Item 102 (SDCC-FLP) occurrence — MULTI-INSTANCE
/// (`UasDatalinkLs.sdcc_flps`). `bytes` is the raw pack; decode it with
/// the free function `decode_sdcc_flp`.
fn convert_sdcc_flp_field(py: Python<'_>, f: &RustSdccFlpField) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SdccFlpField"))?;
    let preceding =
        pyo3::types::PyTuple::new_bound(py, f.preceding_tags.iter().map(|&t| t.into_py(py)));
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("preceding_tags", preceding)?;
    kwargs.set_item("bytes", pyo3::types::PyBytes::new_bound(py, &f.bytes))?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

fn py_to_sdcc_flp_field(p: &Bound<'_, PyAny>) -> PyResult<RustSdccFlpField> {
    let py = p.py();
    Ok(RustSdccFlpField {
        preceding_tags: p.getattr(intern!(py, "preceding_tags"))?.extract()?,
        bytes: p.getattr(intern!(py, "bytes"))?.extract()?,
    })
}

// ---------------------------------------------------------------------------
// ST 1010.3 SDCC-FLP — general-purpose (not ST 0601-specific); entry
// points further down. `SdccFlp` has no `py_to_*` inverse: the only
// encoder, `encode_sdcc_flp_mode2`, takes plain std-dev/correlation
// lists rather than a full struct (see the C1 outcome notes).
// ---------------------------------------------------------------------------

/// Translate a Rust `SdccFlp` to a Python `tstrans.klv.SdccFlp` dataclass.
fn convert_sdcc_flp(py: Python<'_>, m: &RustSdccFlp) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "SdccFlp"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item("matrix_size", m.matrix_size)?;
    kwargs.set_item(
        "std_devs",
        pyo3::types::PyTuple::new_bound(py, m.std_devs.iter().map(|&v| v.into_py(py))),
    )?;
    kwargs.set_item(
        "correlations",
        pyo3::types::PyTuple::new_bound(py, m.correlations.iter().map(|&v| v.into_py(py))),
    )?;
    kwargs.set_item(
        "correlation_present",
        pyo3::types::PyTuple::new_bound(py, m.correlation_present.iter().map(|&v| v.into_py(py))),
    )?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Decode a MISB ST 1010.3 SDCC-FLP pack (Mode 1 and Mode 2). `buf` is
/// the pack bytes starting at Element 1 (Matrix Size) — no outer TLV
/// framing and no leading Universal Label. General-purpose: not
/// ST 0601-specific (see `SdccFlp`'s docstring for the ST 0601 Tag 102
/// "Refined Source List" carriage, which is a separate concern captured
/// in `UasDatalinkLs.sdcc_flps` / `SdccFlpField`).
#[pyfunction]
#[pyo3(name = "decode_sdcc_flp")]
fn decode_sdcc_flp_py(py: Python<'_>, buf: &[u8]) -> PyResult<PyObject> {
    match decode_st1010_sdcc_flp(buf) {
        Ok(m) => convert_sdcc_flp(py, &m),
        Err(e) => Err(klv_field_error_to_pyerr(py, e)),
    }
}

/// Encode a Mode-2 SDCC-FLP: standard deviations as IEEE binary32,
/// correlations as ST 1201 IMAPB(-1, 1, `clen`). Sparse mode + Bit
/// Vector are chosen automatically when zero-correlations make it pay.
/// `len(correlations)` must equal `len(std_devs) * (len(std_devs) - 1) / 2`
/// (the upper-triangle slot count), in row-major (i<j) order. Returns
/// `bytes`.
#[pyfunction]
#[pyo3(name = "encode_sdcc_flp_mode2")]
fn encode_sdcc_flp_mode2_py(
    py: Python<'_>,
    std_devs: Vec<f64>,
    correlations: Vec<f64>,
    clen: usize,
) -> PyResult<PyObject> {
    let bytes = encode_st1010_sdcc_flp_mode2(&std_devs, &correlations, clen)
        .map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0601 — UAS Datalink LS
// ---------------------------------------------------------------------------

/// Translate a Rust `UasDatalinkLs` to a Python `tstrans.klv.UasDatalinkLs`
/// dataclass instance. Mechanical 147-field projection.
#[allow(clippy::cognitive_complexity)]
fn convert_uas_datalink_ls(py: Python<'_>, r: &UasDatalinkLs) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "UasDatalinkLs"))?;
    let kwargs = PyDict::new_bound(py);

    kwargs.set_item(
        "universal_label",
        pyo3::types::PyBytes::new_bound(py, &r.universal_label.0),
    )?;
    kwargs.set_item("declared_version", r.declared_version)?;

    // Optional<String>
    macro_rules! os {
        ($k:expr, $v:expr) => {
            if let Some(s) = $v.as_ref() {
                kwargs.set_item($k, s.as_str())?;
            }
        };
    }
    // Optional<scalar> (Copy)
    macro_rules! op {
        ($k:expr, $v:expr) => {
            if let Some(v) = $v {
                kwargs.set_item($k, v)?;
            }
        };
    }
    // Optional<Vec<u8>> — emit as Python `bytes` (not list[int]); the
    // Python dataclass field is typed `bytes | None` and downstream
    // typed decoders (`decode_security` / `decode_vmti`) take `bytes`.
    macro_rules! ob {
        ($k:expr, $v:expr) => {
            if let Some(b) = $v.as_ref() {
                kwargs.set_item($k, pyo3::types::PyBytes::new_bound(py, b.as_slice()))?;
            }
        };
    }

    os!("mission_id", r.mission_id);
    os!("platform_tail_number", r.platform_tail_number);
    os!("platform_designation", r.platform_designation);
    os!("image_source_sensor", r.image_source_sensor);
    os!("image_coordinate_system", r.image_coordinate_system);
    os!("platform_call_sign", r.platform_call_sign);
    op!("uas_ls_version", r.uas_ls_version);
    op!("timestamp_us", r.timestamp_us);
    op!("platform_heading_deg", r.platform_heading_deg);
    op!("platform_pitch_deg", r.platform_pitch_deg);
    op!("platform_roll_deg", r.platform_roll_deg);
    op!("platform_true_airspeed", r.platform_true_airspeed);
    op!("platform_indicated_airspeed", r.platform_indicated_airspeed);
    op!("platform_pitch_full_deg", r.platform_pitch_full_deg);
    op!("platform_roll_full_deg", r.platform_roll_full_deg);
    op!(
        "platform_angle_of_attack_deg",
        r.platform_angle_of_attack_deg
    );
    op!("sensor_lat_deg", r.sensor_lat_deg);
    op!("sensor_lon_deg", r.sensor_lon_deg);
    op!("sensor_alt_m", r.sensor_alt_m);
    op!("sensor_ellipsoid_height_m", r.sensor_ellipsoid_height_m);
    op!("sensor_hfov_deg", r.sensor_hfov_deg);
    op!("sensor_vfov_deg", r.sensor_vfov_deg);
    op!("sensor_rel_az_deg", r.sensor_rel_az_deg);
    op!("sensor_rel_el_deg", r.sensor_rel_el_deg);
    op!("sensor_rel_roll_deg", r.sensor_rel_roll_deg);
    op!("slant_range_m", r.slant_range_m);
    op!("target_width_m", r.target_width_m);
    op!("frame_center_lat_deg", r.frame_center_lat_deg);
    op!("frame_center_lon_deg", r.frame_center_lon_deg);
    op!("frame_center_elev_m", r.frame_center_elev_m);
    op!(
        "frame_center_ellipsoid_height_m",
        r.frame_center_ellipsoid_height_m
    );
    op!("corner_lat_offset_p1_deg", r.corner_lat_offset_p1_deg);
    op!("corner_lon_offset_p1_deg", r.corner_lon_offset_p1_deg);
    op!("corner_lat_offset_p2_deg", r.corner_lat_offset_p2_deg);
    op!("corner_lon_offset_p2_deg", r.corner_lon_offset_p2_deg);
    op!("corner_lat_offset_p3_deg", r.corner_lat_offset_p3_deg);
    op!("corner_lon_offset_p3_deg", r.corner_lon_offset_p3_deg);
    op!("corner_lat_offset_p4_deg", r.corner_lat_offset_p4_deg);
    op!("corner_lon_offset_p4_deg", r.corner_lon_offset_p4_deg);
    op!("corner_lat_p1_deg", r.corner_lat_p1_deg);
    op!("corner_lon_p1_deg", r.corner_lon_p1_deg);
    op!("corner_lat_p2_deg", r.corner_lat_p2_deg);
    op!("corner_lon_p2_deg", r.corner_lon_p2_deg);
    op!("corner_lat_p3_deg", r.corner_lat_p3_deg);
    op!("corner_lon_p3_deg", r.corner_lon_p3_deg);
    op!("corner_lat_p4_deg", r.corner_lat_p4_deg);
    op!("corner_lon_p4_deg", r.corner_lon_p4_deg);
    op!("generic_flag_data", r.generic_flag_data);
    ob!("security_local_set", r.security_local_set);
    ob!("vmti", r.vmti);
    ob!("miis_core_id", r.miis_core_id);

    // WP-A Table A1 — ranged f64 fields (tags 40-46, 35-38/49/53-55,
    // 51/52/56-58/64/92/93, 67-69/71/76, 79-80).
    op!("target_location_lat_deg", r.target_location_lat_deg);
    op!("target_location_lon_deg", r.target_location_lon_deg);
    op!("target_location_elev_m", r.target_location_elev_m);
    op!("target_track_gate_width_px", r.target_track_gate_width_px);
    op!("target_track_gate_height_px", r.target_track_gate_height_px);
    op!("target_error_ce90_m", r.target_error_ce90_m);
    op!("target_error_le90_m", r.target_error_le90_m);
    op!("wind_direction_deg", r.wind_direction_deg);
    op!("wind_speed", r.wind_speed);
    op!("static_pressure_mbar", r.static_pressure_mbar);
    op!("density_altitude_m", r.density_altitude_m);
    op!("differential_pressure_mbar", r.differential_pressure_mbar);
    op!(
        "airfield_barometric_pressure_mbar",
        r.airfield_barometric_pressure_mbar
    );
    op!("airfield_elevation_m", r.airfield_elevation_m);
    op!("relative_humidity_pct", r.relative_humidity_pct);
    op!("platform_vertical_speed", r.platform_vertical_speed);
    op!("platform_sideslip_deg", r.platform_sideslip_deg);
    op!("platform_ground_speed", r.platform_ground_speed);
    op!("ground_range_m", r.ground_range_m);
    op!("platform_fuel_remaining_kg", r.platform_fuel_remaining_kg);
    op!(
        "platform_magnetic_heading_deg",
        r.platform_magnetic_heading_deg
    );
    op!(
        "platform_angle_of_attack_full_deg",
        r.platform_angle_of_attack_full_deg
    );
    op!("platform_sideslip_full_deg", r.platform_sideslip_full_deg);
    op!("alternate_platform_lat_deg", r.alternate_platform_lat_deg);
    op!("alternate_platform_lon_deg", r.alternate_platform_lon_deg);
    op!("alternate_platform_alt_m", r.alternate_platform_alt_m);
    op!(
        "alternate_platform_heading_deg",
        r.alternate_platform_heading_deg
    );
    op!(
        "alternate_platform_ellipsoid_height_m",
        r.alternate_platform_ellipsoid_height_m
    );
    op!("sensor_north_velocity", r.sensor_north_velocity);
    op!("sensor_east_velocity", r.sensor_east_velocity);

    // WP-B Table B1 — IMAPB f64 fields (tags 96, 103-105, 109, 112-114,
    // 117-120, 132, 134).
    op!("target_width_extended_m", r.target_width_extended_m);
    op!("density_altitude_extended_m", r.density_altitude_extended_m);
    op!(
        "sensor_ellipsoid_height_extended_m",
        r.sensor_ellipsoid_height_extended_m
    );
    op!(
        "alternate_platform_ellipsoid_height_extended_m",
        r.alternate_platform_ellipsoid_height_extended_m
    );
    op!("range_to_recovery_km", r.range_to_recovery_km);
    op!("platform_course_angle_deg", r.platform_course_angle_deg);
    op!("altitude_agl_m", r.altitude_agl_m);
    op!("radar_altimeter_m", r.radar_altimeter_m);
    op!("sensor_azimuth_rate_dps", r.sensor_azimuth_rate_dps);
    op!("sensor_elevation_rate_dps", r.sensor_elevation_rate_dps);
    op!("sensor_roll_rate_dps", r.sensor_roll_rate_dps);
    op!("mi_storage_percent_full", r.mi_storage_percent_full);
    op!("transmission_frequency_mhz", r.transmission_frequency_mhz);
    op!("zoom_percentage", r.zoom_percentage);

    // WP-B Table B2 — var-length int/enum fields (tags 110-139).
    op!("time_airborne_s", r.time_airborne_s);
    op!("propulsion_unit_speed_rpm", r.propulsion_unit_speed_rpm);
    op!("navsats_in_view", r.navsats_in_view);
    op!("positioning_method_source", r.positioning_method_source);
    if let Some(v) = r.platform_status {
        kwargs.set_item("platform_status", convert_platform_status(py, v)?)?;
    }
    if let Some(v) = r.sensor_control_mode {
        kwargs.set_item("sensor_control_mode", convert_sensor_control_mode(py, v)?)?;
    }
    op!("take_off_time_us", r.take_off_time_us);
    op!("mi_storage_capacity_gb", r.mi_storage_capacity_gb);
    op!("leap_seconds", r.leap_seconds);
    op!("correction_offset_us", r.correction_offset_us);
    ob!("active_payloads", r.active_payloads);

    // WP-A Table A4 — named nested-set raw fields (tags 73, 95, 97-101).
    ob!("rvt", r.rvt);
    ob!("sar_mi_local_set", r.sar_mi_local_set);
    ob!("range_image_local_set", r.range_image_local_set);
    ob!("geo_registration_local_set", r.geo_registration_local_set);
    ob!("composite_imaging_local_set", r.composite_imaging_local_set);
    ob!("segment_local_set", r.segment_local_set);
    ob!("amend_local_set", r.amend_local_set);

    // WP-A Table A2 — raw/simple scalar + string fields (tags 39, 60-62,
    // 70, 72, 106-108, 129, 135).
    op!("outside_air_temp_c", r.outside_air_temp_c);
    op!("weapon_load", r.weapon_load);
    op!("weapon_fired", r.weapon_fired);
    op!("laser_prf_code", r.laser_prf_code);
    os!("alternate_platform_name", r.alternate_platform_name);
    op!("event_start_time_us", r.event_start_time_us);
    os!("stream_designator", r.stream_designator);
    os!("operational_base", r.operational_base);
    os!("broadcast_source", r.broadcast_source);
    os!("target_id", r.target_id);
    os!("communications_method", r.communications_method);

    // WP-A Table A3 — coded enums (tags 34, 63, 77). Known codepoints
    // become a Python enum instance; wire-unknown `Other(code)` becomes a
    // raw int (mirrors the ST 0102 `SecurityClassification::Unknown(b)`
    // asymmetry).
    if let Some(v) = r.icing_detected {
        kwargs.set_item("icing_detected", convert_icing_detected(py, v)?)?;
    }
    if let Some(v) = r.sensor_fov_name {
        kwargs.set_item("sensor_fov_name", convert_sensor_fov_name(py, v)?)?;
    }
    if let Some(v) = r.operational_mode {
        kwargs.set_item("operational_mode", convert_operational_mode(py, v)?)?;
    }

    // WP-C Table C1 — pack & list items (tags 81/102/115/116/121/122/
    // 127/128/130/138/140/141/142/143).
    if let Some(h) = r.image_horizon {
        kwargs.set_item("image_horizon", convert_image_horizon(py, &h)?)?;
    }
    let control_commands: Vec<PyObject> = r
        .control_commands
        .iter()
        .map(|c| convert_control_command(py, c))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item(
        "control_commands",
        pyo3::types::PyTuple::new_bound(py, control_commands),
    )?;
    if let Some(ids) = &r.control_command_verification {
        kwargs.set_item(
            "control_command_verification",
            pyo3::types::PyTuple::new_bound(py, ids.iter().map(|&v| v.into_py(py))),
        )?;
    }
    if let Some(ids) = &r.active_wavelengths {
        kwargs.set_item(
            "active_wavelengths",
            pyo3::types::PyTuple::new_bound(py, ids.iter().map(|&v| v.into_py(py))),
        )?;
    }
    if let Some(fr) = r.sensor_frame_rate {
        kwargs.set_item("sensor_frame_rate", convert_sensor_frame_rate(py, &fr)?)?;
    }
    if let Some(ms) = r.metadata_substream_id {
        kwargs.set_item(
            "metadata_substream_id",
            convert_metadata_substream_id(py, &ms)?,
        )?;
    }
    if let Some(cc) = &r.country_codes {
        kwargs.set_item("country_codes", convert_country_codes(py, cc)?)?;
    }
    if let Some(list) = &r.wavelengths_list {
        let items: Vec<PyObject> = list
            .iter()
            .map(|w| convert_wavelength_record(py, w))
            .collect::<PyResult<Vec<_>>>()?;
        kwargs.set_item(
            "wavelengths_list",
            pyo3::types::PyTuple::new_bound(py, items),
        )?;
    }
    if let Some(al) = r.airbase_locations {
        kwargs.set_item("airbase_locations", convert_airbase_locations(py, &al)?)?;
    }
    if let Some(pl) = &r.payload_list {
        kwargs.set_item("payload_list", convert_payload_list(py, pl)?)?;
    }
    if let Some(list) = &r.weapons_stores {
        let items: Vec<PyObject> = list
            .iter()
            .map(|w| convert_weapons_store(py, w))
            .collect::<PyResult<Vec<_>>>()?;
        kwargs.set_item("weapons_stores", pyo3::types::PyTuple::new_bound(py, items))?;
    }
    if let Some(list) = &r.waypoint_list {
        let items: Vec<PyObject> = list
            .iter()
            .map(|w| convert_waypoint(py, w))
            .collect::<PyResult<Vec<_>>>()?;
        kwargs.set_item("waypoint_list", pyo3::types::PyTuple::new_bound(py, items))?;
    }
    if let Some(vd) = r.view_domain {
        kwargs.set_item("view_domain", convert_view_domain(py, &vd)?)?;
    }
    let sdcc_flps: Vec<PyObject> = r
        .sdcc_flps
        .iter()
        .map(|f| convert_sdcc_flp_field(py, f))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item("sdcc_flps", pyo3::types::PyTuple::new_bound(py, sdcc_flps))?;

    kwargs.set_item("unknown", convert_unknown(py, &r.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &r.field_errors)?)?;
    let sentinel_tuple =
        pyo3::types::PyTuple::new_bound(py, r.sentinel_tags.iter().map(|&t| t as u64));
    kwargs.set_item("sentinel_tags", sentinel_tuple)?;

    // imapb_specials: Vec<(u32, ImapbSpecial)> -> tuple[tuple[int, str, int], ...].
    let specials_items: Vec<PyObject> = r
        .imapb_specials
        .iter()
        .map(|&(tag, special)| {
            let (code, payload) = imapb_special_to_code(special)?;
            PyResult::Ok(
                pyo3::types::PyTuple::new_bound(
                    py,
                    &[tag.into_py(py), code.into_py(py), payload.into_py(py)],
                )
                .unbind()
                .into(),
            )
        })
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item(
        "imapb_specials",
        pyo3::types::PyTuple::new_bound(py, specials_items),
    )?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Decode an ST 0601 UAS Datalink LS. `buf` is the full wire-format
/// payload starting with the 16-byte Universal Label.
///
/// - Default (lenient): any 16-byte UL accepted, checksum verified,
///   field-level malformations collected in `.field_errors`.
/// - `strict=True`: also requires the ST 0601 family UL pattern.
/// - `compliance=True`: also enforces Tag 2 first / Tag 1 last /
///   Tag 65 present / no duplicate tags / canonical BER. Implies
///   `strict=True`.
#[pyfunction]
#[pyo3(
    name = "decode_uas_datalink",
    signature = (buf, *, strict = false, compliance = false)
)]
fn decode_uas_datalink_py(
    py: Python<'_>,
    buf: &[u8],
    strict: bool,
    compliance: bool,
) -> PyResult<PyObject> {
    // Audit #11 / GIL-release decision: NOT wrapped — same rationale as
    // `decode_precision_timestamp_py`. Even at the upper end (~10 KB
    // record with 100 unknown TLVs), Rust decode is ~10us per call,
    // below the GIL transition breakeven. Worse, the 80-field
    // `convert_uas_datalink_ls` projection dominates per-call CPU
    // time, so releasing the GIL for the small Rust slice produces
    // GIL ping-pong under hot batch loops (measured: 30K decodes
    // taking 50+ seconds under contention vs 0.5s without the wrap).
    let result = if compliance {
        decode_st0601_strict_compliance(buf)
    } else if strict {
        decode_st0601_strict(buf)
    } else {
        decode_st0601_lenient(buf)
    };
    match result {
        Ok(rec) => convert_uas_datalink_ls(py, &rec),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// ST 0601 — Python → Rust inverse translator + encode entry points
// ---------------------------------------------------------------------------

/// Inverse of `convert_uas_datalink_ls`: extracts every field from a
/// Python `tstrans.klv.UasDatalinkLs` dataclass into a Rust
/// `UasDatalinkLs`.
///
/// `field_errors` is a parser-only diagnostic and is not round-tripped.
///
/// `unknown` IS round-tripped (audit #5): forward-compat TLVs preserved
/// by the ST 0601 decoder flow back into the encoder. Entries whose tag
/// collides with a typed field (see `is_st0601_typed_tag`) are silently
/// dropped — typed wins. Without this filter, ST 0601's encoder would
/// reject the call with `KlvEncodeError::ReservedTagInUnknown`; dropping
/// at the boundary lets `decode -> encode -> decode` succeed for any
/// record the decoder produced.
#[allow(clippy::cognitive_complexity)]
fn py_to_uas_datalink_ls(p: &Bound<'_, PyAny>) -> PyResult<UasDatalinkLs> {
    let mut r = UasDatalinkLs::default();

    // universal_label: 16-byte bytes → UniversalLabel. Any other length
    // is a caller bug; raise instead of silently leaving the field at the
    // default 16-byte zero UL (audit #6).
    let ul_bytes: Vec<u8> = p.getattr(intern!(p.py(), "universal_label"))?.extract()?;
    if ul_bytes.len() != 16 {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "universal_label must be 16 bytes, got {}",
            ul_bytes.len()
        )));
    }
    let mut ul = [0u8; 16];
    ul.copy_from_slice(&ul_bytes);
    r.universal_label = UniversalLabel(ul);

    // declared_version: u8 in Rust, int in Python
    r.declared_version = p.getattr(intern!(p.py(), "declared_version"))?.extract()?;

    // Optional<String>
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(p.py(), stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }
    // Optional<scalar> — works for u8 / u64 / f64 fields
    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(p.py(), stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    // Optional<Vec<u8>>
    macro_rules! ob {
        ($field:ident) => {
            if let Some(b) = p
                .getattr(intern!(p.py(), stringify!($field)))?
                .extract::<Option<Vec<u8>>>()?
            {
                r.$field = Some(b);
            }
        };
    }

    os!(mission_id);
    os!(platform_tail_number);
    os!(platform_designation);
    os!(image_source_sensor);
    os!(image_coordinate_system);
    os!(platform_call_sign);
    op!(uas_ls_version, u8);
    op!(timestamp_us, u64);
    op!(platform_heading_deg, f64);
    op!(platform_pitch_deg, f64);
    op!(platform_roll_deg, f64);
    op!(platform_true_airspeed, f64);
    op!(platform_indicated_airspeed, f64);
    op!(platform_pitch_full_deg, f64);
    op!(platform_roll_full_deg, f64);
    op!(platform_angle_of_attack_deg, f64);
    op!(sensor_lat_deg, f64);
    op!(sensor_lon_deg, f64);
    op!(sensor_alt_m, f64);
    op!(sensor_ellipsoid_height_m, f64);
    op!(sensor_hfov_deg, f64);
    op!(sensor_vfov_deg, f64);
    op!(sensor_rel_az_deg, f64);
    op!(sensor_rel_el_deg, f64);
    op!(sensor_rel_roll_deg, f64);
    op!(slant_range_m, f64);
    op!(target_width_m, f64);
    op!(frame_center_lat_deg, f64);
    op!(frame_center_lon_deg, f64);
    op!(frame_center_elev_m, f64);
    op!(frame_center_ellipsoid_height_m, f64);
    op!(corner_lat_offset_p1_deg, f64);
    op!(corner_lon_offset_p1_deg, f64);
    op!(corner_lat_offset_p2_deg, f64);
    op!(corner_lon_offset_p2_deg, f64);
    op!(corner_lat_offset_p3_deg, f64);
    op!(corner_lon_offset_p3_deg, f64);
    op!(corner_lat_offset_p4_deg, f64);
    op!(corner_lon_offset_p4_deg, f64);
    op!(corner_lat_p1_deg, f64);
    op!(corner_lon_p1_deg, f64);
    op!(corner_lat_p2_deg, f64);
    op!(corner_lon_p2_deg, f64);
    op!(corner_lat_p3_deg, f64);
    op!(corner_lon_p3_deg, f64);
    op!(corner_lat_p4_deg, f64);
    op!(corner_lon_p4_deg, f64);
    op!(generic_flag_data, u8);
    ob!(security_local_set);
    ob!(vmti);
    ob!(miis_core_id);

    // WP-A Table A1 — ranged f64 fields.
    op!(target_location_lat_deg, f64);
    op!(target_location_lon_deg, f64);
    op!(target_location_elev_m, f64);
    op!(target_track_gate_width_px, f64);
    op!(target_track_gate_height_px, f64);
    op!(target_error_ce90_m, f64);
    op!(target_error_le90_m, f64);
    op!(wind_direction_deg, f64);
    op!(wind_speed, f64);
    op!(static_pressure_mbar, f64);
    op!(density_altitude_m, f64);
    op!(differential_pressure_mbar, f64);
    op!(airfield_barometric_pressure_mbar, f64);
    op!(airfield_elevation_m, f64);
    op!(relative_humidity_pct, f64);
    op!(platform_vertical_speed, f64);
    op!(platform_sideslip_deg, f64);
    op!(platform_ground_speed, f64);
    op!(ground_range_m, f64);
    op!(platform_fuel_remaining_kg, f64);
    op!(platform_magnetic_heading_deg, f64);
    op!(platform_angle_of_attack_full_deg, f64);
    op!(platform_sideslip_full_deg, f64);
    op!(alternate_platform_lat_deg, f64);
    op!(alternate_platform_lon_deg, f64);
    op!(alternate_platform_alt_m, f64);
    op!(alternate_platform_heading_deg, f64);
    op!(alternate_platform_ellipsoid_height_m, f64);
    op!(sensor_north_velocity, f64);
    op!(sensor_east_velocity, f64);

    // WP-B Table B1 — IMAPB f64 fields.
    op!(target_width_extended_m, f64);
    op!(density_altitude_extended_m, f64);
    op!(sensor_ellipsoid_height_extended_m, f64);
    op!(alternate_platform_ellipsoid_height_extended_m, f64);
    op!(range_to_recovery_km, f64);
    op!(platform_course_angle_deg, f64);
    op!(altitude_agl_m, f64);
    op!(radar_altimeter_m, f64);
    op!(sensor_azimuth_rate_dps, f64);
    op!(sensor_elevation_rate_dps, f64);
    op!(sensor_roll_rate_dps, f64);
    op!(mi_storage_percent_full, f64);
    op!(transmission_frequency_mhz, f64);
    op!(zoom_percentage, f64);

    // WP-B Table B2 — var-length int/enum fields.
    op!(time_airborne_s, u32);
    op!(propulsion_unit_speed_rpm, u32);
    op!(navsats_in_view, u8);
    op!(positioning_method_source, u8);
    let platform_status_obj = p.getattr(intern!(p.py(), "platform_status"))?;
    if !platform_status_obj.is_none() {
        r.platform_status = Some(platform_status_from_wire(enum_field_to_u8(
            &platform_status_obj,
        )?));
    }
    let sensor_control_mode_obj = p.getattr(intern!(p.py(), "sensor_control_mode"))?;
    if !sensor_control_mode_obj.is_none() {
        r.sensor_control_mode = Some(sensor_control_mode_from_wire(enum_field_to_u8(
            &sensor_control_mode_obj,
        )?));
    }
    op!(take_off_time_us, u64);
    op!(mi_storage_capacity_gb, u32);
    op!(leap_seconds, i32);
    op!(correction_offset_us, i64);
    ob!(active_payloads);

    // WP-A Table A4 — named nested-set raw fields.
    ob!(rvt);
    ob!(sar_mi_local_set);
    ob!(range_image_local_set);
    ob!(geo_registration_local_set);
    ob!(composite_imaging_local_set);
    ob!(segment_local_set);
    ob!(amend_local_set);

    // WP-A Table A2 — raw/simple scalar + string fields.
    op!(outside_air_temp_c, i8);
    op!(weapon_load, u16);
    op!(weapon_fired, u8);
    op!(laser_prf_code, u16);
    os!(alternate_platform_name);
    op!(event_start_time_us, u64);
    os!(stream_designator);
    os!(operational_base);
    os!(broadcast_source);
    os!(target_id);
    os!(communications_method);

    // WP-A Table A3 — coded enums. `enum_field_to_u8` accepts either a
    // Python enum instance or a raw int (wire-unknown pass-through).
    let icing_obj = p.getattr(intern!(p.py(), "icing_detected"))?;
    if !icing_obj.is_none() {
        r.icing_detected = Some(icing_detected_from_wire(enum_field_to_u8(&icing_obj)?));
    }
    let fov_obj = p.getattr(intern!(p.py(), "sensor_fov_name"))?;
    if !fov_obj.is_none() {
        r.sensor_fov_name = Some(sensor_fov_name_from_wire(enum_field_to_u8(&fov_obj)?));
    }
    let opmode_obj = p.getattr(intern!(p.py(), "operational_mode"))?;
    if !opmode_obj.is_none() {
        r.operational_mode = Some(operational_mode_from_wire(enum_field_to_u8(&opmode_obj)?));
    }

    // WP-C Table C1 — pack & list items.
    macro_rules! ou64vec {
        ($field:ident) => {
            if let Some(v) = p
                .getattr(intern!(p.py(), stringify!($field)))?
                .extract::<Option<Vec<u64>>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    let image_horizon_obj = p.getattr(intern!(p.py(), "image_horizon"))?;
    if !image_horizon_obj.is_none() {
        r.image_horizon = Some(py_to_image_horizon(&image_horizon_obj)?);
    }
    for c in p.getattr(intern!(p.py(), "control_commands"))?.iter()? {
        r.control_commands.push(py_to_control_command(&c?)?);
    }
    ou64vec!(control_command_verification);
    ou64vec!(active_wavelengths);
    let sensor_frame_rate_obj = p.getattr(intern!(p.py(), "sensor_frame_rate"))?;
    if !sensor_frame_rate_obj.is_none() {
        r.sensor_frame_rate = Some(py_to_sensor_frame_rate(&sensor_frame_rate_obj)?);
    }
    let metadata_substream_id_obj = p.getattr(intern!(p.py(), "metadata_substream_id"))?;
    if !metadata_substream_id_obj.is_none() {
        r.metadata_substream_id = Some(py_to_metadata_substream_id(&metadata_substream_id_obj)?);
    }
    let country_codes_obj = p.getattr(intern!(p.py(), "country_codes"))?;
    if !country_codes_obj.is_none() {
        r.country_codes = Some(py_to_country_codes(&country_codes_obj)?);
    }
    let wavelengths_list_obj = p.getattr(intern!(p.py(), "wavelengths_list"))?;
    if !wavelengths_list_obj.is_none() {
        let mut list = Vec::new();
        for w in wavelengths_list_obj.iter()? {
            list.push(py_to_wavelength_record(&w?)?);
        }
        r.wavelengths_list = Some(list);
    }
    let airbase_locations_obj = p.getattr(intern!(p.py(), "airbase_locations"))?;
    if !airbase_locations_obj.is_none() {
        r.airbase_locations = Some(py_to_airbase_locations(&airbase_locations_obj)?);
    }
    let payload_list_obj = p.getattr(intern!(p.py(), "payload_list"))?;
    if !payload_list_obj.is_none() {
        r.payload_list = Some(py_to_payload_list(&payload_list_obj)?);
    }
    let weapons_stores_obj = p.getattr(intern!(p.py(), "weapons_stores"))?;
    if !weapons_stores_obj.is_none() {
        let mut list = Vec::new();
        for w in weapons_stores_obj.iter()? {
            list.push(py_to_weapons_store(&w?)?);
        }
        r.weapons_stores = Some(list);
    }
    let waypoint_list_obj = p.getattr(intern!(p.py(), "waypoint_list"))?;
    if !waypoint_list_obj.is_none() {
        let mut list = Vec::new();
        for w in waypoint_list_obj.iter()? {
            list.push(py_to_waypoint(&w?)?);
        }
        r.waypoint_list = Some(list);
    }
    let view_domain_obj = p.getattr(intern!(p.py(), "view_domain"))?;
    if !view_domain_obj.is_none() {
        r.view_domain = Some(py_to_view_domain(&view_domain_obj)?);
    }
    for f in p.getattr(intern!(p.py(), "sdcc_flps"))?.iter()? {
        r.sdcc_flps.push(py_to_sdcc_flp_field(&f?)?);
    }

    r.unknown = py_to_unknown(p, is_st0601_typed_tag)?;

    // sentinel_tags: tuple[int, ...] → Vec<u32>. Extracting u32 directly
    // raises OverflowError on out-of-range values instead of truncating.
    r.sentinel_tags = p
        .getattr(intern!(p.py(), "sentinel_tags"))?
        .extract::<Vec<u32>>()?;

    // imapb_specials: tuple[tuple[int, str, int], ...] → Vec<(u32, ImapbSpecial)>.
    let specials_obj = p.getattr(intern!(p.py(), "imapb_specials"))?;
    for item in specials_obj.iter()? {
        let (tag, code, payload): (u32, String, u64) = item?.extract()?;
        r.imapb_specials
            .push((tag, imapb_special_from_code(&code, payload)?));
    }

    Ok(r)
}

// ---------------------------------------------------------------------------
// OutOfRangePolicy — IntEnum-shaped frozen PyClass.
// ---------------------------------------------------------------------------

/// Policy for ranged values that fall outside their ST 0601 mapped range
/// during encoding.
///
/// - ``ERROR`` (default): raise ``KlvEncodeError(OUT_OF_RANGE)`` — the
///   encoder never silently alters the caller's data.
/// - ``INDICATOR``: emit the tag's spec-defined Out-of-Range special value
///   instead of raising. This applies only to the tags whose INT_MIN sentinel
///   (``0x8000`` for 2-byte, ``0x80000000`` for 4-byte signed mappings) means
///   "Out of Range" per ST 0601.19 §7.5 / requirement ST 0601.13-27: Tags 6,
///   7, 50, 51, 52, 79, 80, 90-93 — all of which are encodable
///   ``UasDatalinkLs`` fields (platform pitch / roll / angle-of-attack /
///   vertical speed / sideslip, sensor north / east velocity, and the
///   full-range pitch / roll / angle-of-attack / sideslip twins). All other
///   fields, and any non-finite value, still raise even under ``INDICATOR``.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[pyclass(name = "OutOfRangePolicy", module = "tstrans.klv", eq, eq_int, frozen)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyOutOfRangePolicy {
    ERROR = 0,
    INDICATOR = 1,
}

impl From<PyOutOfRangePolicy> for RustOutOfRangePolicy {
    fn from(p: PyOutOfRangePolicy) -> Self {
        match p {
            PyOutOfRangePolicy::ERROR => RustOutOfRangePolicy::Error,
            PyOutOfRangePolicy::INDICATOR => RustOutOfRangePolicy::Indicator,
        }
    }
}

/// Encode a Python `UasDatalinkLs` to wire bytes (lenient — emits only
/// the populated fields, no mandatory-tag enforcement). Returns
/// `bytes` containing the 16-byte UL + BER length + body.
///
/// The optional keyword-only `out_of_range_policy` (default: `ERROR`) controls
/// how values outside their ST 0601 mapped range are handled — see
/// `OutOfRangePolicy` for details.
#[pyfunction]
#[pyo3(name = "encode_uas_datalink", signature = (record, *, out_of_range_policy = None))]
fn encode_uas_datalink_py(
    py: Python<'_>,
    record: &Bound<'_, PyAny>,
    out_of_range_policy: Option<PyOutOfRangePolicy>,
) -> PyResult<PyObject> {
    let rust_rec = py_to_uas_datalink_ls(record)?;
    let mut opts = St0601EncodeConfig::default();
    if let Some(p) = out_of_range_policy {
        opts.out_of_range_policy = p.into();
    }
    let bytes =
        encode_st0601_with(&rust_rec, &opts).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `UasDatalinkLs` to wire bytes with strict compliance
/// per ST 0601.19 — requires Tag 2 (precision timestamp), Tag 1
/// (checksum slot — synthesized), and Tag 65 (version). Raises
/// `KlvEncodeError(MISSING_MANDATORY_ITEM)` if a required tag is absent.
#[pyfunction]
#[pyo3(name = "encode_uas_datalink_strict_compliance")]
fn encode_uas_datalink_strict_compliance_py(
    py: Python<'_>,
    record: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    let rust_rec = py_to_uas_datalink_ls(record)?;
    let bytes =
        encode_st0601_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Byte-faithful tag-level patch of a raw ST 0601 local set: edited
/// tags re-encoded, every other TLV copied verbatim, checksum
/// recomputed. See `tstrans.klv.patch_uas_datalink` for the
/// dict-accepting user-facing wrapper.
///
/// No `allow_threads` for the same reason as `decode_uas_datalink`:
/// the per-call Rust work is small; releasing the GIL produces
/// ping-pong under hot batch loops.
#[pyfunction]
#[pyo3(name = "patch_uas_datalink")]
fn patch_uas_datalink_py(
    py: Python<'_>,
    raw: &[u8],
    edits: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    let rust_edits = py_to_uas_datalink_ls(edits)?;
    match patch_st0601(raw, &rust_edits) {
        Ok(bytes) => Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into()),
        Err(KlvPatchError::Decode(e)) => Err(klv_decode_error_to_pyerr(py, e)),
        Err(KlvPatchError::Encode(e)) => Err(klv_encode_error_to_pyerr(py, e)),
        // KlvPatchError is #[non_exhaustive] in tst-core, so a wildcard
        // arm is required from this crate.
        Err(other) => Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "unhandled KlvPatchError variant: {other}"
        ))),
    }
}

/// Look up the ST 0601.19 spec-defined meaning of the INT_MIN sentinel
/// wire value for `tag`. Returns `"out_of_range"`, `"reserved"`,
/// `"not_available"`, or `None` if the spec assigns no INT_MIN special
/// value for that tag (this does NOT mean the tag is unsigned or that
/// INT_MIN is a valid wire value for that tag). See
/// `UasDatalinkLs.sentinel_tags` for where this lookup applies.
#[pyfunction]
#[pyo3(name = "st0601_sentinel_meaning")]
fn st0601_sentinel_meaning_py(tag: u32) -> Option<&'static str> {
    let meaning = rust_st0601_sentinel_meaning(tag)?;
    Some(match meaning {
        St0601SentinelMeaning::OutOfRange => "out_of_range",
        St0601SentinelMeaning::Reserved => "reserved",
        St0601SentinelMeaning::NotAvailable => "not_available",
        // #[non_exhaustive] in tst-core; no current variant reaches here.
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// ST 1204.3 — MIIS Core Identifier
// ---------------------------------------------------------------------------

/// Translate a Rust `IdType` to the matching Python `tstrans.klv.IdType`
/// enum member. Returns an error if an unknown variant is encountered.
fn convert_id_type(py: Python<'_>, ty: RustIdType) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "IdType"))?;
    let variant = match ty {
        RustIdType::Physical => "PHYSICAL",
        RustIdType::Virtual => "VIRTUAL",
        RustIdType::Managed => "MANAGED",
        _ => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "unknown IdType variant crossing the binding",
            ));
        }
    };
    Ok(cls.getattr(variant)?.unbind())
}

/// Translate a Rust `CoreId` to a Python `tstrans.klv.CoreId` dataclass.
fn convert_core_id(py: Python<'_>, id: &RustCoreId) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "CoreId"))?;
    let kwargs = PyDict::new_bound(py);

    kwargs.set_item("version", id.version)?;

    if let Some((ref ty, ref uuid)) = id.sensor {
        let id_type_py = convert_id_type(py, *ty)?;
        let uuid_bytes = pyo3::types::PyBytes::new_bound(py, uuid.as_slice());
        let tuple = pyo3::types::PyTuple::new_bound(py, [id_type_py, uuid_bytes.unbind().into()]);
        kwargs.set_item("sensor", tuple)?;
    }

    if let Some((ref ty, ref uuid)) = id.platform {
        let id_type_py = convert_id_type(py, *ty)?;
        let uuid_bytes = pyo3::types::PyBytes::new_bound(py, uuid.as_slice());
        let tuple = pyo3::types::PyTuple::new_bound(py, [id_type_py, uuid_bytes.unbind().into()]);
        kwargs.set_item("platform", tuple)?;
    }

    if let Some(ref uuid) = id.window {
        kwargs.set_item(
            "window",
            pyo3::types::PyBytes::new_bound(py, uuid.as_slice()),
        )?;
    }
    if let Some(ref uuid) = id.minor {
        kwargs.set_item(
            "minor",
            pyo3::types::PyBytes::new_bound(py, uuid.as_slice()),
        )?;
    }

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Inverse of `convert_id_type`: extract an `IdType` from a Python
/// `tstrans.klv.IdType` enum member.
fn py_to_id_type(p: &Bound<'_, PyAny>) -> PyResult<RustIdType> {
    let name: String = p.getattr("name")?.extract()?;
    match name.as_str() {
        "PHYSICAL" => Ok(RustIdType::Physical),
        "VIRTUAL" => Ok(RustIdType::Virtual),
        "MANAGED" => Ok(RustIdType::Managed),
        other => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "unknown IdType variant: {other}"
        ))),
    }
}

/// Inverse of `convert_core_id`: extract a Rust `CoreId` from a Python
/// `tstrans.klv.CoreId` dataclass.
fn py_to_core_id(p: &Bound<'_, PyAny>) -> PyResult<RustCoreId> {
    let py = p.py();

    let version: u8 = p.getattr(intern!(py, "version"))?.extract()?;

    let sensor_obj = p.getattr(intern!(py, "sensor"))?;
    let sensor = if sensor_obj.is_none() {
        None
    } else {
        let (ty_py, uuid_bytes): (Bound<'_, PyAny>, Vec<u8>) = sensor_obj.extract()?;
        let ty = py_to_id_type(&ty_py)?;
        if uuid_bytes.len() != 16 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "CoreId.sensor UUID must be 16 bytes, got {}",
                uuid_bytes.len()
            )));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&uuid_bytes);
        Some((ty, uuid))
    };

    let platform_obj = p.getattr(intern!(py, "platform"))?;
    let platform = if platform_obj.is_none() {
        None
    } else {
        let (ty_py, uuid_bytes): (Bound<'_, PyAny>, Vec<u8>) = platform_obj.extract()?;
        let ty = py_to_id_type(&ty_py)?;
        if uuid_bytes.len() != 16 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "CoreId.platform UUID must be 16 bytes, got {}",
                uuid_bytes.len()
            )));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&uuid_bytes);
        Some((ty, uuid))
    };

    let window_obj = p.getattr(intern!(py, "window"))?;
    let window = if window_obj.is_none() {
        None
    } else {
        let bytes: Vec<u8> = window_obj.extract()?;
        if bytes.len() != 16 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "CoreId.window UUID must be 16 bytes, got {}",
                bytes.len()
            )));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes);
        Some(uuid)
    };

    let minor_obj = p.getattr(intern!(py, "minor"))?;
    let minor = if minor_obj.is_none() {
        None
    } else {
        let bytes: Vec<u8> = minor_obj.extract()?;
        if bytes.len() != 16 {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "CoreId.minor UUID must be 16 bytes, got {}",
                bytes.len()
            )));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes);
        Some(uuid)
    };

    Ok(RustCoreId::new(version, sensor, platform, window, minor))
}

/// Decode a MISB ST 1204.3 MIIS Core Identifier from binary wire form.
/// `buf` must be exactly the bytes of one Core Identifier (no framing).
/// Raises `KlvError` on any decode failure.
#[pyfunction]
#[pyo3(name = "decode_core_id")]
fn decode_core_id_py(py: Python<'_>, buf: &[u8]) -> PyResult<PyObject> {
    match decode_st1204(buf) {
        Ok(id) => convert_core_id(py, &id),
        Err(e) => Err(st1204_error_to_pyerr(py, e)),
    }
}

/// Encode a Python `CoreId` to its binary wire form. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_core_id")]
fn encode_core_id_py(py: Python<'_>, core_id: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_id = py_to_core_id(core_id)?;
    let bytes = encode_st1204(&rust_id);
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Return the ST 1204.3 §7.4.2 textual representation of a `CoreId`.
#[pyfunction]
#[pyo3(name = "core_id_text")]
fn core_id_text_py(_py: Python<'_>, core_id: &Bound<'_, PyAny>) -> PyResult<String> {
    let rust_id = py_to_core_id(core_id)?;
    Ok(rust_id.to_string())
}

// ---------------------------------------------------------------------------
// ST 0902.8 MISMMS validator
// ---------------------------------------------------------------------------

/// Translate a Rust `MismmsViolation` to a Python `tstrans.klv.MismmsViolation`
/// dataclass instance.
fn convert_mismms_violation(py: Python<'_>, v: &RustMismmsViolation) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "MismmsViolation"))?;
    let kwargs = PyDict::new_bound(py);

    match v {
        RustMismmsViolation::MissingItem { tag, name } => {
            kwargs.set_item("kind", "missing")?;
            kwargs.set_item("tag", *tag as u32)?;
            kwargs.set_item("name", *name)?;
        }
        RustMismmsViolation::MissingSecurityItem { tag, name } => {
            kwargs.set_item("kind", "missing_security")?;
            kwargs.set_item("tag", *tag as u32)?;
            kwargs.set_item("name", *name)?;
        }
        RustMismmsViolation::ZeroLengthItem { tag } => {
            kwargs.set_item("kind", "zero_length")?;
            kwargs.set_item("tag", *tag as u32)?;
        }
        RustMismmsViolation::AlternationConflict { tag_a, tag_b } => {
            kwargs.set_item("kind", "alternation_conflict")?;
            kwargs.set_item("tag", *tag_a as u32)?;
            kwargs.set_item("tag_b", *tag_b as u32)?;
        }
        _ => {
            return Err(PyValueError::new_err(
                "unknown MismmsViolation variant crossing the binding",
            ));
        }
    }

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Validate a Python `UasDatalinkLs` record against the ST 0902.8 Minimum
/// Metadata Set (Table 1). Returns a `list[MismmsViolation]`; an empty list
/// means the record satisfies every MISMMS requirement at the record level.
///
/// Reuses `py_to_uas_datalink_ls` — the same converter used by
/// `encode_uas_datalink` — so the Rust-side check sees an identical record.
#[pyfunction]
#[pyo3(name = "validate_mismms")]
fn validate_mismms_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_uas_datalink_ls(record)?;
    let violations = rust_validate_mismms(&rust_rec);
    let items: Vec<PyObject> = violations
        .iter()
        .map(|v| convert_mismms_violation(py, v))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyList::new_bound(py, items).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0806.4 — RVT (Remote Video Terminal) Local Set
// ---------------------------------------------------------------------------

/// Translate a Rust `RvtPoiType` to the matching Python
/// `tstrans.klv.RvtPoiType` enum instance for known codepoints, or a raw
/// `int` for `Other(code)` (wire-unknown, round-trips byte-exact) —
/// mirrors the ST 0601 `IcingDetected` asymmetry.
fn convert_rvt_poi_type(py: Python<'_>, v: RustRvtPoiType) -> PyResult<PyObject> {
    let code = match v {
        RustRvtPoiType::Friendly => 1u8,
        RustRvtPoiType::Hostile => 2,
        RustRvtPoiType::Target => 3,
        RustRvtPoiType::Unknown => 4,
        RustRvtPoiType::Other(b) => return Ok(b.into_py(py)),
        // #[non_exhaustive] in tst-core: a wildcard is required even
        // though every current variant is covered above.
        _ => {
            return Err(PyValueError::new_err(
                "unknown RvtPoiType variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtPoiType"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_rvt_poi_type`.
fn rvt_poi_type_from_wire(b: u8) -> RustRvtPoiType {
    match b {
        1 => RustRvtPoiType::Friendly,
        2 => RustRvtPoiType::Hostile,
        3 => RustRvtPoiType::Target,
        4 => RustRvtPoiType::Unknown,
        other => RustRvtPoiType::Other(other),
    }
}

/// Translate a Rust `RvtAoiType` to the matching Python
/// `tstrans.klv.RvtAoiType` enum instance for known codepoints, or a raw
/// `int` for `Other(code)`. Code 3 is "Reserved" here vs. "Target" for
/// `RvtPoiType`.
fn convert_rvt_aoi_type(py: Python<'_>, v: RustRvtAoiType) -> PyResult<PyObject> {
    let code = match v {
        RustRvtAoiType::Friendly => 1u8,
        RustRvtAoiType::Hostile => 2,
        RustRvtAoiType::Reserved => 3,
        RustRvtAoiType::Unknown => 4,
        RustRvtAoiType::Other(b) => return Ok(b.into_py(py)),
        _ => {
            return Err(PyValueError::new_err(
                "unknown RvtAoiType variant crossing the binding",
            ));
        }
    };
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtAoiType"))?;
    Ok(cls.call1((code,))?.unbind())
}

/// Inverse of `convert_rvt_aoi_type`.
fn rvt_aoi_type_from_wire(b: u8) -> RustRvtAoiType {
    match b {
        1 => RustRvtAoiType::Friendly,
        2 => RustRvtAoiType::Hostile,
        3 => RustRvtAoiType::Reserved,
        4 => RustRvtAoiType::Unknown,
        other => RustRvtAoiType::Other(other),
    }
}

/// Translate a Rust `RvtPoi` to a Python `tstrans.klv.RvtPoi` dataclass.
fn convert_rvt_poi(py: Python<'_>, poi: &RustRvtPoi) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtPoi"))?;
    let kwargs = PyDict::new_bound(py);

    if let Some(v) = poi.number {
        kwargs.set_item("number", v)?;
    }
    if let Some(v) = poi.lat_deg {
        kwargs.set_item("lat_deg", v)?;
    }
    if let Some(v) = poi.lon_deg {
        kwargs.set_item("lon_deg", v)?;
    }
    if let Some(v) = poi.alt_m {
        kwargs.set_item("alt_m", v)?;
    }
    if let Some(v) = poi.poi_type {
        kwargs.set_item("poi_type", convert_rvt_poi_type(py, v)?)?;
    }
    if let Some(v) = &poi.text {
        kwargs.set_item("text", v.as_str())?;
    }
    if let Some(v) = &poi.source_icon {
        kwargs.set_item("source_icon", v.as_str())?;
    }
    if let Some(v) = &poi.source_id {
        kwargs.set_item("source_id", v.as_str())?;
    }
    if let Some(v) = &poi.label {
        kwargs.set_item("label", v.as_str())?;
    }
    if let Some(v) = &poi.operation_id {
        kwargs.set_item("operation_id", v.as_str())?;
    }
    kwargs.set_item(
        "sentinel_tags",
        pyo3::types::PyTuple::new_bound(py, poi.sentinel_tags.iter().map(|&t| t as u64)),
    )?;
    kwargs.set_item("unknown", convert_unknown(py, &poi.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &poi.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Inverse of `convert_rvt_poi`.
fn py_to_rvt_poi(p: &Bound<'_, PyAny>) -> PyResult<RustRvtPoi> {
    let mut r = RustRvtPoi::default();
    let py = p.py();

    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }

    op!(number, u16);
    op!(lat_deg, f64);
    op!(lon_deg, f64);
    op!(alt_m, f64);
    let poi_type_obj = p.getattr(intern!(py, "poi_type"))?;
    if !poi_type_obj.is_none() {
        r.poi_type = Some(rvt_poi_type_from_wire(enum_field_to_u8(&poi_type_obj)?));
    }
    os!(text);
    os!(source_icon);
    os!(source_id);
    os!(label);
    os!(operation_id);

    r.sentinel_tags = p
        .getattr(intern!(py, "sentinel_tags"))?
        .extract::<Vec<u32>>()?;
    r.unknown = py_to_unknown(p, is_st0806_poi_aoi_typed_tag)?;

    Ok(r)
}

/// Translate a Rust `RvtAoi` to a Python `tstrans.klv.RvtAoi` dataclass.
fn convert_rvt_aoi(py: Python<'_>, aoi: &RustRvtAoi) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtAoi"))?;
    let kwargs = PyDict::new_bound(py);

    if let Some(v) = aoi.number {
        kwargs.set_item("number", v)?;
    }
    if let Some(v) = aoi.corner_lat_p1_deg {
        kwargs.set_item("corner_lat_p1_deg", v)?;
    }
    if let Some(v) = aoi.corner_lon_p1_deg {
        kwargs.set_item("corner_lon_p1_deg", v)?;
    }
    if let Some(v) = aoi.corner_lat_p3_deg {
        kwargs.set_item("corner_lat_p3_deg", v)?;
    }
    if let Some(v) = aoi.corner_lon_p3_deg {
        kwargs.set_item("corner_lon_p3_deg", v)?;
    }
    if let Some(v) = aoi.aoi_type {
        kwargs.set_item("aoi_type", convert_rvt_aoi_type(py, v)?)?;
    }
    if let Some(v) = &aoi.text {
        kwargs.set_item("text", v.as_str())?;
    }
    if let Some(v) = &aoi.source_id {
        kwargs.set_item("source_id", v.as_str())?;
    }
    if let Some(v) = &aoi.label {
        kwargs.set_item("label", v.as_str())?;
    }
    if let Some(v) = &aoi.operation_id {
        kwargs.set_item("operation_id", v.as_str())?;
    }
    kwargs.set_item(
        "sentinel_tags",
        pyo3::types::PyTuple::new_bound(py, aoi.sentinel_tags.iter().map(|&t| t as u64)),
    )?;
    kwargs.set_item("unknown", convert_unknown(py, &aoi.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &aoi.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Inverse of `convert_rvt_aoi`.
fn py_to_rvt_aoi(p: &Bound<'_, PyAny>) -> PyResult<RustRvtAoi> {
    let mut r = RustRvtAoi::default();
    let py = p.py();

    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }

    op!(number, u16);
    op!(corner_lat_p1_deg, f64);
    op!(corner_lon_p1_deg, f64);
    op!(corner_lat_p3_deg, f64);
    op!(corner_lon_p3_deg, f64);
    let aoi_type_obj = p.getattr(intern!(py, "aoi_type"))?;
    if !aoi_type_obj.is_none() {
        r.aoi_type = Some(rvt_aoi_type_from_wire(enum_field_to_u8(&aoi_type_obj)?));
    }
    os!(text);
    os!(source_id);
    os!(label);
    os!(operation_id);

    r.sentinel_tags = p
        .getattr(intern!(py, "sentinel_tags"))?
        .extract::<Vec<u32>>()?;
    r.unknown = py_to_unknown(p, is_st0806_poi_aoi_typed_tag)?;

    Ok(r)
}

/// Translate a Rust `RvtUserData` to a Python `tstrans.klv.RvtUserData`
/// dataclass. `data_type`/`numeric_id` are `@property` accessors computed
/// on the Python side from `numeric_id_raw` (see `WeaponsStore`'s
/// bitfield accessors for the same pattern) — only the two wire fields
/// cross the binding.
fn convert_rvt_user_data(py: Python<'_>, ud: &RustRvtUserData) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtUserData"))?;
    Ok(cls
        .call1((
            ud.numeric_id_raw,
            pyo3::types::PyBytes::new_bound(py, &ud.data),
        ))?
        .unbind())
}

/// Inverse of `convert_rvt_user_data`.
fn py_to_rvt_user_data(p: &Bound<'_, PyAny>) -> PyResult<RustRvtUserData> {
    let py = p.py();
    Ok(RustRvtUserData {
        numeric_id_raw: p.getattr(intern!(py, "numeric_id_raw"))?.extract()?,
        data: p.getattr(intern!(py, "data"))?.extract()?,
    })
}

/// Translate a Rust `RvtLs` to a Python `tstrans.klv.RvtLs` dataclass.
fn convert_rvt_ls(py: Python<'_>, ls: &RustRvtLs) -> PyResult<PyObject> {
    let klv_mod = py.import_bound("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "RvtLs"))?;
    let kwargs = PyDict::new_bound(py);

    if let Some(v) = ls.crc32 {
        kwargs.set_item("crc32", v)?;
    }
    if let Some(v) = ls.timestamp_us {
        kwargs.set_item("timestamp_us", v)?;
    }
    if let Some(v) = ls.platform_true_airspeed {
        kwargs.set_item("platform_true_airspeed", v)?;
    }
    if let Some(v) = ls.platform_indicated_airspeed {
        kwargs.set_item("platform_indicated_airspeed", v)?;
    }
    if let Some(v) = ls.telemetry_accuracy_indicator {
        kwargs.set_item("telemetry_accuracy_indicator", v)?;
    }
    if let Some(v) = ls.frag_circle_radius_m {
        kwargs.set_item("frag_circle_radius_m", v)?;
    }
    if let Some(v) = ls.frame_code {
        kwargs.set_item("frame_code", v)?;
    }
    if let Some(v) = ls.rvt_ls_version {
        kwargs.set_item("rvt_ls_version", v)?;
    }
    if let Some(v) = ls.video_data_rate {
        kwargs.set_item("video_data_rate", v)?;
    }
    if let Some(v) = &ls.digital_video_file_format {
        kwargs.set_item("digital_video_file_format", v.as_str())?;
    }
    let user_defined: Vec<PyObject> = ls
        .user_defined
        .iter()
        .map(|u| convert_rvt_user_data(py, u))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item(
        "user_defined",
        pyo3::types::PyTuple::new_bound(py, user_defined),
    )?;
    let pois: Vec<PyObject> = ls
        .points_of_interest
        .iter()
        .map(|p| convert_rvt_poi(py, p))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item(
        "points_of_interest",
        pyo3::types::PyTuple::new_bound(py, pois),
    )?;
    let aois: Vec<PyObject> = ls
        .areas_of_interest
        .iter()
        .map(|a| convert_rvt_aoi(py, a))
        .collect::<PyResult<Vec<_>>>()?;
    kwargs.set_item(
        "areas_of_interest",
        pyo3::types::PyTuple::new_bound(py, aois),
    )?;
    if let Some(v) = ls.aircraft_mgrs_zone {
        kwargs.set_item("aircraft_mgrs_zone", v)?;
    }
    if let Some(v) = &ls.aircraft_mgrs_band_grid {
        kwargs.set_item("aircraft_mgrs_band_grid", v.as_str())?;
    }
    if let Some(v) = ls.aircraft_mgrs_easting_m {
        kwargs.set_item("aircraft_mgrs_easting_m", v)?;
    }
    if let Some(v) = ls.aircraft_mgrs_northing_m {
        kwargs.set_item("aircraft_mgrs_northing_m", v)?;
    }
    if let Some(v) = ls.frame_center_mgrs_zone {
        kwargs.set_item("frame_center_mgrs_zone", v)?;
    }
    if let Some(v) = &ls.frame_center_mgrs_band_grid {
        kwargs.set_item("frame_center_mgrs_band_grid", v.as_str())?;
    }
    if let Some(v) = ls.frame_center_mgrs_easting_m {
        kwargs.set_item("frame_center_mgrs_easting_m", v)?;
    }
    if let Some(v) = ls.frame_center_mgrs_northing_m {
        kwargs.set_item("frame_center_mgrs_northing_m", v)?;
    }
    kwargs.set_item("unknown", convert_unknown(py, &ls.unknown)?)?;
    kwargs.set_item("field_errors", convert_field_errors(py, &ls.field_errors)?)?;

    Ok(cls.call((), Some(&kwargs))?.unbind())
}

/// Inverse of `convert_rvt_ls`.
fn py_to_rvt_ls(p: &Bound<'_, PyAny>) -> PyResult<RustRvtLs> {
    let mut r = RustRvtLs::default();
    let py = p.py();

    macro_rules! op {
        ($field:ident, $ty:ty) => {
            if let Some(v) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<$ty>>()?
            {
                r.$field = Some(v);
            }
        };
    }
    macro_rules! os {
        ($field:ident) => {
            if let Some(s) = p
                .getattr(intern!(py, stringify!($field)))?
                .extract::<Option<String>>()?
            {
                r.$field = Some(s);
            }
        };
    }

    op!(crc32, u32);
    op!(timestamp_us, u64);
    op!(platform_true_airspeed, u16);
    op!(platform_indicated_airspeed, u16);
    op!(telemetry_accuracy_indicator, u8);
    op!(frag_circle_radius_m, u16);
    op!(frame_code, u32);
    op!(rvt_ls_version, u8);
    op!(video_data_rate, u32);
    os!(digital_video_file_format);

    for u in p.getattr(intern!(py, "user_defined"))?.iter()? {
        r.user_defined.push(py_to_rvt_user_data(&u?)?);
    }
    for poi in p.getattr(intern!(py, "points_of_interest"))?.iter()? {
        r.points_of_interest.push(py_to_rvt_poi(&poi?)?);
    }
    for aoi in p.getattr(intern!(py, "areas_of_interest"))?.iter()? {
        r.areas_of_interest.push(py_to_rvt_aoi(&aoi?)?);
    }

    op!(aircraft_mgrs_zone, u8);
    os!(aircraft_mgrs_band_grid);
    op!(aircraft_mgrs_easting_m, u32);
    op!(aircraft_mgrs_northing_m, u32);
    op!(frame_center_mgrs_zone, u8);
    os!(frame_center_mgrs_band_grid);
    op!(frame_center_mgrs_easting_m, u32);
    op!(frame_center_mgrs_northing_m, u32);

    r.unknown = py_to_unknown(p, is_st0806_rvt_typed_tag)?;

    Ok(r)
}

/// Decode an RVT Local Set body (ST 0806.4 Table 8-1) — the form carried
/// as the value of ST 0601 Tag 73. `buf` is **body-only** (no UL / outer
/// BER length wrapper).
#[pyfunction]
#[pyo3(name = "decode_rvt")]
fn decode_rvt_py(py: Python<'_>, buf: &[u8]) -> PyResult<PyObject> {
    match decode_st0806(buf) {
        Ok(ls) => convert_rvt_ls(py, &ls),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

/// Decode a standalone RVT Local Set: 16-byte UL + BER length + body,
/// verifying the CRC-32/MPEG-2 checksum (Tag 1) when present. Raises
/// `KlvError(CHECKSUM_MISMATCH)` on a bad CRC, `KlvError
/// (BAD_UNIVERSAL_LABEL)` on a mismatched UL.
#[pyfunction]
#[pyo3(name = "decode_rvt_standalone")]
fn decode_rvt_standalone_py(py: Python<'_>, buf: &[u8]) -> PyResult<PyObject> {
    match decode_st0806_standalone(buf) {
        Ok(ls) => convert_rvt_ls(py, &ls),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

/// Encode a Python `RvtLs` to wire bytes — RVT LS **body only** (no UL /
/// outer BER length wrapper / no Tag 1 CRC). Use for embedded RVT carried
/// in ST 0601 Tag 73. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_rvt")]
fn encode_rvt_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_rvt_ls(record)?;
    let bytes = encode_st0806(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

/// Encode a Python `RvtLs` as a standalone RVT record:
/// `[RVT_LS_UL:16][outer BER length][Tag 2 timestamp first][body][Tag 1
/// CRC-32/MPEG-2 last]` per ST 0806.4-02/-04. Raises `KlvEncodeError`
/// with `tag=2` if `timestamp_us` is unset. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_rvt_standalone")]
fn encode_rvt_standalone_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = py_to_rvt_ls(record)?;
    let bytes =
        encode_st0806_standalone(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new_bound(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0805.1 — KLV -> Cursor-on-Target (CoT) conversion
// ---------------------------------------------------------------------------

/// Map a Rust `CotError` to a Python `ValueError`. A missing input field is
/// an invalid-argument error on an already-decoded record, not a KLV
/// byte-decode failure, so this does NOT route through
/// `make_klv_error`/`KlvError` (contrast `st1204_error_to_pyerr` above,
/// which decodes wire bytes) — `ValueError` also keeps cross-binding
/// symmetry with the JVM `IllegalArgumentException` mapping.
fn cot_error_to_pyerr(e: CotError) -> PyErr {
    PyValueError::new_err(format!("{e}"))
}

/// Translate a Python `CotConfig` dataclass instance to a Rust `CotConfig`.
fn py_to_cot_config(cfg: &Bound<'_, PyAny>) -> PyResult<RustCotConfig> {
    let py = cfg.py();
    Ok(RustCotConfig {
        platform_type: cfg.getattr(intern!(py, "platform_type"))?.extract()?,
        update_interval_us: cfg.getattr(intern!(py, "update_interval_us"))?.extract()?,
        producer: cfg.getattr(intern!(py, "producer"))?.extract()?,
        geoid_undulation_m: cfg.getattr(intern!(py, "geoid_undulation_m"))?.extract()?,
        how: cfg.getattr(intern!(py, "how"))?.extract()?,
    })
}

/// `config = None` (the pyfunction default) means `CotConfig::default()`
/// (ST 0805.1's own defaults) — mirrors the Rust API's `&CotConfig`
/// requirement without forcing every Python call site to construct one.
fn cot_config_from_py(config: Option<&Bound<'_, PyAny>>) -> PyResult<RustCotConfig> {
    match config {
        Some(c) => py_to_cot_config(c),
        None => Ok(RustCotConfig::default()),
    }
}

/// Serialize a Platform Position CoT event (ST 0805.1 §5 Table 1) from a
/// decoded `UasDatalinkLs` record. `generated_us` (POSIX epoch
/// microseconds) is stamped into `detail/_flow-tags_`; it is a required
/// keyword argument rather than sampled internally, so conversion stays
/// deterministic (a replayed-file CoT run must be byte-identical to a live
/// one). `config` defaults to `CotConfig()` when omitted.
///
/// Raises `ValueError` naming the missing KLV tag when a mapping-required
/// field (uid components, timestamp, sensor position, altitude) is absent
/// from `record`.
#[pyfunction]
#[pyo3(
    name = "platform_position_xml",
    signature = (record, *, config = None, generated_us)
)]
fn platform_position_xml_py(
    record: &Bound<'_, PyAny>,
    config: Option<&Bound<'_, PyAny>>,
    generated_us: u64,
) -> PyResult<String> {
    let ls = py_to_uas_datalink_ls(record)?;
    let cfg = cot_config_from_py(config)?;
    st0805_platform_position_xml(&ls, &cfg, generated_us).map_err(cot_error_to_pyerr)
}

/// Serialize a Sensor Point of Interest CoT event (ST 0805.1 §5 Table 2)
/// from a decoded `UasDatalinkLs` record, linked back to the Platform
/// Position event via `detail/link`. See `platform_position_xml` for the
/// shared `config`/`generated_us` contract.
///
/// Raises `ValueError` naming the missing KLV tag when a mapping-required
/// field (uid components, timestamp, an aimpoint position pair, that
/// pair's elevation) is absent from `record`.
#[pyfunction]
#[pyo3(
    name = "sensor_point_of_interest_xml",
    signature = (record, *, config = None, generated_us)
)]
fn sensor_point_of_interest_xml_py(
    record: &Bound<'_, PyAny>,
    config: Option<&Bound<'_, PyAny>>,
    generated_us: u64,
) -> PyResult<String> {
    let ls = py_to_uas_datalink_ls(record)?;
    let cfg = cot_config_from_py(config)?;
    st0805_sensor_point_of_interest_xml(&ls, &cfg, generated_us).map_err(cot_error_to_pyerr)
}

/// Deterministic Platform Position `uid`: `"{tag10}_{tag3}"` (ST 0805.1 §5
/// Table 1). Raises `ValueError` naming the missing tag when Platform
/// Designation (Tag 10) or Mission ID (Tag 3) is absent from `record`.
#[pyfunction]
#[pyo3(name = "platform_uid")]
fn platform_uid_py(record: &Bound<'_, PyAny>) -> PyResult<String> {
    let ls = py_to_uas_datalink_ls(record)?;
    st0805_platform_uid(&ls).map_err(cot_error_to_pyerr)
}

/// Deterministic SPI `uid`: `"{tag10}_{tag3}_{tag11}"` (ST 0805.1 §5 Table
/// 2). Raises `ValueError` naming the missing tag when Platform
/// Designation (Tag 10), Mission ID (Tag 3), or Image Source Sensor (Tag
/// 11) is absent from `record`.
#[pyfunction]
#[pyo3(name = "spi_uid")]
fn spi_uid_py(record: &Bound<'_, PyAny>) -> PyResult<String> {
    let ls = py_to_uas_datalink_ls(record)?;
    st0805_spi_uid(&ls).map_err(cot_error_to_pyerr)
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyOutOfRangePolicy>()?;
    m.add_function(wrap_pyfunction!(decode_precision_timestamp_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_security_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_vmti_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_uas_datalink_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_uas_datalink_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        encode_uas_datalink_strict_compliance_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(patch_uas_datalink_py, m)?)?;
    m.add_function(wrap_pyfunction!(st0601_sentinel_meaning_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_security_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_security_strict_compliance_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_precision_timestamp_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_vmti_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_vmti_standalone_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_vmti_strict_compliance_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        encode_vmti_standalone_strict_compliance_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(decode_core_id_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_core_id_py, m)?)?;
    m.add_function(wrap_pyfunction!(core_id_text_py, m)?)?;
    m.add_function(wrap_pyfunction!(validate_mismms_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_sdcc_flp_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_sdcc_flp_mode2_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_rvt_py, m)?)?;
    m.add_function(wrap_pyfunction!(decode_rvt_standalone_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_rvt_py, m)?)?;
    m.add_function(wrap_pyfunction!(encode_rvt_standalone_py, m)?)?;
    m.add_function(wrap_pyfunction!(platform_position_xml_py, m)?)?;
    m.add_function(wrap_pyfunction!(sensor_point_of_interest_xml_py, m)?)?;
    m.add_function(wrap_pyfunction!(platform_uid_py, m)?)?;
    m.add_function(wrap_pyfunction!(spi_uid_py, m)?)?;
    Ok(())
}
