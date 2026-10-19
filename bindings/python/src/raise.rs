//! The ONE Rust→Python raise path.
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
//! slot, isolated by `Owned::with_mut`) → PyO3's `pyo3_runtime.PanicException`;
//! and `Internal` on a domain that does not list it (`HandleState::Poisoned`,
//! the forward-compat wildcards for a future `#[non_exhaustive]` variant) →
//! `RuntimeError`. Domains that DO
//! list `Internal` (hls, mux, klv-decode) raise their own `INTERNAL`.
//!
//! The mux / demux / klv / codec exceptions carry per-variant attributes
//! (`pid`, `tag`, `offset_bits`, the `write_file` breadcrumb), so their
//! mappers keep constructing the class themselves — with the member taken
//! from `kind_of_*(&e).name()` — and only their `KINDS` live here, for
//! the import-time check.

use pyo3::exceptions::{PyImportError, PyRuntimeError};
use pyo3::intern;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::types::PyDict;
pub(crate) use tst_pipeline::binding::Domain;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K, HandleState};

#[cfg(feature = "srt")]
pub(crate) const SRT: Domain = Domain::Srt;
#[cfg(feature = "rtp")]
pub(crate) const RTP: Domain = Domain::Rtp;
#[cfg(feature = "rtp")]
pub(crate) const RTSP: Domain = Domain::Rtsp;
#[cfg(feature = "udp")]
pub(crate) const UDP: Domain = Domain::Udp;
#[cfg(feature = "tcp")]
pub(crate) const TCP: Domain = Domain::Tcp;
#[cfg(feature = "hls")]
pub(crate) const HLS: Domain = Domain::Hls;
#[cfg(feature = "rist")]
pub(crate) const RIST: Domain = Domain::Rist;
pub(crate) const MUX: Domain = Domain::Mux;
pub(crate) const DEMUX: Domain = Domain::Demux;
pub(crate) const KLV_DECODE: Domain = Domain::KlvDecode;
pub(crate) const KLV_ENCODE: Domain = Domain::KlvEncode;
pub(crate) const CODEC: Domain = Domain::Codec;

pub(crate) static DOMAINS: &[Domain] = &[
    #[cfg(feature = "srt")]
    SRT,
    #[cfg(feature = "rtp")]
    RTP,
    #[cfg(feature = "rtp")]
    RTSP,
    #[cfg(feature = "udp")]
    UDP,
    #[cfg(feature = "tcp")]
    TCP,
    #[cfg(feature = "hls")]
    HLS,
    #[cfg(feature = "rist")]
    RIST,
    MUX,
    DEMUX,
    KLV_DECODE,
    KLV_ENCODE,
    CODEC,
];

/// Raise `e` as `d`'s exception class (see the module doc for the two
/// member-less kinds).
#[allow(dead_code)] // every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
pub(crate) fn raise(py: Python<'_>, d: &Domain, e: BindingError) -> PyErr {
    if e.kind == K::PanicCaught {
        return PanicException::new_err(e.detail);
    }
    if e.kind == K::Internal && !d.kinds().contains(&K::Internal) {
        return PyRuntimeError::new_err(e.detail);
    }
    let name = e.kind.name();
    let built: PyResult<PyErr> = (|| {
        let exceptions = py.import(intern!(py, "tstrans.exceptions"))?;
        let kind_enum = exceptions.getattr(d.python_kind_enum())?;
        let value = kind_enum.getattr(name).map_err(|_| {
            PyRuntimeError::new_err(format!(
                "tstrans.exceptions.{}.{} is missing for BindingErrorKind::{} — \
                 the import-time check did not run",
                d.python_kind_enum(),
                name,
                e.kind.variant_name()
            ))
        })?;
        let cls = exceptions.getattr(d.python_error_class())?;
        let kwargs = PyDict::new(py);
        kwargs.set_item(intern!(py, "kind"), value)?;
        kwargs.set_item(intern!(py, "message"), e.detail.as_str())?;
        Ok(PyErr::from_value(cls.call((), Some(&kwargs))?))
    })();
    built.unwrap_or_else(|err| err)
}

/// Flatten `Owned::with_mut(|t| t.op())`: the handle state and the op's own
/// error both go through [`raise`].
#[allow(dead_code)] // every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
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

// Only the transport surfaces call this; cfg (not `allow(dead_code)`) so a
// genuinely-orphaned helper would still warn.
#[cfg(any(
    feature = "srt",
    feature = "rtp",
    feature = "udp",
    feature = "tcp",
    feature = "rist"
))]
pub(crate) fn pyok<R>(py: Python<'_>, d: &Domain, r: Result<R, HandleState>) -> PyResult<R> {
    r.map_err(|state| raise(py, d, state.into()))
}

/// The domain's kind whose `name()` is `name` (test helpers; names like
/// `IO` exist in several domains, so the lookup is per domain).
// Its only caller, `errors::raise_for_test`, needs one of the transport
// features; a bare `--no-default-features` build has none.
#[allow(dead_code)]
pub(crate) fn kind_by_name(d: &Domain, name: &str) -> Option<K> {
    d.kinds().iter().copied().find(|k| k.name() == name)
}

/// Startup check: every member of every domain must resolve.
pub(crate) fn check_error_kinds(py: Python<'_>) -> PyResult<()> {
    let exceptions = py.import(intern!(py, "tstrans.exceptions"))?;
    for d in DOMAINS {
        let kind_enum = exceptions.getattr(d.python_kind_enum())?;
        for k in d.kinds() {
            if kind_enum.getattr(k.name()).is_err() {
                return Err(PyImportError::new_err(format!(
                    "tstrans.exceptions.{}.{} is missing (required by BindingErrorKind::{}); \
                     the Python kind enum and the Rust kind table are out of step",
                    d.python_kind_enum(),
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
// PyO3's `#[pyfunction]` expansion emits a `PyErr -> PyErr` `.into()`
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
