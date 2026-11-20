//! `OpenCancel`, the host's cancel for opens that return no shell yet, and
//! the two steps every open takes with it (`begin` before the URL is
//! parsed, `finish` inside the parking open).

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

use tst_core::transport::TransportError;
use tst_pipeline::binding::{BindingError, Domain, OpenCancel, OpenSlot};

use crate::raise::raise;

/// A sticky cancel for opens in progress: `cancel()` ends every open
/// parked in a listener-mode first accept with it, and every open given it
/// later raises `CLOSED` without opening anything. With an
/// already-cancelled object an open is refused with `CLOSED` before its
/// configuration and its URL are looked at. Ctrl-C does not interrupt an
/// open parked in a first accept; cancel it from another thread through
/// this object.
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
    // Read with the GIL held: the object's mutex is held only for a flag or
    // a list edit, never across a slot's cancel or a GIL release. Releasing
    // the GIL here would cost the exit park after the interpreter-exit hook.
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("OpenCancel(cancelled={})", self.is_cancelled())
    }
}

/// The object an open runs under: the caller's `cancel=`, or a private one
/// nothing cancels (so an open without the argument behaves as before).
pub(crate) fn object_of(cancel: Option<&Bound<'_, PyOpenCancel>>) -> OpenCancel {
    cancel.map_or_else(OpenCancel::new, |c| c.get().inner.clone())
}

/// Take one open's slot. `None`: the object was already cancelled, and the
/// open raises `CLOSED` before it parses its URL.
///
/// Runs with the GIL held, for the reason `is_cancelled` gives: the
/// object's mutex and the fresh slot's are each held for a flag or a list
/// edit, never across a cancel's wake or a GIL release.
pub(crate) fn begin(oc: &OpenCancel) -> Option<OpenSlot> {
    let slot = oc.slot();
    (!slot.is_cancelled()).then_some(slot)
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

/// A failure of an open before it touched its transport (a URL it cannot
/// parse, a mode it does not take), reported through [`finish`] like any
/// other failure of the open. Nothing was opened, so it may run with the
/// GIL held.
pub(crate) fn refuse(slot: OpenSlot, e: BindingError) -> BindingError {
    finish(slot, Err::<(), _>(e)).unwrap_err()
}

/// [`refuse`] for a configuration the open refused with a Python exception
/// (`MuxError`, or a field `TypeError` / `ValueError` / `OverflowError`):
/// the domain's `CLOSED` when the object was cancelled by now, else `e`.
pub(crate) fn refuse_config(py: Python<'_>, domain: &Domain, slot: OpenSlot, e: PyErr) -> PyErr {
    if slot.finish() {
        raise(py, domain, closed())
    } else {
        e
    }
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
