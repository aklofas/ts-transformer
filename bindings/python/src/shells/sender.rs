//! `Sender`: TS bytes in, any transport out.
//!
//! Wraps `tst_pipeline::Sender<Box<dyn Transport>>`, opened from a URL by
//! `Sender.open(url)`:
//! - GIL released around connect / send / flush.
//! - The handle is a `tst_pipeline::binding::Owned`: the slot is taken only
//!   inside `with_mut` / `with_ref_blocking`, always under
//!   `py.allow_threads`, and `close()` is `Owned::close` (cancel first, then
//!   take, then close). A call parked on another thread therefore ends
//!   promptly instead of blocking the close or tripping PyO3's borrow check.
//! - The transport's error domain rides in the snapshot, which stays
//!   readable after `close()`: a closed `rtp://` shell raises `RtpError`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyType;

use tst_core::transport::Transport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{Opened, Owned};
use tst_pipeline::{Sender as PlSender, SenderConfig};

#[cfg(feature = "srt")]
use crate::raise::raise;
use crate::raise::{pyok, pyres};
use crate::shells::cancel::{PyCancelHandle, PySocketStats, cancel_source};
use crate::util::{CancelSource, close_owned};

/// Python sender over any transport — wraps
/// `tst_pipeline::Sender<Box<dyn Transport>>`.
///
/// Opened with `Sender.open("udp://host:port")` (or `srt://`, `rtp://`,
/// `tcp://`, `tcps://`, `rist://`); per-transport options are URL query
/// keys.
#[pyclass(name = "Sender", module = "tstrans.shells")]
pub(crate) struct PySender {
    /// The binding layer's handle state machine; see the module doc.
    owned: Owned<PlSender<Box<dyn Transport>>, Snapshot<()>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

impl PySender {
    /// Wrap an already-opened transport (the `open` and
    /// `Socket.into_sender()` paths meet here).
    pub(crate) fn from_opened(
        _py: Python<'_>,
        opened: Opened<Box<dyn Transport>>,
    ) -> PyResult<Self> {
        let Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        let inner = PlSender::new(transport, SenderConfig::default());
        Ok(Self {
            owned: Owned::new(
                inner,
                cancel.as_dyn(),
                Snapshot {
                    domain,
                    handles: (),
                },
            ),
            cancel,
        })
    }
}

#[pymethods]
impl PySender {
    /// Open a sender from `url`. Releases the GIL during the connect.
    /// Raises `ValueError` for an unsupported scheme and the transport's
    /// error class for a bad URL or a connect failure.
    #[classmethod]
    fn open(_cls: &Bound<'_, PyType>, py: Python<'_>, url: &str) -> PyResult<Self> {
        Self::from_opened(py, crate::shells::open::open_send(py, url)?)
    }

    /// Deprecated: use `open`.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "`Sender.from_url` is deprecated; use `Sender.open(url)`",
        )?;
        Self::from_opened(py, crate::shells::open::open_send(py, url)?)
    }

    /// Send one pre-muxed TS chunk (any bytes-like). Releases the GIL
    /// while the send blocks; a concurrent `close()` cancels first, so the
    /// call ends with the transport's `CLOSED` kind.
    fn send_bytes(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice: &[u8] = coerced.as_bytes();
        let res =
            crate::util::allow_threads_parking(py, || self.owned.with_mut(|s| s.send_ts(slice)));
        pyres(py, &self.owned.snapshot().domain, res)
    }

    /// Flush a partial 7-packet bundle (see `Sender::send_ts`). Releases
    /// the GIL. Call it before `close()` when the tail matters: `close()`
    /// cancels first and does not drain the framing buffer.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        let res = crate::util::allow_threads_parking(py, || self.owned.with_mut(|s| s.flush()));
        pyres(py, &self.owned.snapshot().domain, res)
    }

    /// Shareable cancel handle (shared state, see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Scheme-neutral 16-field wire stats. Waits (GIL released) for an
    /// in-flight `send_bytes` on another thread to release the slot.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |s| {
                    s.socket_stats().unwrap_or_default()
                })
        });
        Py::new(
            py,
            PySocketStats::from_core(pyok(py, &self.owned.snapshot().domain, core)?),
        )
    }

    /// SRT-rich 17-field stats. Raises `TypeError` unless the shell was
    /// opened from an `srt://` URL.
    #[cfg(feature = "srt")]
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<crate::srt::transport::PySrtStats>> {
        let stats = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |s| {
                    s.transport()
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

    /// Close: cancel first (a parked `send_bytes` on another thread ends
    /// promptly), then take the slot and close the transport. A partial
    /// bundle in the framing buffer is not delivered: `flush()` first
    /// when the tail matters. Idempotent; later calls raise `CLOSED`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)
    }

    /// `True` while the sender owns a live transport (a send in flight on
    /// another thread counts as live; the probe never waits).
    fn is_alive(&self) -> bool {
        self.owned.alive(|s| s.is_alive())
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
            "Sender(closed)".to_string()
        } else {
            "Sender(open)".to_string()
        }
    }
}
