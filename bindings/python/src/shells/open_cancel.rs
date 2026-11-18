//! `OpenCancel`, the host's cancel for opens that return no shell yet, and
//! the two steps every open takes with it (`begin` before the URL is
//! parsed, `finish` inside the parking open).

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

use tst_core::transport::TransportError;
use tst_pipeline::binding::{BindingError, OpenCancel, OpenSlot};

/// A sticky cancel for opens in progress: `cancel()` ends every open
/// parked in a listener-mode first accept with it, and every open given it
/// later raises `CLOSED` without opening anything.
#[pyclass(frozen, name = "OpenCancel", module = "tstrans.shells")]
pub(crate) struct PyOpenCancel {
    inner: OpenCancel,
}

#[pymethods]
impl PyOpenCancel {
    #[new]
    fn new() -> Self {
        Self {
            inner: OpenCancel::new(),
        }
    }

    /// Cancel every open in progress with this object and refuse every
    /// later one. Idempotent; safe from any thread.
    ///
    /// Waking a first accept closes its listening socket, which can wait,
    /// so it runs without the GIL.
    fn cancel(&self, py: Python<'_>) {
        crate::util::allow_threads_parking(py, || self.inner.cancel());
    }

    /// `True` once `cancel()` has run.
    fn is_cancelled(&self, py: Python<'_>) -> bool {
        // The flag sits under the object's mutex: read it without the GIL.
        crate::util::allow_threads_parking(py, || self.inner.is_cancelled())
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("OpenCancel(cancelled={})", self.is_cancelled(py))
    }
}

/// The object an open runs under: the caller's `cancel=`, or a private one
/// nothing cancels (so an open without the argument behaves as before).
pub(crate) fn object_of(cancel: Option<&Bound<'_, PyOpenCancel>>) -> OpenCancel {
    cancel.map_or_else(OpenCancel::new, |c| c.get().inner.clone())
}

/// Take one open's slot, without the GIL (the object's mutex). `None`: the
/// object was already cancelled, and the open raises `CLOSED` before it
/// parses its URL.
pub(crate) fn begin(py: Python<'_>, oc: &OpenCancel) -> Option<OpenSlot> {
    crate::util::allow_threads_parking(py, || {
        let slot = oc.slot();
        (!slot.is_cancelled()).then_some(slot)
    })
}

/// What an open raises when its object is cancelled: the transport's
/// `CLOSED` kind.
pub(crate) fn closed() -> BindingError {
    BindingError::from(TransportError::ExplicitClose)
}

/// End an open, without the GIL (dropping what it opened can close a
/// socket): when the object was cancelled by now, `r` is dropped, a
/// success included, and the open reports `CLOSED`; otherwise `r` is
/// returned whole and the object never touches it again.
pub(crate) fn finish<T>(slot: OpenSlot, r: Result<T, BindingError>) -> Result<T, BindingError> {
    if slot.finish() {
        drop(r);
        return Err(closed());
    }
    r
}

/// `_open_cancel_with_slow_open_for_test(delay_ms)` -> an `OpenCancel` with
/// one open in progress whose wake takes `delay_ms`, as a listener close
/// can. Test-only: the open never finishes, so its slot stays registered
/// (leaked) until `cancel()` takes it.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_open_cancel_with_slow_open_for_test")]
pub(crate) fn open_cancel_with_slow_open_for_test(delay_ms: u64) -> PyOpenCancel {
    let inner = OpenCancel::new();
    let slot = inner.slot();
    slot.install(crate::shells::cancel::slow_cancel(delay_ms));
    std::mem::forget(slot);
    PyOpenCancel { inner }
}
