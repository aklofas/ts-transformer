//! The ONE Rust→Python raise path (Arc 2 WP-B2, spec §3.3 + §5).
//!
//! Every failure reaches Python as a `tst_pipeline::binding::BindingError`
//! (built by the `From<…>` impls next to each Rust error type) and is raised
//! through [`raise`]: the domain's exception class is constructed with
//! `kind=getattr(<Domain>ErrorKind, kind.name())` — `name()` is the
//! kind's binding string with the domain prefix stripped (`UdpIo` → `IO`),
//! which is what the per-domain Python enums spell (`variant_name()` is the
//! prefixed, unique spelling used only in diagnostics). The Python enums carry
//! no mapping table of their own, and a member the enum lacks is a STARTUP
//! failure: [`check_error_kinds`] resolves every member of every domain's
//! `kinds` when `tstrans._native` initialises (`lib.rs`), so drift between
//! `exceptions.py` and the Rust table fails `import tstrans`, never a user's
//! `except` clause.
//!
//! Two kinds have no member on purpose: `PanicCaught` (a panic inside the
//! slot, isolated by `Owned::with_mut`) → PyO3's `pyo3_runtime.PanicException`,
//! the type a panic surfaced as before Arc 2; and `Internal` on a domain that
//! does not list it (`HandleState::Poisoned`, A2's K7 forward-compat wildcards
//! for a future `#[non_exhaustive]` variant) → `RuntimeError`. Domains that DO
//! list `Internal` (hls, mux, klv-decode) raise their own `INTERNAL`.
//!
//! The mux / demux / klv / codec exceptions carry per-variant attributes
//! (`pid`, `tag`, `offset_bits`, the `write_file` breadcrumb), so their
//! mappers keep constructing the class themselves — with the member taken
//! from A2's `kind_of_*(&e).name()` — and only their `KINDS` live here, for
//! the import-time check.

use pyo3::exceptions::{PyImportError, PyRuntimeError};
use pyo3::intern;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K, HandleState};

pub(crate) struct Domain {
    pub error_class: &'static str,
    pub kind_enum: &'static str,
    /// The subset of `BindingErrorKind` this domain's enum carries — every
    /// `name()` here must resolve on `kind_enum` at import.
    pub kinds: &'static [K],
}

#[cfg(feature = "srt")]
pub(crate) static SRT: Domain = Domain {
    error_class: "SrtError",
    kind_enum: "SrtErrorKind",
    kinds: &[
        K::SrtConnectFailed,
        K::SrtAcceptFailed,
        K::Backpressure,
        K::SrtTimeout,
        K::Closed,
        K::Broken,
        K::ConfigInvalid,
        K::SrtIo,
        K::TooLarge,
        K::InputMalformed,
        K::EndOfStream,
    ],
};

#[cfg(feature = "rtp")]
pub(crate) static RTP: Domain = Domain {
    error_class: "RtpError",
    kind_enum: "RtpErrorKind",
    kinds: &[
        K::Broken,
        K::TooLarge,
        K::Closed,
        K::Backpressure,
        K::RtpPayloadTypeParam,
        K::RtpMissingPayloadTypeParam,
        K::RtpUrl,
        K::RtpHostNotLiteral,
        K::RtpIo,
        K::RtpIfaceUnsupported,
        K::EndOfStream,
    ],
};

#[cfg(feature = "rtp")]
pub(crate) static RTSP: Domain = Domain {
    error_class: "RtspError",
    kind_enum: "RtspErrorKind",
    kinds: &[
        K::RtspProtocol,
        K::RtspAuthFailed,
        K::RtspAuthRequired,
        K::RtspNotFound,
        K::RtspUnsupportedTransport,
        K::RtspTls,
        K::RtspIo,
        K::RtspTimeout,
        K::RtspServer,
        K::RtspMount,
        // The shared closed-handle kind: `HandleState::Closed` reaches this
        // domain through the rtsp client's closable slots (B2.9b). Without
        // the member it would degrade to a misdiagnosing `RuntimeError`.
        K::Closed,
    ],
};

#[cfg(feature = "udp")]
pub(crate) static UDP: Domain = Domain {
    error_class: "UdpError",
    kind_enum: "UdpErrorKind",
    kinds: &[
        K::UdpUrl,
        K::UdpIo,
        K::TooLarge,
        K::Closed,
        K::UdpInvalidConfig,
        K::Backpressure,
        K::Broken,
    ],
};

