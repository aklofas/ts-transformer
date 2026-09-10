//! Rust-side helpers that construct the Python exception classes
//! defined in `tstrans.exceptions`. Type wrappers use these to raise
//! Python-side exceptions — e.g. `Demuxer.feed_bytes` calls
//! `make_demux_error(py, "BAD_PMT", "...")` when the underlying
//! `tst_core::mpegts::Demuxer` returns an error.
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
/// returned as a `PyErr`. The bash ratchets in `scripts/check/python/`
/// enforce that every variant name used in this crate is a valid member of
/// the corresponding `FooErrorKind` enum.
///
/// We deliberately do NOT use PyO3's `create_exception!`: that would mint
/// NEW exception classes on the Rust side, distinct from the Python-defined
/// `class MuxError` etc. Users need `isinstance(err, MuxError)` to work
/// whether the exception comes from Python or Rust, so this side must CALL
/// INTO the Python-defined classes rather than defining its own.
fn make_kinded_error(
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

/// Generate a `pub fn make_<Name>_error(py, kind_variant, message) -> PyErr`
/// thin wrapper around [`make_kinded_error`]. The error class is
/// `<Prefix>Error` and the kind enum is `<Prefix>ErrorKind`. An optional
/// `cfg(...)` arm gates the wrapper behind a cargo feature.
macro_rules! make_error_fn {
    ($fn_name:ident, $prefix:literal $(, cfg($($cfg:tt)*))?) => {
        $(#[cfg($($cfg)*)])?
        pub fn $fn_name(py: Python<'_>, kind_variant: &str, message: &str) -> PyErr {
            make_kinded_error(
                py,
                concat!($prefix, "Error"),
                concat!($prefix, "ErrorKind"),
                kind_variant,
                message,
            )
        }
    };
}

// One thin wrapper per tstrans.exceptions error class.
// The bash ratchets in scripts/check/python/ enforce that every *ErrorKind
// variant has at least one literal make_*_error(py, "VARIANT", ...) call
// site in the crate. Callers pass string literals for kind_variant; an
// unknown name surfaces as AttributeError from the Python side.
make_error_fn!(make_mux_error, "Mux");
make_error_fn!(make_demux_error, "Demux");
make_error_fn!(make_klv_error, "Klv");
make_error_fn!(make_rtsp_error, "Rtsp", cfg(feature = "rtp"));
make_error_fn!(make_rtp_error, "Rtp", cfg(feature = "rtp"));
make_error_fn!(make_srt_error, "Srt", cfg(feature = "srt"));
make_error_fn!(make_udp_error, "Udp", cfg(feature = "udp"));
make_error_fn!(make_tcp_error, "Tcp", cfg(feature = "tcp"));
make_error_fn!(make_hls_error, "Hls", cfg(feature = "hls"));
make_error_fn!(make_rist_error, "Rist", cfg(feature = "rist"));

/// Test helper: forces a `MuxError` raise from Rust, used by
/// `test_error_wiring.py` to confirm end-to-end wiring. Exposed only
/// under the `_native._raise_mux_error_for_test` name.
#[pyfunction]
#[pyo3(name = "_raise_mux_error_for_test")]
pub fn raise_mux_error_for_test(py: Python<'_>, message: &str) -> PyResult<()> {
    Err(make_mux_error(py, "INTERNAL", message))
}

/// Test helper: forces an `SrtError` raise from Rust, exposed as
/// `_native._raise_srt_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. (The kind
/// argument is a runtime `&str`, so this call site is invisible to
/// the literal-kind grep in `scripts/check/python/error-mapping-coverage.sh`
/// — that ratchet is covered by the mapper functions' own literal
/// call sites, not by this helper.)
#[cfg(feature = "srt")]
#[pyfunction]
#[pyo3(name = "_raise_srt_error_for_test")]
pub fn raise_srt_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(make_srt_error(py, kind, message))
}

/// Test helper: forces a `UdpError` raise from Rust, exposed as
/// `_native._raise_udp_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. (Runtime
/// `&str` kind — not visible to the error-mapping-coverage ratchet;
/// see the `srt` helper above.)
#[cfg(feature = "udp")]
#[pyfunction]
#[pyo3(name = "_raise_udp_error_for_test")]
pub fn raise_udp_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(make_udp_error(py, kind, message))
}

