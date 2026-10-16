//! `Receiver`: any transport in, TS bytes out.
//!
//! Wraps `tst_pipeline::Receiver<Box<dyn RecvTransport>>`, opened from a URL
//! by `Receiver.open(url)`. Same handle model as `Sender`: an `Owned` slot
//! taken only under `py.allow_threads`, cancel-first `close()`, and the
//! transport's error domain in the snapshot. The transport's end-reason
//! handle is attached to the `Owned`, so `end_reason()` keeps answering
//! after `close()`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyType};

use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{Opened, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig};

#[cfg(feature = "srt")]
use crate::raise::raise;
use crate::raise::{pyok, pyres};
use crate::shells::cancel::{PyCancelHandle, PySocketStats, cancel_source};
use crate::shells::end_reason::{end_detail, end_reason_to_py};
use crate::util::{CancelSource, close_owned};

/// Python receiver over any transport — wraps
/// `tst_pipeline::Receiver<Box<dyn RecvTransport>>`.
///
/// `Receiver.open("srt://:7000?mode=listener")` binds, listens and accepts
/// ONE peer; the datagram and stream transports open their socket.
#[pyclass(name = "Receiver", module = "tstrans.shells")]
pub(crate) struct PyReceiver {
    owned: Owned<PlReceiver<Box<dyn RecvTransport>>, Snapshot<()>>,
    cancel: Arc<CancelSource>,
}

impl PyReceiver {
    /// Wrap an already-opened transport (the `open` and
    /// `Socket.into_receiver()` paths meet here).
    pub(crate) fn from_opened(
        _py: Python<'_>,
        opened: Opened<Box<dyn RecvTransport>>,
    ) -> PyResult<Self> {
        let Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        // Taken before the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        let mut owned = Owned::new(
            inner,
            cancel.as_dyn(),
            Snapshot {
                domain,
                handles: (),
            },
        );
        if let Some(h) = end_reason {
            owned = owned.with_end_reason(h);
        }
        Ok(Self { owned, cancel })
    }
}

#[pymethods]
impl PyReceiver {
    /// Open a receiver from `url` (GIL released). For `srt://` the URL
    /// must carry `?mode=listener`; the first accept is not cancellable,
    /// since no handle exists until this returns.
    #[classmethod]
    fn open(_cls: &Bound<'_, PyType>, py: Python<'_>, url: &str) -> PyResult<Self> {
        Self::from_opened(py, crate::shells::open::open_recv(py, url)?)
    }

    /// Deprecated: use `open`.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "`Receiver.from_url` is deprecated; use `Receiver.open(url)`",
        )?;
        Self::from_opened(py, crate::shells::open::open_recv(py, url)?)
    }

    /// One 188-byte TS packet per call; `max_len` is accepted for API
    /// symmetry and floored at 188. Releases the GIL while parked;
    /// `close()` on another thread cancels first.
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let _cap = max_len.max(188);
        let pkt =
            crate::util::allow_threads_parking(py, || self.owned.with_mut(|r| r.next_packet()));
        let bytes = pyres(py, &self.owned.snapshot().domain, pkt)?;
        Ok(PyBytes::new_bound(py, &bytes).unbind())
    }

    /// Shareable cancel handle (shared state, see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |r| {
                    r.socket_stats().unwrap_or_default()
                })
        });
        Py::new(
            py,
            PySocketStats::from_core(pyok(py, &self.owned.snapshot().domain, core)?),
        )
    }

    /// Snapshot of the SRT-rich 17-field stats. Raises `TypeError` unless
    /// the shell was opened from an `srt://` URL.
    #[cfg(feature = "srt")]
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<crate::srt::transport::PySrtStats>> {
        let stats = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |r| {
                    r.transport()
                        .as_any()
                        .and_then(|a| a.downcast_ref::<tst_srt::SrtTransport>())
                        .map(|t| t.stats())
                })
        });
        match pyok(py, &self.owned.snapshot().domain, stats)? {
            Some(r) => {
                let stats = r.map_err(|e| {
                    raise(
                        py,
                        &self.owned.snapshot().domain,
                        tst_pipeline::binding::BindingError::from(e),
                    )
                })?;
                Py::new(py, crate::srt::transport::PySrtStats::from_srt(&stats))
            }
            None => Err(pyo3::exceptions::PyTypeError::new_err(
                "srt_stats() is only available on an srt:// shell",
            )),
        }
    }

    /// Why the receive session ended, as an `EndReason`, or `None` while
    /// it has not ended (or the transport records no end reason). Lock-free
    /// and readable after `close()`.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.owned.end_reason() {
            Some(r) => end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Free-text detail for `end_reason()`: the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`, else `None`.
    fn end_detail(&self) -> Option<String> {
        let reason = self.owned.end_reason()?;
        end_detail(&reason).map(str::to_owned)
    }

    /// Close: cancel first, so a `recv_bytes()` parked on another thread
    /// ends promptly with the transport's `CLOSED` kind, then close the
    /// transport. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)
    }

    /// `True` while the receiver owns a live transport (a parked
    /// `recv_bytes` on another thread counts as live; never waits).
    fn is_alive(&self) -> bool {
        self.owned.alive(|r| r.is_alive())
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        if self.owned.is_closed() {
            "Receiver(closed)".to_string()
        } else {
            "Receiver(open)".to_string()
        }
    }
}
