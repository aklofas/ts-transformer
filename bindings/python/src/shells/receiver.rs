//! `Receiver`: any transport in, TS bytes out.
//!
//! Wraps `tst_pipeline::Receiver<Box<dyn RecvTransport>>`, opened from a URL
//! by `Receiver.open(url)`. Same handle model as `Sender`: an `Owned` slot
//! taken only under `py.detach`, cancel-first `close()`, and the
//! transport's error domain in the snapshot. The transport's end-reason
//! handle is attached to the `Owned`, so `end_reason()` keeps answering
//! after `close()`; so do `peer_addr()` and `source_rejected()`, which read
//! the transport's source record from the snapshot, and `rtp_stats()`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyType};

use tst_core::transport::{RecvTransport, SourceHandle};
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, HandleState, Opened, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig};

use crate::generated::stats_shells::{ReceiverStats, SocketStats};
use crate::raise::{pyok, pyres, raise};
use crate::shells::cancel::{PyCancelHandle, cancel_source};
use crate::shells::end_reason::{end_detail, end_reason_to_py};
use crate::shells::open::{OpenedRecv, RtpStatsSource};
use crate::shells::open_cancel::{PyOpenCancel, object_of};
use crate::util::{CancelSource, close_owned, drop_owned};

/// Python receiver over any transport — wraps
/// `tst_pipeline::Receiver<Box<dyn RecvTransport>>`.
///
/// `Receiver.open("srt://:7000?mode=listener")` binds, listens and accepts
/// ONE peer; the datagram and stream transports open their socket.
#[pyclass(name = "Receiver", module = "tstrans.shells")]
pub(crate) struct PyReceiver {
    owned: Owned<PlReceiver<Box<dyn RecvTransport>>, Snapshot<Handles>>,
    cancel: Arc<CancelSource>,
}

/// The shell's construction-time handles, read without the slot.
struct Handles {
    /// The transport's source record; `None` for a transport that keeps none.
    source: Option<SourceHandle>,
    /// The RTP counters; `None` off RTP.
    #[cfg_attr(not(feature = "rtp"), allow(dead_code))]
    rtp_stats: RtpStatsSource,
    /// The address the open bound; `None` for a transport whose opener
    /// records none.
    local_addr: Option<std::net::SocketAddr>,
}

impl Drop for PyReceiver {
    fn drop(&mut self) {
        drop_owned(&self.owned.snapshot().domain, &self.owned);
    }
}