/// Test helper: forces a `TcpError` raise from Rust, exposed as
/// `_native._raise_tcp_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. (Runtime
/// `&str` kind — not visible to the error-mapping-coverage ratchet;
/// see the `srt` helper above.)
#[cfg(feature = "tcp")]
#[pyfunction]
#[pyo3(name = "_raise_tcp_error_for_test")]
pub fn raise_tcp_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(make_tcp_error(py, kind, message))
}

/// Test helper: forces an `HlsError` raise from Rust, exposed as
/// `_native._raise_hls_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. (Runtime
/// `&str` kind — not visible to the error-mapping-coverage ratchet;
/// see the `srt` helper above.)
#[cfg(feature = "hls")]
#[pyfunction]
#[pyo3(name = "_raise_hls_error_for_test")]
pub fn raise_hls_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(make_hls_error(py, kind, message))
}

/// Test helper: forces a `RistError` raise from Rust, exposed as
/// `_native._raise_rist_error_for_test` so `test_error_wiring.py` can
/// confirm end-to-end wiring for a caller-supplied kind. (Runtime
/// `&str` kind — not visible to the error-mapping-coverage ratchet;
/// see the `srt` helper above.)
#[cfg(feature = "rist")]
#[pyfunction]
#[pyo3(name = "_raise_rist_error_for_test")]
pub fn raise_rist_error_for_test(py: Python<'_>, kind: &str, message: &str) -> PyResult<()> {
    Err(make_rist_error(py, kind, message))
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
    use tst_core::error::MuxErrorKind;
    let kind_str = match e.kind() {
        MuxErrorKind::InputMalformed => "INPUT_MALFORMED",
        MuxErrorKind::ConfigInvalid => "CONFIG_INVALID",
        MuxErrorKind::InvalidUsage => "INVALID_USAGE",
        MuxErrorKind::Backpressure => "BACKPRESSURE",
        MuxErrorKind::Internal => "INTERNAL",
        _ => "INTERNAL",
    };
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
    make_mux_error(py, kind_str, &msg)
}

