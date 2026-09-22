//! Rust-side helpers that construct the Python exception classes
//! defined in `tstrans.exceptions`, for the OFFLINE domains only — mux,
//! demux, KLV and the three `srt.Socket` lowlevel sites. Every transport
//! and shell error goes through `crate::raise` instead (Arc 2 WP-B2),
//! which resolves `BindingErrorKind::name()` on the domain's kind enum.
//!
//! The attribute-carrying mappers below (`mux_error_to_pyerr`,
//! `demux_error_to_pyerr`, `klv_decode_error_to_pyerr`,
//! `klv_encode_error_to_pyerr`, `codec_parse_error_to_pyerr`) take their
//! member from the A2 classifiers (`kind_of_mux`, `kind_of_demux`, …) and
//! add the per-variant attributes the Python classes expose.
//!
//! Implementation note: we deliberately do NOT use PyO3's
//! `create_exception!` (which would mint *new* exception classes on
//! the Rust side, distinct from the Python-defined `class MuxError`).
//! Users need `isinstance(err, tstrans.exceptions.MuxError)` to work
//! whether the error comes from Python or Rust — so the Rust side
//! must *call into* the Python-defined classes, which is what
//! `py.import_bound("tstrans.exceptions").getattr("MuxError")?` does.
//! This is slower than `create_exception!` (per-raise dict lookup +
//! Python call) but the tradeoff is required for the contract.

// PyO3's `#[pyfunction]` macro (Rust 2024 edition) generates extractor
// code that calls `pyo3::impl_::extract_argument::unwrap_required_argument`
// — an unsafe fn — without an explicit `unsafe {}` block in the expansion.
// The `useless_conversion` allow covers a `PyErr -> PyErr` `.into()` emitted
// by the same macro. Both suppressions are scoped to macro-generated code
// only; hand-written code in this file contains no unsafe blocks.
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;

// ---------------------------------------------------------------------------
// Shared constructor + macro-generated thin wrappers
// ---------------------------------------------------------------------------

/// Construct any `FooError` Python exception from `tstrans.exceptions`.
///
/// Looks up `<kind_enum_class>.<kind_variant>` and calls
/// `<error_class>(kind=<variant>, message=<message>)` with kwargs.
/// Any attribute-lookup failure (e.g. an unknown `kind_variant`) is itself
/// returned as a `PyErr`. Members reached this way come from
/// `BindingErrorKind::name()` (via the `kind_of_*` classifiers), which
/// `crate::raise::check_error_kinds` proves resolvable at `import tstrans`.
///
/// We deliberately do NOT use PyO3's `create_exception!`: that would mint
/// NEW exception classes on the Rust side, distinct from the Python-defined
/// `class MuxError` etc. Users need `isinstance(err, MuxError)` to work
/// whether the exception comes from Python or Rust, so this side must CALL
/// INTO the Python-defined classes rather than defining its own.
pub(crate) fn make_kinded_error(
    py: Python<'_>,
    error_class: &str,
    kind_enum_class: &str,
    kind_variant: &str,
    message: &str,
) -> PyErr {
    let exceptions = match py.import_bound("tstrans.exceptions") {
        Ok(m) => m,
        Err(e) => return e,
    };
    let kind_enum = match exceptions.getattr(kind_enum_class) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let kind_value = match kind_enum.getattr(kind_variant) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let cls = match exceptions.getattr(error_class) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let kwargs = PyDict::new_bound(py);
    if let Err(e) = kwargs.set_item("kind", kind_value) {
        return e;
    }
    if let Err(e) = kwargs.set_item("message", message) {
        return e;
    }
    match cls.call((), Some(&kwargs)) {
        Ok(instance) => PyErr::from_value_bound(instance),
        Err(e) => e,
    }
}

// The per-prefix `make_<name>_error` wrappers were deleted in 0.7.0 with
// the Python error-mapping ratchet that counted their literal call sites
// (Arc 2 WP-B2): the kind vocabulary is now proven Rust-side by
// scripts/check/rust/kind-table-coverage.sh and binding-side by
// `crate::raise::check_error_kinds` at `import tstrans`, so there is
// nothing left for a per-kind call-site census to check. The handful of
// remaining callers name the class and the enum at the call site.

