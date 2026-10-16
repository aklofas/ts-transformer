//! PyO3 wrappers for `tst_core::klv::*` typed sets.
//!
//! Translation strategy: each Rust `*Ls` / `*Pack` struct is converted
//! to an instance of a Python-side dataclass under `tstrans.klv.*` via
//! per-set translator functions (`convert_uas_datalink`, etc.). Decode
//! entry points are `#[pyfunction]`s that map `KlvDecodeError` to
//! `tstrans.exceptions.KlvError` via `make_kinded_error`.
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
use tst_core::klv::pack::OwnedRawField;
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod as RustClsCountry, ObjectCountryCodingMethod as RustObjCountry,
    SecurityClassification as RustSecCls, SecurityLs, decode as decode_st0102_lenient,
    decode_strict as decode_st0102_strict,
    encode_strict_compliance as encode_st0102_strict_compliance, encode_to_vec as encode_st0102,
};
use tst_core::klv::st0601::{
    EncodeConfig as St0601EncodeConfig, MismmsViolation as RustMismmsViolation,
    OutOfRangePolicy as RustOutOfRangePolicy, St0601SentinelMeaning,
    decode as decode_st0601_lenient, decode_strict as decode_st0601_strict,
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

use crate::errors::{klv_encode_error_to_pyerr, make_kinded_error};

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
    make_kinded_error(py, "KlvError", "KlvErrorKind", kind, &msg)
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
    make_kinded_error(py, "KlvError", "KlvErrorKind", kind, &msg)
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
    make_kinded_error(py, "KlvError", "KlvErrorKind", kind, &msg)
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
    // GIL release: NOT wrapped in `py.allow_threads`.
    // The Rust `decode_st0605` call is ~1us for the 26-byte spec pack;
    // GIL transition overhead exceeds the decode time for any realistic
    // payload size at this entry point (the measured breakeven is ~50us
    // per call; see `bindings/python/tests/test_gil_release.py`).
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
pub(crate) fn convert_field_errors(
    py: Python<'_>,
    errs: &[RustKlvFieldError],
) -> PyResult<PyObject> {
    let items: Vec<PyObject> = errs
        .iter()
        .map(|e| convert_field_error(py, e))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyTuple::new_bound(py, items).unbind().into())
}

/// Translate a list of `OwnedRawField` to a Python tuple of `(tag, bytes)`
/// 2-tuples. The shape matches `SecurityLs.unknown` / `UasDatalinkLs.unknown`
/// in the Python typed-set dataclasses.
pub(crate) fn convert_unknown(py: Python<'_>, unknown: &[OwnedRawField]) -> PyResult<PyObject> {
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
/// corrupting the Rust side (validate, don't drop).
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
///    rather than failing the round-trip (deterministic precedence).
pub(crate) fn py_to_unknown(
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
/// `crates/tst-core/src/klv/st0601/tags.rs` (142 entries:
/// 1-65, 67-143). Tag 66 is the deprecated placeholder (permanently
/// untyped by design — ST 0601.19 §8.66: "This item has been
/// Deprecated") and 144..=255 are forward-compat; both may legitimately
/// appear in `unknown` (66 and 200 are the durable unknown-tag test
/// stand-ins used across this suite — never add them here). The typed
/// set includes Tag 102 (MULTI-INSTANCE SDCC-FLP, typed via
/// `sdcc_flps`) and Tag 115 (MULTI-INSTANCE Control Command, typed
/// via `control_commands`) — keep this in sync with `tags::TAGS` when
/// new tags are typed, or a caller-supplied `unknown` entry for a
/// newly-typed tag will slip past this filter and get rejected
/// downstream by the real Rust encoder's own (stricter, canonical)
/// check instead of being silently dropped here per the documented
/// "typed wins" collision policy.
pub(crate) fn is_st0601_typed_tag(tag: u32) -> bool {
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
            Ok(cls.call1((known.to_wire(),))?.unbind())
        }
    }
}

fn convert_classifying_country(py: Python<'_>, v: RustClsCountry) -> PyResult<PyObject> {
    match v {
        RustClsCountry::Unknown(b) => Ok(b.into_py(py)),
        known => {
            let klv_mod = py.import_bound("tstrans.klv")?;
            let cls = klv_mod.getattr(intern!(py, "ClassifyingCountryCodingMethod"))?;
            Ok(cls.call1((known.to_wire(),))?.unbind())
        }
    }
}

