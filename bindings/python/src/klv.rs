//! PyO3 wrappers for `tst_core::klv::*` typed sets.
//!
//! The typed records (`UasDatalinkLs`, `VmtiLs`, `SecurityLs`, `RvtLs`, ...)
//! cross through the converters `print-records` writes into
//! `crate::generated::<family>`; the few fields those cannot express are
//! converted by hand in `crate::custom`. This module holds what is left:
//! the decode and encode entry points (`#[pyfunction]`s, which map
//! `KlvDecodeError` to `tstrans.exceptions.KlvError` via `make_kinded_error`),
//! the error translators, and the converters for the pass-through lists
//! (`convert_unknown`, `convert_field_errors`) that the custom converters
//! call.
//!
//! Covers ST 0601 / ST 0102 / ST 0605 / ST 0903 / ST 0806 / ST 1204 /
//! ST 1010 / ST 0805 decode and encode with field-error surfacing on the
//! decode path.
//!
//! `#![allow(...)]` mirrors the pattern in `errors.rs` and `mpegts.rs` —
//! PyO3's macro expansions under Rust 2024 trip these lints. Hand-
//! written code in this module has no unsafe blocks.
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::IntoPyObjectExt;
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
    decode as decode_st0102_lenient, decode_strict as decode_st0102_strict,
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
use tst_core::klv::st0605::{decode as decode_st0605, encode as encode_st0605};
use tst_core::klv::st0805::{
    CotConfig as RustCotConfig, platform_position_xml as st0805_platform_position_xml,
    platform_uid as st0805_platform_uid,
    sensor_point_of_interest_xml as st0805_sensor_point_of_interest_xml, spi_uid as st0805_spi_uid,
};
use tst_core::klv::st0806::{
    decode as decode_st0806, decode_standalone as decode_st0806_standalone,
    encode_to_vec as encode_st0806, encode_to_vec_standalone as encode_st0806_standalone,
};
use tst_core::klv::st0903::{
    decode as decode_st0903_lenient, decode_strict as decode_st0903_strict,
    encode_standalone_strict_compliance as encode_st0903_standalone_strict_compliance,
    encode_strict_compliance as encode_st0903_strict_compliance, encode_to_vec as encode_st0903,
    encode_to_vec_standalone as encode_st0903_standalone,
};
use tst_core::klv::st1010::{
    decode_sdcc_flp as decode_st1010_sdcc_flp,
    encode_sdcc_flp_mode2 as encode_st1010_sdcc_flp_mode2,
};
use tst_core::klv::st1204::{St1204Error, decode as decode_st1204, encode_to_vec as encode_st1204};

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