/// Test-only: raise `member` (a `BindingErrorKind::name()` string) through
/// the real raise path, so the pytest kind-wiring suites exercise `raise.rs`
/// instead of a parallel literal-string mapper.
#[cfg(any(
    feature = "srt",
    feature = "udp",
    feature = "tcp",
    feature = "hls",
    feature = "rist"
))]
fn raise_for_test(py: Python<'_>, d: &crate::raise::Domain, member: &str, message: &str) -> PyErr {
    match crate::raise::kind_by_name(d, member) {
        Some(k) => crate::raise::raise(py, d, tst_pipeline::binding::BindingError::new(k, message)),
        None => pyo3::exceptions::PyValueError::new_err(format!(
            "{} has no kind with member {member}",
            d.kind_enum
        )),
    }
}

/// Test helper: forces a `MuxError` raise from Rust, used by
/// `test_error_wiring.py` to confirm end-to-end wiring. Exposed only
/// under the `_native._raise_mux_error_for_test` name.
#[pyfunction]
#[pyo3(name = "_raise_mux_error_for_test")]
pub fn raise_mux_error_for_test(py: Python<'_>, message: &str) -> PyResult<()> {
    Err(make_kinded_error(
        py,
        "MuxError",
        "MuxErrorKind",
        "INTERNAL",
        message,
    ))
}

/// Test helper: forces an `SrtError` raise from Rust, exposed as
/// `_native._raise_srt_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. The kind is a
/// `BindingErrorKind::name()` member string resolved against the domain's
/// `KINDS` table, so an unknown member is a `ValueError`, not a silent pass.
#[cfg(feature = "srt")]
#[pyfunction]
#[pyo3(name = "_raise_srt_error_for_test")]
pub fn raise_srt_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(raise_for_test(py, &crate::raise::SRT, kind, message))
}

/// Test helper: forces a `UdpError` raise from Rust, exposed as
/// `_native._raise_udp_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind (a
/// `BindingErrorKind::name()` member string; see the `srt` helper above).
#[cfg(feature = "udp")]
#[pyfunction]
#[pyo3(name = "_raise_udp_error_for_test")]
pub fn raise_udp_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(raise_for_test(py, &crate::raise::UDP, kind, message))
}

/// Test helper: forces a `TcpError` raise from Rust, exposed as
/// `_native._raise_tcp_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind (a
/// `BindingErrorKind::name()` member string; see the `srt` helper above).
#[cfg(feature = "tcp")]
#[pyfunction]
#[pyo3(name = "_raise_tcp_error_for_test")]
pub fn raise_tcp_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(raise_for_test(py, &crate::raise::TCP, kind, message))
}

/// Test helper: forces an `HlsError` raise from Rust, exposed as
/// `_native._raise_hls_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind (a
/// `BindingErrorKind::name()` member string; see the `srt` helper above).
#[cfg(feature = "hls")]
#[pyfunction]
#[pyo3(name = "_raise_hls_error_for_test")]
pub fn raise_hls_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(raise_for_test(py, &crate::raise::HLS, kind, message))
}

/// Test helper: forces a `RistError` raise from Rust, exposed as
/// `_native._raise_rist_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind (a
/// `BindingErrorKind::name()` member string; see the `srt` helper above).
#[cfg(feature = "rist")]
#[pyfunction]
#[pyo3(name = "_raise_rist_error_for_test")]
pub fn raise_rist_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(raise_for_test(py, &crate::raise::RIST, kind, message))
}

// ---------------------------------------------------------------------------
// Rust-typed → PyErr mappers
// ---------------------------------------------------------------------------

/// Map a Rust `MuxError` to a Python `MuxError` instance. Routes
/// via the 5-variant `MuxErrorKind` coarse classification —
/// the muxer's `kind()` accessor (plan #91) is the source of truth
/// for which Python `MuxErrorKind` variant to use.
///
/// The `MuxErrorKind` enum is `#[non_exhaustive]`; the wildcard
/// arm routes unknown future variants to `INTERNAL` so this fn never
/// panics on a Rust-side enum addition (the test suite will surface
/// the omission when the new variant gets a tagged-test fixture).
///
/// Called from Muxer wrappers.
#[allow(dead_code)]
pub(crate) fn mux_error_to_pyerr(py: Python<'_>, e: tst_core::MuxError) -> PyErr {
    // A2's K4 ruling: `INVALID_NAL` / `KLV_TOO_LARGE` / `INVALID_AV1_OBU` /
    // `MISP_TIME` are precise kinds since 0.7.0 — the five coarse buckets of
    // `MuxErrorKind` no longer flatten them.
    let kind_str = tst_pipeline::binding::kind::kind_of_mux(&e).name();
    // BufferFull gets a Python-only breadcrumb: the most common way to
    // hit it is pushing on the original Muxer inside an active
    // `Muxer.write_file(...)` block — those pushes bypass the drain
    // proxy the `with` statement yields, so nothing ever drains. The
    // hint lives here (not in tst-core's Display) because `write_file`
    // exists only in the Python binding.
    let msg = match &e {
        tst_core::MuxError::BufferFull { .. } => format!(
            "{e}; if pushing inside `Muxer.write_file(...)`, push on the \
             proxy object the `with` statement yields — pushes on the \
             original Muxer bypass the per-push drain"
        ),
        _ => e.to_string(),
    };
    make_kinded_error(py, "MuxError", "MuxErrorKind", kind_str, &msg)
}