fn convert_object_country(py: Python<'_>, v: RustObjCountry) -> PyResult<PyObject> {
    match v {
        RustObjCountry::Unknown(b) => Ok(b.into_py(py)),
        known => {
            let klv_mod = py.import_bound("tstrans.klv")?;
            let cls = klv_mod.getattr(intern!(py, "ObjectCountryCodingMethod"))?;
            Ok(cls.call1((known.to_wire(),))?.unbind())
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
    // GIL release: NOT wrapped — same rationale as
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
pub(crate) fn enum_field_to_u8(p: &Bound<'_, PyAny>) -> PyResult<u8> {
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
pub(crate) fn enum_field_to_u64(p: &Bound<'_, PyAny>) -> PyResult<u64> {
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
/// lossless. Entries whose tag collides with a typed field
/// (see `is_st0102_typed_tag`) are silently dropped — typed wins.
fn py_to_security_ls(p: &Bound<'_, PyAny>) -> PyResult<SecurityLs> {
    let mut r = SecurityLs::default();
    let py = p.py();

    // 3 typed-enum fields: Python sends either an enum instance OR a raw int
    // (Unknown codepoint). Use enum_field_to_u8 to coerce both shapes.
    let sc_obj = p.getattr(intern!(py, "security_classification"))?;
    if !sc_obj.is_none() {
        r.security_classification = Some(RustSecCls::from_wire(enum_field_to_u8(&sc_obj)?));
    }
    let cc_obj = p.getattr(intern!(py, "classifying_country_coding_method"))?;
    if !cc_obj.is_none() {
        r.classifying_country_coding_method =
            Some(RustClsCountry::from_wire(enum_field_to_u8(&cc_obj)?));
    }
    let oc_obj = p.getattr(intern!(py, "object_country_coding_method"))?;
    if !oc_obj.is_none() {
        r.object_country_coding_method =
            Some(RustObjCountry::from_wire(enum_field_to_u8(&oc_obj)?));
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
    // `ob!`-style bytes fields of the generated `UasDatalinkLs` converter.
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
    // GIL release: NOT wrapped — same rationale as
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
/// `unknown` IS round-tripped: forward-compat TLVs preserved
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
    // instead of silently dropping.
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
/// `unknown` IS round-tripped: forward-compat TLVs preserved
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
// ST 1010.3 SDCC-FLP — general-purpose (not ST 0601-specific); entry
// points further down. `SdccFlp` has no `py_to_*` inverse: the only
// encoder, `encode_sdcc_flp_mode2`, takes plain std-dev/correlation
// lists rather than a full struct.
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
    // GIL release: NOT wrapped — same rationale as
    // `decode_precision_timestamp_py`. Even at the upper end (~10 KB
    // record with 100 unknown TLVs), Rust decode is ~10us per call,
    // below the GIL transition breakeven. Worse, the 80-field
    // generated `UasDatalinkLs` projection dominates per-call CPU
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
        Ok(rec) => crate::generated::st0601::uas_datalink_ls_to_py(py, &rec),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// ST 0601 — Python → Rust inverse translator + encode entry points
// ---------------------------------------------------------------------------

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
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
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
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
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
    let rust_edits = crate::generated::st0601::uas_datalink_ls_from_py(edits)?;
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
/// Reuses the generated `uas_datalink_ls_from_py` — the same converter used by
/// `encode_uas_datalink` — so the Rust-side check sees an identical record.
#[pyfunction]
#[pyo3(name = "validate_mismms")]
fn validate_mismms_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
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
/// `make_kinded_error`/`KlvError` (contrast `st1204_error_to_pyerr` above,
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
    let ls = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
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
    let ls = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
    let cfg = cot_config_from_py(config)?;
    st0805_sensor_point_of_interest_xml(&ls, &cfg, generated_us).map_err(cot_error_to_pyerr)
}

/// Deterministic Platform Position `uid`: `"{tag10}_{tag3}"` (ST 0805.1 §5
/// Table 1). Raises `ValueError` naming the missing tag when Platform
/// Designation (Tag 10) or Mission ID (Tag 3) is absent from `record`.
#[pyfunction]
#[pyo3(name = "platform_uid")]
fn platform_uid_py(record: &Bound<'_, PyAny>) -> PyResult<String> {
    let ls = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
    st0805_platform_uid(&ls).map_err(cot_error_to_pyerr)
}

/// Deterministic SPI `uid`: `"{tag10}_{tag3}_{tag11}"` (ST 0805.1 §5 Table
/// 2). Raises `ValueError` naming the missing tag when Platform
/// Designation (Tag 10), Mission ID (Tag 3), or Image Source Sensor (Tag
/// 11) is absent from `record`.
#[pyfunction]
#[pyo3(name = "spi_uid")]
fn spi_uid_py(record: &Bound<'_, PyAny>) -> PyResult<String> {
    let ls = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
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