#[cfg(feature = "tcp")]
pub(crate) static TCP: Domain = Domain {
    error_class: "TcpError",
    kind_enum: "TcpErrorKind",
    kinds: &[
        K::TcpUrl,
        K::TcpIo,
        K::TooLarge,
        K::Closed,
        K::TcpConnectTimeout,
        K::TcpInvalidConfig,
        K::TcpTls,
        K::TcpTlsDisabled,
        K::Backpressure,
        K::Broken,
    ],
};

#[cfg(feature = "hls")]
pub(crate) static HLS: Domain = Domain {
    error_class: "HlsError",
    kind_enum: "HlsErrorKind",
    kinds: &[
        K::HlsUrl,
        K::HlsIo,
        K::HlsBindFailed,
        K::HlsInvalidConfig,
        K::HlsUnalignedPushTs,
        K::HlsFinished,
        K::HlsTlsDisabled,
        K::HlsTls,
        K::Internal,
        K::Closed,
    ],
};

#[cfg(feature = "rist")]
pub(crate) static RIST: Domain = Domain {
    error_class: "RistError",
    kind_enum: "RistErrorKind",
    kinds: &[
        K::RistUrl,
        K::RistFfi,
        K::TooLarge,
        K::Closed,
        K::RistInvalidConfig,
        K::RistEncryptionDisabled,
        K::RistContextCreateFailed,
        K::RistPeerCreateFailed,
        K::Backpressure,
        K::Broken,
    ],
};

// Checked at import; raised by their own attribute-carrying mappers.
pub(crate) static MUX: Domain = Domain {
    error_class: "MuxError",
    kind_enum: "MuxErrorKind",
    kinds: &[
        K::InputMalformed,
        K::ConfigInvalid,
        K::InvalidUsage,
        K::Backpressure,
        K::Internal,
        K::InvalidNal,
        K::KlvTooLarge,
        K::InvalidAv1Obu,
        K::MispTime,
    ],
};

pub(crate) static DEMUX: Domain = Domain {
    error_class: "DemuxError",
    kind_enum: "DemuxErrorKind",
    kinds: &[
        K::DemuxUnrecoverable,
        K::DemuxStrictRejection,
        K::DemuxMalformedPsi,
        K::DemuxMalformedPes,
        K::DemuxSyncBufExhausted,
    ],
};

pub(crate) static KLV_DECODE: Domain = Domain {
    error_class: "KlvError",
    kind_enum: "KlvErrorKind",
    kinds: &[
        K::KlvDecodeTruncatedSet,
        K::KlvDecodeBadUniversalLabel,
        K::KlvDecodeChecksumMismatch,
        K::KlvDecodeDuplicateTag,
        K::KlvDecodeMissingRequiredTag,
        K::KlvDecodeMalformedBytes,
        K::Internal,
    ],
};

pub(crate) static KLV_ENCODE: Domain = Domain {
    error_class: "KlvEncodeError",
    kind_enum: "KlvEncodeErrorKind",
    kinds: &[
        K::KlvEncodeBufferTooSmall,
        K::KlvEncodeRecordTooLarge,
        K::KlvEncodeOutOfRange,
        K::KlvEncodeStringTooLong,
        K::KlvEncodeUnsupportedImapbLength,
        K::KlvEncodeInvalidImapbParams,
        K::KlvEncodeMissingMandatoryItem,
        K::KlvEncodeReservedTagInUnknown,
        K::KlvEncodeVTargetPackEmpty,
        K::KlvEncodeDuplicateTargetId,
        K::KlvEncodeForbiddenStandaloneOffset,
    ],
};

pub(crate) static CODEC: Domain = Domain {
    error_class: "CodecError",
    kind_enum: "CodecErrorKind",
    kinds: &[
        K::CodecTruncatedRbsp,
        K::CodecInvalidGolomb,
        K::CodecReservedValue,
        K::CodecUnsupportedProfile,
        K::CodecDanglingSpsReference,
        K::CodecDanglingVpsReference,
        K::CodecEngineError,
        K::CodecInvalidLeb128,
        K::CodecBadSyncWord,
        K::CodecTruncated,
        K::CodecForbidden,
        K::CodecUnsupportedFreeFormat,
        K::CodecInvalidLengthSize,
        K::CodecNalLengthOverflow,
        K::CodecBufferTooSmall,
    ],
};

pub(crate) static DOMAINS: &[&Domain] = &[
    #[cfg(feature = "srt")]
    &SRT,
    #[cfg(feature = "rtp")]
    &RTP,
    #[cfg(feature = "rtp")]
    &RTSP,
    #[cfg(feature = "udp")]
    &UDP,
    #[cfg(feature = "tcp")]
    &TCP,
    #[cfg(feature = "hls")]
    &HLS,
    #[cfg(feature = "rist")]
    &RIST,
    &MUX,
    &DEMUX,
    &KLV_DECODE,
    &KLV_ENCODE,
    &CODEC,
];

