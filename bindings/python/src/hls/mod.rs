//! Python bindings for tst-hls (`tstrans.hls`). Gated on `feature = "hls"`.
//!
//! HLS lives in the `tst-hls` crate
//! (`tst_hls`) — the `hls` cargo feature here pulls
//! `tst-hls` + `tst-pipeline` (for the `MuxPublisher` shell).
//!
//! Surface (module `"tstrans.hls"`):
//! - `Publisher` (ABC) + `PublisherStats`
//! - `MuxPublisher` + `MuxPublisherStats`
//! - `HlsPublisher` + `HlsPublisherBuilder`
//! - `HlsMode` + `HlsStats`
//! - `HlsError` / `HlsErrorKind` (in `tstrans.exceptions`) + error
//!   mapping
//!
//! GIL boundaries: `HlsPublisher` and `MuxPublisher` guard their inner
//! publisher with a mutex, and a push holds it with the GIL released. A
//! thread that waited for that mutex while holding the GIL would freeze the
//! interpreter (the push needs the GIL back before its guard can drop), so
//! every method of both classes takes, uses and releases the mutex inside
//! `py.allow_threads` and raises once the GIL is back — see [`Locked`]. The
//! construction constants (`local_addr`, `local_port`, `repr`) take no lock.
//!
//! Error mapping goes through `crate::raise`:
//! `From<HlsError>`/`From<HlsUrlError>` for `BindingError` live next to
//! the Rust types, and `raise` resolves the kind's `name()` on
//! `tstrans.exceptions.HlsErrorKind` — checked at `import tstrans`, so
//! there is no name table and no off-by-one to keep in sync.
//!
//! One ratchet backs this module:
//! `scripts/check/python/publisher-class-mirror.sh` — the Python
//! `Publisher` ABC's abstract methods mirror the Rust
//! `tst_core::publisher::Publisher` trait.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

use tst_hls::HlsError;
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

/// Why a call that takes a publisher's mutex produced no value. Built
/// inside `py.allow_threads`, where nothing can be raised, and turned into
/// the exception after the GIL is back.
pub(crate) enum Locked<E> {
    /// The mutex is poisoned.
    Poisoned,
    /// The inner publisher was already consumed.
    Gone,
    /// The native call itself failed.
    Inner(E),
}

/// Map a `tst_pipeline::MuxPublisherError<HlsError>` raised by a
/// `MuxPublisher` send/cut.
///
/// The source decides the CLASS: a MUX-sourced failure keeps
/// `mux_error_to_pyerr` (a `MuxError` carrying `.pid` and the `write_file`
/// breadcrumb), everything else is a `BindingError` on the HLS domain via
/// `From<MuxPublisherError<E>>`: `Publisher(e)` keeps the inner HLS kind,
/// `Closed` is `CLOSED`, a poisoned lock is `INTERNAL`.
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
    // PublisherStats. The `Publisher` ABC itself is a pure-Python
    // `abc.ABC` defined in `tstrans/hls.py` (see the NOTE below).
    m.add_class::<publisher_abc::PyPublisherStats>()?;
    // HlsMode + HlsStats.
    m.add_class::<config::PyHlsMode>()?;
    m.add_class::<config::PyHlsStats>()?;
    // HlsPublisher + builder + server handle.
    m.add_class::<publisher::PyHlsPublisher>()?;
    m.add_class::<publisher::PyHlsPublisherBuilder>()?;
    m.add_class::<publisher::PyHlsServerHandle>()?;
    // MuxPublisher + MuxPublisherStats.
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