/// Map a Rust `CodecParseError` to a Python `CodecError` instance.
///
/// `codec` is a short lowercase string naming the codec that failed
/// (e.g. `"h264"`, `"h265"`, `"aac"`). Forwards all variant-specific
/// fields as keyword arguments to `CodecError.__init__` so the Python
/// side can read `.offset_bits`, `.field`, `.expected`, etc.
///
/// The member comes from `tst_pipeline::binding::kind_of_codec`, so a
/// Rust-side enum addition cannot silently fall through here; only the
/// per-variant ATTRIBUTE forwarding below is hand-written, and its
/// wildcard arm simply forwards no extras.
///
/// Called from codec-parser wrappers.
#[allow(dead_code)]
pub(crate) fn codec_parse_error_to_pyerr(
    py: Python<'_>,
    err: &tst_core::codec::CodecParseError,
    codec: &str,
) -> PyErr {
    use tst_core::codec::CodecParseError;
    let exceptions = match py.import_bound("tstrans.exceptions") {
        Ok(m) => m,
        Err(e) => return e,
    };
    let kind_class = match exceptions.getattr(intern!(py, "CodecErrorKind")) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let codec_error_class = match exceptions.getattr(intern!(py, "CodecError")) {
        Ok(c) => c,
        Err(e) => return e,
    };
    // The KIND comes from A2's one table (`kind_of_codec`); this match only
    // harvests the per-variant ATTRIBUTES the `CodecError` class carries.
    // `BufferTooSmall` gained its own `BUFFER_TOO_SMALL` member in 0.7.0
    // (it folded into `ENGINE_ERROR` before) and now carries `needed` /
    // `have` — still unreachable from Python, since the write-into-a-caller
    // -buffer entry points have no binding.
    let kind_name = tst_pipeline::binding::kind::kind_of_codec(err).name();
    let extra_attrs: Vec<(&str, PyObject)> = match err {
        CodecParseError::TruncatedRbsp {
            offset_bits,
            needed_bits,
        } => vec![
            ("offset_bits", offset_bits.into_py(py)),
            ("needed_bits", needed_bits.into_py(py)),
        ],
        CodecParseError::InvalidGolomb { offset_bits } => {
            vec![("offset_bits", offset_bits.into_py(py))]
        }
        CodecParseError::ReservedValue { field, value } => vec![
            ("field", (*field).into_py(py)),
            ("value", value.into_py(py)),
        ],
        CodecParseError::UnsupportedProfile { profile_idc } => {
            vec![("profile_idc", profile_idc.into_py(py))]
        }
        CodecParseError::DanglingSpsReference { sps_id } => {
            vec![("sps_id", sps_id.into_py(py))]
        }
        CodecParseError::DanglingVpsReference { vps_id } => {
            vec![("vps_id", vps_id.into_py(py))]
        }
        CodecParseError::EngineError(_) => vec![],
        CodecParseError::InvalidLeb128 { offset_bytes } => {
            vec![("offset_bytes", offset_bytes.into_py(py))]
        }
        CodecParseError::BadSyncWord { expected, found } => vec![
            ("expected", expected.into_py(py)),
            ("found", found.into_py(py)),
        ],
        CodecParseError::Truncated { needed, had } => {
            vec![("needed", needed.into_py(py)), ("had", had.into_py(py))]
        }
        CodecParseError::Forbidden { field } => vec![("field", (*field).into_py(py))],
        CodecParseError::UnsupportedFreeFormat { layer } => {
            vec![("layer", layer.into_py(py))]
        }
        CodecParseError::InvalidLengthSize { got } => vec![("got", got.into_py(py))],
        CodecParseError::NalLengthOverflow {
            nal_len,
            length_size,
        } => vec![
            ("nal_len", nal_len.into_py(py)),
            ("length_size", length_size.into_py(py)),
        ],
        CodecParseError::BufferTooSmall { needed, have } => {
            vec![("needed", needed.into_py(py)), ("have", have.into_py(py))]
        }
        // Catch-all for #[non_exhaustive] additions not yet mapped.
        _ => vec![],
    };
    let kind = match kind_class.getattr(kind_name) {
        Ok(k) => k,
        Err(e) => return e,
    };
    let message = format!("{err}");
    let kwargs = PyDict::new_bound(py);
    if let Err(e) = kwargs.set_item("kind", kind) {
        return e;
    }
    if let Err(e) = kwargs.set_item("codec", codec) {
        return e;
    }
    if let Err(e) = kwargs.set_item("message", &message) {
        return e;
    }
    for (k, v) in extra_attrs {
        if let Err(e) = kwargs.set_item(k, v) {
            return e;
        }
    }
    let positional_args = pyo3::types::PyTuple::empty_bound(py);
    match codec_error_class.call(positional_args, Some(&kwargs)) {
        Ok(instance) => PyErr::from_value_bound(instance),
        Err(e) => e,
    }
}