/// Decode an ST 0605 §7 Precision Time Stamp Pack. `buf` is the full
/// 26-byte wire-format pack (16-byte UL + 1-byte BER length + 1-byte
/// TimeStatus + 8-byte BE microsecond timestamp).
#[pyfunction]
#[pyo3(name = "decode_precision_timestamp")]
fn decode_precision_timestamp_py(py: Python<'_>, buf: &[u8]) -> PyResult<Py<PyAny>> {
    // GIL release: NOT wrapped in `py.detach`.
    // The Rust `decode_st0605` call is ~1us for the 26-byte spec pack;
    // GIL transition overhead exceeds the decode time for any realistic
    // payload size at this entry point (the measured breakeven is ~50us
    // per call; see `bindings/python/tests/test_gil_release.py`).
    match decode_st0605(buf) {
        Ok(pack) => crate::generated::st0605::precision_time_stamp_pack_to_py(py, &pack),
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
fn convert_field_error(py: Python<'_>, fe: &RustKlvFieldError) -> PyResult<Py<PyAny>> {
    let klv_mod = py.import("tstrans.klv")?;
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
    let kwargs = PyDict::new(py);
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
) -> PyResult<Py<PyAny>> {
    let items: Vec<Py<PyAny>> = errs
        .iter()
        .map(|e| convert_field_error(py, e))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyTuple::new(py, items)?.unbind().into())
}

/// Translate a list of `OwnedRawField` to a Python tuple of `(tag, bytes)`
/// 2-tuples. The shape matches `SecurityLs.unknown` / `UasDatalinkLs.unknown`
/// in the Python typed-set dataclasses.
pub(crate) fn convert_unknown(py: Python<'_>, unknown: &[OwnedRawField]) -> PyResult<Py<PyAny>> {
    let items: Vec<Py<PyAny>> = unknown
        .iter()
        .map(|f| {
            Ok(pyo3::types::PyTuple::new(
                py,
                &[
                    f.tag.into_py_any(py)?,
                    pyo3::types::PyBytes::new(py, &f.value).into_any().unbind(),
                ],
            )?
            .unbind()
            .into())
        })
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyTuple::new(py, items)?.unbind().into())
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
    for item in unknown_obj.try_iter()? {
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

// ---------------------------------------------------------------------------
// ST 0102 — Security Metadata LS
// ---------------------------------------------------------------------------

/// Decode an ST 0102 Security LS. `buf` is **body-only** (no UL / outer
/// BER length wrapper). With `strict=True`, rejects missing required
/// tags + unknown codepoints + non-canonical BER + malformed UTF-16.
#[pyfunction]
#[pyo3(name = "decode_security", signature = (buf, *, strict = false))]
fn decode_security_py(py: Python<'_>, buf: &[u8], strict: bool) -> PyResult<Py<PyAny>> {
    // GIL release: NOT wrapped — same rationale as
    // `decode_precision_timestamp_py`. ST 0102 records are typically
    // 20-200 bytes, well under the GIL-transition breakeven.
    let result = if strict {
        decode_st0102_strict(buf)
    } else {
        decode_st0102_lenient(buf)
    };
    match result {
        Ok(sec) => crate::generated::st0102::security_ls_to_py(py, &sec),
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

/// Encode a Python `SecurityLs` to wire bytes (lenient — emits only the
/// populated fields, no mandatory-tag enforcement). Returns `bytes`
/// containing the ST 0102 body (no outer UL / BER length wrapper).
#[pyfunction]
#[pyo3(name = "encode_security")]
fn encode_security_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0102::security_ls_from_py(record)?;
    let bytes = encode_st0102(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0605 — Python → Rust inverse translator + encode entry point
// ---------------------------------------------------------------------------

/// Encode a Python `PrecisionTimeStampPack` to the 26-byte wire form
/// (16-byte UL + 1-byte BER length + 1-byte TimeStatus + 8-byte BE
/// microsecond timestamp). Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_precision_timestamp")]
fn encode_precision_timestamp_py(py: Python<'_>, pack: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_pack = crate::generated::st0605::precision_time_stamp_pack_from_py(pack)?;
    let bytes = encode_st0605(&rust_pack);
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0903 — VTargetPack translator (entry points further down)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ST 0903 — VmtiLs entry point
// ---------------------------------------------------------------------------

/// Decode an ST 0903 VMTI LS. `buf` is **body-only** (no UL / outer
/// BER length wrapper). With `strict=True`, rejects missing required
/// tags per ST 0903.6 §6 Table 1, duplicate tags, malformed UTF-8.
#[pyfunction]
#[pyo3(name = "decode_vmti", signature = (buf, *, strict = false))]
fn decode_vmti_py(py: Python<'_>, buf: &[u8], strict: bool) -> PyResult<Py<PyAny>> {
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
        Ok(vmti) => crate::generated::st0903::vmti_ls_to_py(py, &vmti),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

// ---------------------------------------------------------------------------
// ST 0903 — Python → Rust inverse translators + encode entry points
// ---------------------------------------------------------------------------

/// Encode a Python `VmtiLs` to wire bytes — VMTI LS **body only** (no UL
/// / outer BER length wrapper / no Tag 1 checksum per ST 0903.6-120).
/// Use for embedded-VMTI carried inside MPEG-TS via tst-pipeline. Returns
/// `bytes`.
#[pyfunction]
#[pyo3(name = "encode_vmti")]
fn encode_vmti_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0903::vmti_ls_from_py(record)?;
    let bytes = encode_st0903(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

/// Encode a Python `VmtiLs` as a **standalone-VMTI** wire record:
/// `[VMTI_LS_UL:16][outer BER length][body][Tag 1 checkSum TLV]` per
/// ST 0903.4-17 / ST 0903.6-119. The Tag 1 checksum is computed from
/// the assembled framing; any value in `VmtiLs.checksum` is ignored.
/// Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_vmti_standalone")]
fn encode_vmti_standalone_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0903::vmti_ls_from_py(record)?;
    let bytes =
        encode_st0903_standalone(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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
) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0102::security_ls_from_py(record)?;
    let bytes =
        encode_st0102_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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
) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0903::vmti_ls_from_py(record)?;
    let bytes =
        encode_st0903_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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
) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0903::vmti_ls_from_py(record)?;
    let bytes = encode_st0903_standalone_strict_compliance(&rust_rec)
        .map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

// ---------------------------------------------------------------------------
// ST 1010.3 SDCC-FLP — general-purpose (not ST 0601-specific); entry
// points further down. `SdccFlp` has no `py_to_*` inverse: the only
// encoder, `encode_sdcc_flp_mode2`, takes plain std-dev/correlation
// lists rather than a full struct.
// ---------------------------------------------------------------------------

/// Decode a MISB ST 1010.3 SDCC-FLP pack (Mode 1 and Mode 2). `buf` is
/// the pack bytes starting at Element 1 (Matrix Size) — no outer TLV
/// framing and no leading Universal Label. General-purpose: not
/// ST 0601-specific (see `SdccFlp`'s docstring for the ST 0601 Tag 102
/// "Refined Source List" carriage, which is a separate concern captured
/// in `UasDatalinkLs.sdcc_flps` / `SdccFlpField`).
#[pyfunction]
#[pyo3(name = "decode_sdcc_flp")]
fn decode_sdcc_flp_py(py: Python<'_>, buf: &[u8]) -> PyResult<Py<PyAny>> {
    match decode_st1010_sdcc_flp(buf) {
        Ok(m) => crate::generated::st1010::sdcc_flp_to_py(py, &m),
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
) -> PyResult<Py<PyAny>> {
    let bytes = encode_st1010_sdcc_flp_mode2(&std_devs, &correlations, clen)
        .map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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
) -> PyResult<Py<PyAny>> {
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
#[pyclass(
    from_py_object,
    name = "OutOfRangePolicy",
    module = "tstrans.klv",
    eq,
    eq_int,
    frozen
)]
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
) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
    let mut opts = St0601EncodeConfig::default();
    if let Some(p) = out_of_range_policy {
        opts.out_of_range_policy = p.into();
    }
    let bytes =
        encode_st0601_with(&rust_rec, &opts).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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
) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
    let bytes =
        encode_st0601_strict_compliance(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

/// Byte-faithful tag-level patch of a raw ST 0601 local set: edited
/// tags re-encoded, every other TLV copied verbatim, checksum
/// recomputed. See `tstrans.klv.patch_uas_datalink` for the
/// dict-accepting user-facing wrapper.
///
/// No `detach` for the same reason as `decode_uas_datalink`:
/// the per-call Rust work is small; releasing the GIL produces
/// ping-pong under hot batch loops.
#[pyfunction]
#[pyo3(name = "patch_uas_datalink")]
fn patch_uas_datalink_py(
    py: Python<'_>,
    raw: &[u8],
    edits: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let rust_edits = crate::generated::st0601::uas_datalink_ls_from_py(edits)?;
    match patch_st0601(raw, &rust_edits) {
        Ok(bytes) => Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into()),
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

/// Decode a MISB ST 1204.3 MIIS Core Identifier from binary wire form.
/// `buf` must be exactly the bytes of one Core Identifier (no framing).
/// Raises `KlvError` on any decode failure.
#[pyfunction]
#[pyo3(name = "decode_core_id")]
fn decode_core_id_py(py: Python<'_>, buf: &[u8]) -> PyResult<Py<PyAny>> {
    match decode_st1204(buf) {
        Ok(id) => crate::generated::st1204::core_id_to_py(py, &id),
        Err(e) => Err(st1204_error_to_pyerr(py, e)),
    }
}

/// Encode a Python `CoreId` to its binary wire form. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_core_id")]
fn encode_core_id_py(py: Python<'_>, core_id: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_id = crate::generated::st1204::core_id_from_py(core_id)?;
    let bytes = encode_st1204(&rust_id);
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

/// Return the ST 1204.3 §7.4.2 textual representation of a `CoreId`.
#[pyfunction]
#[pyo3(name = "core_id_text")]
fn core_id_text_py(_py: Python<'_>, core_id: &Bound<'_, PyAny>) -> PyResult<String> {
    let rust_id = crate::generated::st1204::core_id_from_py(core_id)?;
    Ok(rust_id.to_string())
}

// ---------------------------------------------------------------------------
// ST 0902.8 MISMMS validator
// ---------------------------------------------------------------------------

/// Translate a Rust `MismmsViolation` to a Python `tstrans.klv.MismmsViolation`
/// dataclass instance.
fn convert_mismms_violation(py: Python<'_>, v: &RustMismmsViolation) -> PyResult<Py<PyAny>> {
    let klv_mod = py.import("tstrans.klv")?;
    let cls = klv_mod.getattr(intern!(py, "MismmsViolation"))?;
    let kwargs = PyDict::new(py);

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
fn validate_mismms_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0601::uas_datalink_ls_from_py(record)?;
    let violations = rust_validate_mismms(&rust_rec);
    let items: Vec<Py<PyAny>> = violations
        .iter()
        .map(|v| convert_mismms_violation(py, v))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(pyo3::types::PyList::new(py, items)?.unbind().into())
}

// ---------------------------------------------------------------------------
// ST 0806.4 — RVT (Remote Video Terminal) Local Set
// ---------------------------------------------------------------------------

/// Decode an RVT Local Set body (ST 0806.4 Table 8-1) — the form carried
/// as the value of ST 0601 Tag 73. `buf` is **body-only** (no UL / outer
/// BER length wrapper).
#[pyfunction]
#[pyo3(name = "decode_rvt")]
fn decode_rvt_py(py: Python<'_>, buf: &[u8]) -> PyResult<Py<PyAny>> {
    match decode_st0806(buf) {
        Ok(ls) => crate::generated::st0806::rvt_ls_to_py(py, &ls),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

/// Decode a standalone RVT Local Set: 16-byte UL + BER length + body,
/// verifying the CRC-32/MPEG-2 checksum (Tag 1) when present. Raises
/// `KlvError(CHECKSUM_MISMATCH)` on a bad CRC, `KlvError
/// (BAD_UNIVERSAL_LABEL)` on a mismatched UL.
#[pyfunction]
#[pyo3(name = "decode_rvt_standalone")]
fn decode_rvt_standalone_py(py: Python<'_>, buf: &[u8]) -> PyResult<Py<PyAny>> {
    match decode_st0806_standalone(buf) {
        Ok(ls) => crate::generated::st0806::rvt_ls_to_py(py, &ls),
        Err(e) => Err(klv_decode_error_to_pyerr(py, e)),
    }
}

/// Encode a Python `RvtLs` to wire bytes — RVT LS **body only** (no UL /
/// outer BER length wrapper / no Tag 1 CRC). Use for embedded RVT carried
/// in ST 0601 Tag 73. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_rvt")]
fn encode_rvt_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0806::rvt_ls_from_py(record)?;
    let bytes = encode_st0806(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
}

/// Encode a Python `RvtLs` as a standalone RVT record:
/// `[RVT_LS_UL:16][outer BER length][Tag 2 timestamp first][body][Tag 1
/// CRC-32/MPEG-2 last]` per ST 0806.4-02/-04. Raises `KlvEncodeError`
/// with `tag=2` if `timestamp_us` is unset. Returns `bytes`.
#[pyfunction]
#[pyo3(name = "encode_rvt_standalone")]
fn encode_rvt_standalone_py(py: Python<'_>, record: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let rust_rec = crate::generated::st0806::rvt_ls_from_py(record)?;
    let bytes =
        encode_st0806_standalone(&rust_rec).map_err(|e| klv_encode_error_to_pyerr(py, e))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes).unbind().into())
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

/// `config = None` (the pyfunction default) means `CotConfig::default()`
/// (ST 0805.1's own defaults) — mirrors the Rust API's `&CotConfig`
/// requirement without forcing every Python call site to construct one.
fn cot_config_from_py(config: Option<&Bound<'_, PyAny>>) -> PyResult<RustCotConfig> {
    match config {
        Some(c) => crate::generated::st0805::cot_config_from_py(c),
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
