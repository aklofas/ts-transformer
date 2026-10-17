//! `CancelHandle`, the value class every shell hands out, plus the helper that
//! turns a transport's cancel handle into the shell's shared [`CancelSource`].

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::prelude::*;

use tst_core::transport::TransportCancel;

use crate::util::CancelSource;

/// Python-side cancel handle. Wraps the shell's shared
/// [`CancelSource`]: every clone obtained from the same shell, and the
/// shell's own `close()`, forwards into one cancel and flips one flag, so
/// `is_cancelled()` reports the shell's state, not this wrapper's history.
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.shells")]
pub(crate) struct PyCancelHandle {
    src: Arc<CancelSource>,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent. Wakes a thread parked in
    /// `send_bytes` / `recv_bytes` / `__next__`; that call raises the
    /// transport's `CLOSED` kind.
    fn cancel(&self) {
        TransportCancel::cancel(&*self.src);
    }

    /// `True` once the shell was cancelled or closed through ANY handle
    /// or its own `close()` (shared state).
    fn is_cancelled(&self) -> bool {
        self.src.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("CancelHandle(cancelled={})", self.is_cancelled())
    }
}

impl PyCancelHandle {
    /// Clone the shell's shared cancel state so this handle observes, and
    /// contributes to, the same cancel.
    pub(crate) fn from_source(src: &Arc<CancelSource>) -> Self {
        Self {
            src: Arc::clone(src),
        }
    }
}

/// The shell's shared cancel state over the transport's own cancel handle
/// (taken from `Opened` before the transport was boxed into the shell).
pub(crate) fn cancel_source(cancel: &Arc<dyn TransportCancel>) -> Arc<CancelSource> {
    CancelSource::new(Arc::clone(cancel))
}