/// Map a Rust `CodecParseError` to a Python `CodecError` instance.
///
/// `codec` is a short lowercase string naming the codec that failed
/// (e.g. `"h264"`, `"h265"`, `"aac"`). Forwards all variant-specific
/// fields as keyword arguments to `CodecError.__init__` so the Python
/// side can read `.offset_bits`, `.field`, `.expected`, etc.
///
/// The wildcard arm routes unknown future variants (added via the
/// `#[non_exhaustive]` hatch on `CodecParseError`) to `ENGINE_ERROR`
/// so this fn never panics on a Rust-side enum addition — the
/// `pyarm` row in `scripts/ratchets/error-mapping.tsv` (driven by
/// `scripts/check/python/error-mapping-coverage.sh`) will surface the
/// omission in CI.
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
    let (kind_name, extra_attrs): (&str, Vec<(&str, PyObject)>) = match err {
        CodecParseError::TruncatedRbsp {
            offset_bits,
            needed_bits,
        } => (
            "TRUNCATED_RBSP",
            vec![
                ("offset_bits", offset_bits.into_py(py)),
                ("needed_bits", needed_bits.into_py(py)),
            ],
        ),
        CodecParseError::InvalidGolomb { offset_bits } => (
            "INVALID_GOLOMB",
            vec![("offset_bits", offset_bits.into_py(py))],
        ),
        CodecParseError::ReservedValue { field, value } => (
            "RESERVED_VALUE",
            vec![
                ("field", (*field).into_py(py)),
                ("value", value.into_py(py)),
            ],
        ),
        CodecParseError::UnsupportedProfile { profile_idc } => (
            "UNSUPPORTED_PROFILE",
            vec![("profile_idc", profile_idc.into_py(py))],
        ),
        CodecParseError::DanglingSpsReference { sps_id } => (
            "DANGLING_SPS_REFERENCE",
            vec![("sps_id", sps_id.into_py(py))],
        ),
        CodecParseError::DanglingVpsReference { vps_id } => (
            "DANGLING_VPS_REFERENCE",
            vec![("vps_id", vps_id.into_py(py))],
        ),
        CodecParseError::EngineError(_) => ("ENGINE_ERROR", vec![]),
        CodecParseError::InvalidLeb128 { offset_bytes } => (
            "INVALID_LEB128",
            vec![("offset_bytes", offset_bytes.into_py(py))],
        ),
        CodecParseError::BadSyncWord { expected, found } => (
            "BAD_SYNC_WORD",
            vec![
                ("expected", expected.into_py(py)),
                ("found", found.into_py(py)),
            ],
        ),
        CodecParseError::Truncated { needed, had } => (
            "TRUNCATED",
            vec![("needed", needed.into_py(py)), ("had", had.into_py(py))],
        ),
        CodecParseError::Forbidden { field } => {
            ("FORBIDDEN", vec![("field", (*field).into_py(py))])
        }
        CodecParseError::UnsupportedFreeFormat { layer } => (
            "UNSUPPORTED_FREE_FORMAT",
            vec![("layer", layer.into_py(py))],
        ),
        CodecParseError::InvalidLengthSize { got } => {
            ("INVALID_LENGTH_SIZE", vec![("got", got.into_py(py))])
        }
        CodecParseError::NalLengthOverflow {
            nal_len,
            length_size,
        } => (
            "NAL_LENGTH_OVERFLOW",
            vec![
                ("nal_len", nal_len.into_py(py)),
                ("length_size", length_size.into_py(py)),
            ],
        ),
        // Not reachable from Python: `BufferTooSmall` is raised only by
        // the Rust write-into-a-caller-buffer entry points
        // (`annexb_to_length_prefixed_into`), which have no Python
        // binding — every Python conversion returns a fresh `bytes`.
        // Mapped explicitly anyway (rather than left to the wildcard) so
        // the `pyarm` coverage rail stays honest, and to `ENGINE_ERROR`
        // rather than a new kind, since no Python caller can observe it.
        CodecParseError::BufferTooSmall { .. } => ("ENGINE_ERROR", vec![]),
        // Catch-all for #[non_exhaustive] additions not yet mapped:
        _ => ("ENGINE_ERROR", vec![]),
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
    let (kind_str, tag): (&str, Option<u64>) = match &e {
        RustE::BufferTooSmall { .. } => ("BUFFER_TOO_SMALL", None),
        RustE::RecordTooLarge => ("RECORD_TOO_LARGE", None),
        RustE::OutOfRange { tag, .. } => ("OUT_OF_RANGE", Some(u64::from(*tag))),
        RustE::StringTooLong { tag, .. } => ("STRING_TOO_LONG", Some(u64::from(*tag))),
        RustE::UnsupportedImapbLength { .. } => ("UNSUPPORTED_IMAPB_LENGTH", None),
        RustE::InvalidImapbParams { .. } => ("INVALID_IMAPB_PARAMS", None),
        RustE::MissingMandatoryItem { tag, .. } => {
            ("MISSING_MANDATORY_ITEM", Some(u64::from(*tag)))
        }
        RustE::ReservedTagInUnknown { tag } => ("RESERVED_TAG_IN_UNKNOWN", Some(u64::from(*tag))),
        RustE::VTargetPackEmpty { target_id } => ("VTARGET_PACK_EMPTY", Some(*target_id)),
        RustE::DuplicateTargetId { target_id } => ("DUPLICATE_TARGET_ID", Some(*target_id)),
        RustE::ForbiddenStandaloneOffset { tag } => {
            ("FORBIDDEN_STANDALONE_OFFSET", Some(u64::from(*tag)))
        }
        _ => ("BUFFER_TOO_SMALL", None),
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
