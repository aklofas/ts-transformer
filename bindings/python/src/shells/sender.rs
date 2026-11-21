//! `Sender`: TS bytes in, any transport out.
//!
//! Wraps `tst_pipeline::Sender<Box<dyn Transport>>`, opened from a URL by
//! `Sender.open(url)`:
//! - GIL released around connect / send / flush.
//! - The handle is a `tst_pipeline::binding::Owned`: the slot is taken only
//!   inside `with_mut` / `with_ref_blocking`, always under
//!   `py.detach`, and `close()` is `Owned::close` (cancel first, then
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
use tst_pipeline::binding::{BindingError, HandleState, Opened, Owned};
use tst_pipeline::{Sender as PlSender, SenderConfig};

use crate::generated::stats_shells::SocketStats;
use crate::raise::{pyok, pyres, raise};
use crate::shells::cancel::{PyCancelHandle, cancel_source};
use crate::shells::open_cancel::{PyOpenCancel, object_of};
use crate::util::{CancelSource, close_owned, drop_owned};

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

/// Whether a call on another thread holds the slot. Test-only.
#[cfg(feature = "py-test-hooks")]
impl PySender {
    pub(crate) fn slot_held(&self) -> bool {
        self.owned.is_held()
    }
}

impl Drop for PySender {
    fn drop(&mut self) {
        drop_owned(&self.owned.snapshot().domain, &self.owned);
    }
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
    ///
    /// `cancel`: an `OpenCancel` already cancelled refuses the open with
    /// the transport's `CLOSED` before the URL is parsed; one cancelled
    /// while the connect runs does not interrupt it, but the open then
    /// raises `CLOSED` and the connection is closed again.
    #[classmethod]
    #[pyo3(signature = (url, *, cancel = None))]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        url: &str,
        cancel: Option<&Bound<'_, PyOpenCancel>>,
    ) -> PyResult<Self> {
        let oc = object_of(cancel);
        let (opened, ()) = crate::shells::open::open_send(py, url, &oc, || Ok(()))?;
        Self::from_opened(py, opened)
    }

    /// Send one pre-muxed TS chunk (any bytes-like). Releases the GIL
    /// while the send blocks; a concurrent `close()` cancels first, so the
    /// call ends with the transport's `CLOSED` kind.
    fn send_bytes(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice: &[u8] = coerced.as_bytes();
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut_unless_cancelled(|s| s.send_ts(slice))
        });
        pyres(py, &self.owned.snapshot().domain, res)
    }

    /// Flush a partial 7-packet bundle (see `Sender::send_ts`). Releases
    /// the GIL. Call it before `close()` when the tail matters: `close()`
    /// cancels first and does not drain the framing buffer; `finish()`
    /// flushes and closes without a cancel.
    ///
    /// A stream of one or two TS packets is sent too. While the stream has
    /// been aligned since its first byte (no byte skipped, no packet
    /// boundary failed), the whole packets held are sent when every
    /// boundary present reads `0x47`, the first byte of a partial packet
    /// included. Once a byte was skipped or a boundary failed, `flush()`
    /// sends nothing more until three consecutive sync bytes have confirmed
    /// the alignment again; `finish()` sends or reports what it then holds
    /// back.
    ///
    /// A flush is the caller's word that what was sent so far is a stream,
    /// and it costs something when that is wrong. If the first 188 to 376
    /// bytes only look aligned (`0x47` at offset 0, and at offset 188 when
    /// more than 188 bytes are held) and are flushed before a third
    /// boundary could refute them, those 188 or 376 bytes are sent although
    /// they are not packets, and a real packet they cover (a stream joined
    /// mid-packet whose first byte happens to read `0x47`) is lost with
    /// them; without the flush the sender would have found the real
    /// packets and sent that one. The sender then stays on the first
    /// byte's grid, so a later `send_bytes()` or `flush()` can send one
    /// more 188 bytes that are not a packet before a boundary fails and it
    /// resyncs. This never happens without a flush (or `finish()`) inside
    /// the first 376 bytes, a flush with fewer than 188 bytes held sends
    /// nothing, and for random bytes it takes about one start in 256.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut_unless_cancelled(|s| s.flush())
        });
        pyres(py, &self.owned.snapshot().domain, res)
    }

    /// Shareable cancel handle (shared state, see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(
            py,
            PyCancelHandle::for_shell(&self.cancel, self.owned.cancel_arc()),
        )
    }

    /// Scheme-neutral 16-field wire stats. Waits (GIL released) for an
    /// in-flight `send_bytes` on another thread to release the slot.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<SocketStats>> {
        let core = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |s| {
                    s.socket_stats().unwrap_or_default()
                })
        });
        Py::new(
            py,
            SocketStats::from(&pyok(py, &self.owned.snapshot().domain, core)?),
        )
    }

    /// SRT-rich 17-field stats. Raises `TypeError` unless the shell was
    /// opened from an `srt://` URL.
    #[cfg(feature = "srt")]
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<crate::generated::stats_srt::SrtStats>> {
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
                let stats =
                    r.map_err(|e| raise(py, &self.owned.snapshot().domain, BindingError::from(e)))?;
                Py::new(py, crate::generated::stats_srt::SrtStats::from(&stats))
            }
            None => Err(pyo3::exceptions::PyTypeError::new_err(
                "srt_stats() is only available on an srt:// shell",
            )),
        }
    }

    /// Close: cancel first (a parked `send_bytes` on another thread ends
    /// promptly), then take the slot and close the transport. A partial
    /// bundle in the framing buffer is not delivered: `flush()` first, or
    /// `finish()`, when the tail matters. Idempotent; later calls raise
    /// `CLOSED`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)
    }

    /// End the stream, then close: send what `flush()` sends, then make one
    /// end-of-stream pass over what the sender still holds, so packets
    /// behind damage that three consecutive sync bytes verify are sent now
    /// (no later call would). Closes the transport without cancelling
    /// first: the lossless end, also over a transport that queues (an RTSP
    /// push writes its queue, then TEARDOWN). The sender is closed either
    /// way; a later `finish()` or `close()` does nothing, and a `finish()`
    /// after `close()` is quiet.
    ///
    /// Raises the first send error (the transport's error class); what was
    /// not yet sent is then abandoned. Without one, raises the transport's
    /// `INPUT_MALFORMED` after sending everything it could, when bytes are
    /// left that could not be sent as whole aligned TS packets: a partial
    /// last packet, fewer than three packets behind skipped bytes, bytes
    /// that are not TS. Bytes the sender skipped on the way to finding its
    /// alignment, a good packet in front of early damage among them, are
    /// not part of that report.
    ///
    /// It does not cancel: a `send_bytes` parked on another thread keeps
    /// the shell and `finish()` waits behind it; `close()` instead if that
    /// is not wanted. After `cancel_handle().cancel()` it raises `CLOSED`,
    /// buffered data included.
    fn finish(&self, py: Python<'_>) -> PyResult<()> {
        // lossless shutdown: drains through the transport, documented to wait behind a send in flight
        match crate::util::allow_threads_parking(py, || {
            self.owned.with_mut_unless_cancelled(|s| s.finish())
        }) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(e),
            )),
            // Already closed: the slot is empty, the answer a second
            // `finish()` gets from the shell itself.
            Err(HandleState::Closed) => Ok(()),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
        }
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