/// Map a Rust `KlvEncodeError` to a Python `KlvEncodeError` instance.
/// Covers all 8 variants; the wildcard arm routes to `BUFFER_TOO_SMALL`
/// (a benign "encode failed; widen output buffer" fallback) for any
/// future Rust variants introduced through the `#[non_exhaustive]`
/// hatch — explicit arms get added as new variants surface.
///
/// Where the Rust variant carries a numeric identifier it is forwarded to
/// the Python `KlvEncodeError.tag` attribute: a KLV tag for `OutOfRange`,
/// `StringTooLong`, `MissingMandatoryItem`, `ReservedTagInUnknown`, and
/// `ForbiddenStandaloneOffset`; the VTarget Pack `target_id` for
/// `VTargetPackEmpty` and `DuplicateTargetId`. Variants without one
/// (`BufferTooSmall`, `RecordTooLarge`, `UnsupportedImapbLength`,
/// `InvalidImapbParams`) leave `.tag = None`.
///
/// Called from KLV `encode_*` wrappers.
#[allow(dead_code)]
pub(crate) fn klv_encode_error_to_pyerr(py: Python<'_>, e: tst_core::KlvEncodeError) -> PyErr {
    use tst_core::error::KlvEncodeError as RustE;
    // `tag` is `Option<u64>` so the VTarget Pack `target_id` (a u64 since
    // REF-KLV-04) reaches `.tag` losslessly; the KLV-tag-number variants
    // widen their u16/u32 tag values to u64 (lossless). PyO3 maps `u64` →
    // Python `int` (unbounded), matching the `.tag: Optional[int]` stub.
    // The KIND comes from A2's one table; this match only harvests `.tag`
    // (a KLV tag for most variants, a VTarget Pack `target_id` for two).
    let kind_str = tst_pipeline::binding::kind::kind_of_klv_encode(&e).name();
    let tag: Option<u64> = match &e {
        RustE::BufferTooSmall { .. } => None,
        RustE::RecordTooLarge => None,
        RustE::OutOfRange { tag, .. } => Some(u64::from(*tag)),
        RustE::StringTooLong { tag, .. } => Some(u64::from(*tag)),
        RustE::UnsupportedImapbLength { .. } => None,
        RustE::InvalidImapbParams { .. } => None,
        RustE::MissingMandatoryItem { tag, .. } => Some(u64::from(*tag)),
        RustE::ReservedTagInUnknown { tag } => Some(u64::from(*tag)),
        RustE::VTargetPackEmpty { target_id } => Some(*target_id),
        RustE::DuplicateTargetId { target_id } => Some(*target_id),
        RustE::ForbiddenStandaloneOffset { tag } => Some(u64::from(*tag)),
        _ => None,
    };
    let msg = e.to_string();
    let exceptions = match py.import_bound("tstrans.exceptions") {
        Ok(m) => m,
        Err(err) => return err,
    };
    let kind_enum = match exceptions.getattr(intern!(py, "KlvEncodeErrorKind")) {
        Ok(en) => en,
        Err(err) => return err,
    };
    let kind_value = match kind_enum.getattr(kind_str) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let cls = match exceptions.getattr(intern!(py, "KlvEncodeError")) {
        Ok(c) => c,
        Err(err) => return err,
    };
    let kwargs = PyDict::new_bound(py);
    if let Err(err) = kwargs.set_item("kind", kind_value) {
        return err;
    }
    if let Some(t) = tag {
        if let Err(err) = kwargs.set_item("tag", t) {
            return err;
        }
    }
    match cls.call((msg,), Some(&kwargs)) {
        Ok(instance) => PyErr::from_value_bound(instance),
        Err(err) => err,
    }
}