impl PyReceiver {
    /// The address the open bound, or the transport's `CLOSED` once closed.
    fn bound(&self, py: Python<'_>) -> PyResult<Option<std::net::SocketAddr>> {
        if self.owned.is_closed() {
            Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(HandleState::Closed),
            ))
        } else {
            Ok(self.owned.snapshot().handles.local_addr)
        }
    }

    /// Wrap an already-opened transport (the `open` and
    /// `Socket.into_receiver()` paths meet here).
    pub(crate) fn from_opened(
        py: Python<'_>,
        opened: Opened<Box<dyn RecvTransport>>,
    ) -> PyResult<Self> {
        Self::from_recv(py, OpenedRecv::plain(opened))
    }

    /// [`Self::from_opened`] with the RTP counters of an `rtp://` transport.
    fn from_recv(_py: Python<'_>, opened: OpenedRecv) -> PyResult<Self> {
        let OpenedRecv { opened, rtp_stats } = opened;
        let Opened {
            transport,
            cancel,
            domain,
            local_addr,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        // Taken before the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let source = transport.source_handle();
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        let mut owned = Owned::new(
            inner,
            cancel.as_dyn(),
            Snapshot {
                domain,
                handles: Handles {
                    source,
                    rtp_stats,
                    local_addr,
                },
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
    /// must carry `?mode=listener`, and the open blocks in its first accept
    /// until a peer connects. An `srt://` bind or accept fault raises
    /// `SrtError(kind=BROKEN)` with the message prefixed `bind: ` or
    /// `accept: `.
    ///
    /// `cancel`: no handle exists until this returns, so the first accept
    /// is cancelled through an `OpenCancel` made beforehand: its
    /// `cancel()` from another thread ends the accept with
    /// `SrtError(CLOSED)`, and an object already cancelled refuses the
    /// open with the transport's `CLOSED` before the URL is parsed.
    #[classmethod]
    #[pyo3(signature = (url, *, cancel = None))]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        url: &str,
        cancel: Option<&Bound<'_, PyOpenCancel>>,
    ) -> PyResult<Self> {
        let oc = object_of(cancel);
        Self::from_recv(py, crate::shells::open::open_recv(py, url, &oc)?)
    }

    /// One 188-byte TS packet per call; `max_len` is accepted for API
    /// symmetry and floored at 188. Releases the GIL while parked;
    /// `close()` on another thread cancels first.
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let _cap = max_len.max(188);
        let pkt = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut_unless_cancelled(|r| r.next_packet())
        });
        let bytes = pyres(py, &self.owned.snapshot().domain, pkt)?;
        Ok(PyBytes::new(py, &bytes).unbind())
    }

    /// Shareable cancel handle (shared state, see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(
            py,
            PyCancelHandle::for_shell(&self.cancel, self.owned.cancel_arc()),
        )
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<SocketStats>> {
        let core = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |r| {
                    r.socket_stats().unwrap_or_default()
                })
        });
        Py::new(
            py,
            SocketStats::from(&pyok(py, &self.owned.snapshot().domain, core)?),
        )
    }

    /// The receiver's counters (`ReceiverStats`): the packets handed out
    /// and what sync recovery skipped to find them. Waits behind a parked
    /// `recv_bytes()`, as `socket_stats()` does; raises the transport's
    /// `CLOSED` kind after `close()`.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<ReceiverStats>> {
        let raw = crate::util::allow_threads_parking(py, || {
            self.owned
                // counters live inside the shell and have no value to report while a parked call holds it
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |r| r.stats())
        });
        Py::new(
            py,
            ReceiverStats::from(&pyok(py, &self.owned.snapshot().domain, raw)?),
        )
    }

    /// The RTP counters (`tstrans.rtp.RtpStats`) of a receiver over
    /// `rtp://`; `None` on any other transport. Never waits behind a parked
    /// `recv_bytes()`; the last value after `close()`.
    #[cfg(feature = "rtp")]
    fn rtp_stats(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<crate::generated::stats_rtp::RtpStats>>> {
        crate::shells::demux_receiver::rtp_stats_of(py, &self.owned.snapshot().handles.rtp_stats)
    }

    /// Snapshot of the SRT-rich 17-field stats. Raises `TypeError` unless
    /// the shell was opened from an `srt://` URL.
    #[cfg(feature = "srt")]
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<crate::generated::stats_srt::SrtStats>> {
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
                Py::new(py, crate::generated::stats_srt::SrtStats::from(&stats))
            }
            None => Err(pyo3::exceptions::PyTypeError::new_err(
                "srt_stats() is only available on an srt:// shell",
            )),
        }
    }

    /// Why the receive session ended, as an `EndReason`, or `None` while
    /// it has not ended (or the transport records no end reason). Lock-free
    /// and readable after `close()`.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
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

    /// `(host, port)` the open bound: a `udp://`, `rist://@` or `rtp://`
    /// bind (the kernel's port when the URL named 0), or an `srt://`
    /// listener once its caller connected; `None` for a transport whose
    /// opener records none. Never waits behind a parked `recv_bytes()`; raises
    /// the transport's `CLOSED` kind once the receiver is closed.
    fn local_addr(&self, py: Python<'_>) -> PyResult<Option<crate::util::PyAddr>> {
        Ok(self.bound(py)?.map(crate::util::addr_tuple))
    }

    /// The port of `local_addr()`, or `None` where that is `None`. Same
    /// rules as `local_addr()`.
    fn local_port(&self, py: Python<'_>) -> PyResult<Option<u16>> {
        Ok(self.bound(py)?.map(|a| a.port()))
    }

    /// `(host, port)` of the sender whose datagram (`udp://`) or block
    /// (`rist://`) was delivered last; `None` before the first one and for
    /// a transport that keeps no source record. Never waits behind a
    /// parked `recv_bytes()`; the last value after `close()`.
    fn peer_addr(&self) -> Option<crate::util::PyAddr> {
        crate::util::peer_addr_tuple(self.owned.snapshot().handles.source.as_ref())
    }

    /// Datagrams or blocks the `source` key of a `udp://` or `rist://` URL
    /// turned away; 0 for a transport that keeps no source record. Never
    /// waits behind a parked `recv_bytes()`; the last value after `close()`.
    fn source_rejected(&self) -> u64 {
        self.owned
            .snapshot()
            .handles
            .source
            .as_ref()
            .map_or(0, SourceHandle::rejected)
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