/// Raise `e` as `d`'s exception class (see the module doc for the two
/// member-less kinds).
#[allow(dead_code)] // callers land with the per-domain re-points in WP-B2.
pub(crate) fn raise(py: Python<'_>, d: &Domain, e: BindingError) -> PyErr {
    if e.kind == K::PanicCaught {
        return PanicException::new_err(e.detail);
    }
    if e.kind == K::Internal && !d.kinds.contains(&K::Internal) {
        return PyRuntimeError::new_err(e.detail);
    }
    let name = e.kind.name();
    let built: PyResult<PyErr> = (|| {
        let exceptions = py.import_bound(intern!(py, "tstrans.exceptions"))?;
        let kind_enum = exceptions.getattr(d.kind_enum)?;
        let value = kind_enum.getattr(name).map_err(|_| {
            PyRuntimeError::new_err(format!(
                "tstrans.exceptions.{}.{} is missing for BindingErrorKind::{} — \
                 the import-time check did not run",
                d.kind_enum,
                name,
                e.kind.variant_name()
            ))
        })?;
        let cls = exceptions.getattr(d.error_class)?;
        let kwargs = PyDict::new_bound(py);
        kwargs.set_item(intern!(py, "kind"), value)?;
        kwargs.set_item(intern!(py, "message"), e.detail.as_str())?;
        Ok(PyErr::from_value_bound(cls.call((), Some(&kwargs))?))
    })();
    built.unwrap_or_else(|err| err)
}

/// Flatten `Owned::with_mut(|t| t.op())`: the handle state and the op's own
/// error both go through [`raise`].
#[allow(dead_code)] // callers land with the per-class `Owned` re-points in WP-B2.
pub(crate) fn pyres<R, E: Into<BindingError>>(
    py: Python<'_>,
    d: &Domain,
    r: Result<Result<R, E>, HandleState>,
) -> PyResult<R> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(raise(py, d, e.into())),
        Err(state) => Err(raise(py, d, state.into())),
    }
}

pub(crate) fn pyok<R>(py: Python<'_>, d: &Domain, r: Result<R, HandleState>) -> PyResult<R> {
    r.map_err(|state| raise(py, d, state.into()))
}

/// The domain's kind whose `name()` is `name` (test helpers; names like
/// `IO` exist in several domains, so the lookup is per domain).
// Its only caller, `errors::raise_for_test`, needs one of the transport
// features; a bare `--no-default-features` build has none.
#[allow(dead_code)]
pub(crate) fn kind_by_name(d: &Domain, name: &str) -> Option<K> {
    d.kinds.iter().copied().find(|k| k.name() == name)
}

/// Spec §3.3 startup check: every member of every domain must resolve.
pub(crate) fn check_error_kinds(py: Python<'_>) -> PyResult<()> {
    let exceptions = py.import_bound(intern!(py, "tstrans.exceptions"))?;
    for d in DOMAINS {
        let kind_enum = exceptions.getattr(d.kind_enum)?;
        for k in d.kinds {
            if kind_enum.getattr(k.name()).is_err() {
                return Err(PyImportError::new_err(format!(
                    "tstrans.exceptions.{}.{} is missing (required by BindingErrorKind::{}); \
                     the Python kind enum and the Rust kind table are out of step",
                    d.kind_enum,
                    k.name(),
                    k.variant_name()
                )));
            }
        }
    }
    Ok(())
}

/// `tstrans._native._check_error_kinds()` — the same walk `_native`'s init
/// performs, exposed so the pytest suite can mutate one enum and watch it
/// refuse without re-importing the extension.
// PyO3 0.22's `#[pyfunction]` expansion emits a `PyErr -> PyErr` `.into()`
// on the wrapped result; the lint fires on the generated code, not on
// anything written here. Scoped to this one item (`errors.rs` carries the
// same suppression module-wide for the same reason).
#[allow(clippy::useless_conversion)]
mod check_fn {
    use super::check_error_kinds;
    use pyo3::prelude::*;

    #[pyfunction]
    #[pyo3(name = "_check_error_kinds")]
    pub(crate) fn check_error_kinds_py(py: Python<'_>) -> PyResult<()> {
        check_error_kinds(py)
    }
}

pub(crate) use check_fn::check_error_kinds_py;
