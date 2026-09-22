//! Python bindings for tst-hls (`tstrans.hls`). Gated on `feature = "hls"`.
//!
//! Populated by Plan A5b Wave C. HLS lives in the `tst-hls` crate
//! (`tst_hls`) — the `hls` cargo feature here pulls
//! `tst-hls` + `tst-pipeline` (for the `MuxPublisher` shell).
//!
//! Surface (module `"tstrans.hls"`):
//! - `Publisher` (ABC) + `PublisherStats` — T10
//! - `MuxPublisher` + `MuxPublisherStats` — T11
//! - `HlsPublisher` + `HlsPublisherBuilder` — T12
//! - `HlsMode` + `HlsStats` — T13
//! - `HlsError` / `HlsErrorKind` (in `tstrans.exceptions`) + error
//!   mapping + ratchets — T14
//!
//! GIL boundaries: `push_ts` / `cut_segment` / `finish` / builder
//! `build` release the GIL via `py.allow_threads` (disk + HTTP work is
//! pure Rust). Read-only getters do not release it.
//!
//! Error mapping: `tst_hls::HlsError` → `tstrans.exceptions.HlsError`
//! via `map_hls_error` (exhaustive over `HlsErrorKind`, with a wildcard
//! for the `#[non_exhaustive]` enum). The Rust `HlsErrorKind` is
//! 1-indexed; the Python `HlsErrorKind` IntEnum is 0-indexed — the
//! mapping uses enum *names* so there's no off-by-one.
//!
//! Two ratchets back this module:
//! - the consolidated `scripts/check/python/error-mapping-coverage.sh` —
//!   every `HlsErrorKind` variant has a `make_hls_error(py, "<KIND>", ...)`
//!   call site.
//! - `scripts/check/python/publisher-class-mirror.sh` — the Python
//!   `Publisher` ABC's abstract methods mirror the Rust
//!   `tst_core::publisher::Publisher` trait.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

use tst_hls::{HlsError, HlsUrlError};
use tst_pipeline::MuxPublisherError;

use crate::raise::{HLS, raise};
use tst_pipeline::binding::BindingError;

pub(crate) mod config;
pub(crate) mod mux_publisher;
pub(crate) mod publisher;
pub(crate) mod publisher_abc;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map a `tst_hls::HlsError` through the one raise path.
pub(crate) fn map_hls_error(py: Python<'_>, e: HlsError) -> PyErr {
    raise(py, &HLS, BindingError::from(e))
}

/// Map a `tst_hls::HlsUrlError` (from `builder.from_url`) — `URL`.
pub(crate) fn map_hls_url_error(py: Python<'_>, e: HlsUrlError) -> PyErr {
    raise(py, &HLS, BindingError::from(e))
}

/// Map a `tst_pipeline::MuxPublisherError<HlsError>` raised by a
/// `MuxPublisher` send/cut.
///
/// The source decides the CLASS: a MUX-sourced failure keeps
/// `mux_error_to_pyerr` (a `MuxError` carrying `.pid` and the `write_file`
/// breadcrumb — it was flattened to `HlsError(INVALID_CONFIG)` before
/// 0.7.0), everything else is a `BindingError` on the HLS domain via A2's
/// `From<MuxPublisherError<E>>`: `Publisher(e)` keeps the inner HLS kind,
/// `Closed` is `CLOSED` (was `FINISHED`), a poisoned lock is `INTERNAL`.
pub(crate) fn map_mux_publisher_error(py: Python<'_>, e: MuxPublisherError<HlsError>) -> PyErr {
    match e {
        MuxPublisherError::Mux(mux_err) => crate::errors::mux_error_to_pyerr(py, mux_err),
        other => raise(py, &HLS, BindingError::from(other)),
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "hls")?;
    // T10 — PublisherStats. The `Publisher` ABC itself is a pure-Python
    // `abc.ABC` defined in `tstrans/hls.py` (see the NOTE below).
    m.add_class::<publisher_abc::PyPublisherStats>()?;
    // T13 — HlsMode + HlsStats.
    m.add_class::<config::PyHlsMode>()?;
    m.add_class::<config::PyHlsStats>()?;
    // T12 — HlsPublisher + builder + server handle.
    m.add_class::<publisher::PyHlsPublisher>()?;
    m.add_class::<publisher::PyHlsPublisherBuilder>()?;
    m.add_class::<publisher::PyHlsServerHandle>()?;
    // T11 — MuxPublisher + MuxPublisherStats.
    m.add_class::<mux_publisher::PyMuxPublisher>()?;
    m.add_class::<mux_publisher::PyMuxPublisherStats>()?;

    // NOTE: the `Publisher` ABC + `Publisher.register(HlsPublisher)`
    // virtual-subclass wiring lives in the Python layer
    // (`tstrans/hls.py`), NOT here. A native PyO3 pyclass has the plain
    // `type` metaclass and no `.register()` classmethod, so the ABC must
    // be a real `abc.ABC` built in Python; the native crate exposes only
    // `PublisherStats` here.

    parent.add_submodule(&m)?;
    Ok(())
}
