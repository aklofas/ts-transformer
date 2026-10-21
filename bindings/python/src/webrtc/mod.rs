//! `tstrans.webrtc` — the WHEP browser-playback server: `WhepServer`
//! (`server.rs`) and `MountHandle` + `SessionStats` (`mount.rs`). The
//! `ServerStats` / `MountStats` records are generated
//! (`crate::generated::stats_webrtc`); `MountConfig` and its enums are the
//! pure-Python dataclass in `tstrans/webrtc.py`.
//!
//! Errors are `WebrtcError` through the shared kind table: the `From` impls
//! in `crates/tst-webrtc/src/binding_kind.rs` build the `BindingError`, and
//! this module holds no mapping of its own.
//!
//! Neither class holds a lock. `tst_webrtc::WhepServer` and `MountHandle`
//! take `&self` everywhere and guard their own state, so each wrapper keeps
//! the value and an atomic closed latch: a call that waits (a statistics
//! read waits up to 2 s for the media thread, `stop` up to 2 s per thread)
//! never makes another thread's `close()` wait behind it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

mod mount;
mod server;

use pyo3::prelude::*;
use tst_pipeline::binding::{BindingError, HandleState};

use crate::raise::{WEBRTC, raise};

/// A `tst-webrtc` error, or a handle state, as `WebrtcError`.
fn webrtc_err(py: Python<'_>, e: impl Into<BindingError>) -> PyErr {
    raise(py, &WEBRTC, e.into())
}

/// `WebrtcError(CLOSED)` for a call on a closed wrapper.
fn closed(py: Python<'_>) -> PyErr {
    webrtc_err(py, HandleState::Closed)
}

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "webrtc")?;
    m.add_class::<server::PyWhepServer>()?;
    m.add_class::<mount::PyMountHandle>()?;
    m.add_class::<mount::PySessionStats>()?;
    crate::generated::stats_webrtc::register(&m)?;
    parent.add_submodule(&m)?;
    Ok(())
}
