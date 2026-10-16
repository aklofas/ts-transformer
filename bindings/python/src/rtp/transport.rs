//! The raw RTP transport classes (`Transport` / `RecvTransport`, the same
//! grammar as `tstrans.udp`). `CancelHandle` and `SocketStats` are the
//! `tstrans.shells` classes.
//!
//! PyO3 wrappers for `tst_rtp::RtpTransport` (send) and
//! `tst_rtp::RtpRecvTransport` (recv).  Each PyClass wraps a single
//! concrete transport — NOT generic over `T: Transport` — matching
//! tst-c (handles concrete per-transport).
//!
//! GIL boundaries:
//! - `send`, `recv` → wrapped in `py.allow_threads(|| ...)` so concurrent
//!   Python threads can keep working while UDP I/O blocks on the kernel.
//! - `stats` → also releases the GIL: it waits for the slot a parked
//!   `recv` / in-flight `send` on another thread holds.
//! - `cancel_handle`, `cancel`, `end_reason`, `__enter__`, `__exit__` →
//!   fast read-only / atomic operations; no GIL release.
//! - Concurrency:
//!   Every wrapper holds a `tst_pipeline::binding::Owned`, which takes
//!   the slot only inside `with_mut` / `with_ref` (GIL released) and
//!   makes `close()` cancel-first.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::time::Duration;
use tst_pipeline::binding::view::BlockingReason;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::EndReasonHandle;
use tst_core::transport::{RecvTransport, Transport, TransportCancel, TransportError};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned, SendHalf};
use tst_rtp::builder::RtpRecvSocketBuilder;
use tst_rtp::{RtpRecvTransport, RtpSocketBuilder, RtpTransport};

use crate::raise::{RTP, pyok, pyres, raise};
use crate::shells::cancel::{PyCancelHandle, PySocketStats};
use crate::util::{CancelSource, close_owned};

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Every live `RtpTransport` / `RtpRecvTransport` has a cancel handle;
/// the trait accessor is nonetheless an `Option`, so this converts the
/// `None` that cannot happen into a loud raise instead of `.expect`.
/// `Internal` is not an `RtpErrorKind` member, so `raise` surfaces it as
/// a bare `RuntimeError` (the forward-compat rule) — deliberate:
/// it is a binding bug, not an RTP condition.
pub(crate) fn rtp_cancel_source(
    py: Python<'_>,
    handle: Option<Arc<dyn TransportCancel + Send + Sync>>,
) -> PyResult<Arc<CancelSource>> {
    let handle = handle.ok_or_else(|| {
        raise(
            py,
            &RTP,
            BindingError::new(
                BindingErrorKind::Internal,
                "RTP transport returned no cancel handle",
            ),
        )
    })?;
    Ok(CancelSource::new(handle))
}

/// The RTP receive transport of an RTSP session or publisher mount as an
/// opened shell transport; its own cancel handle is taken before it is
/// boxed (`FlagCancel` when it has none), the way `tst_rtp::opener` does.
pub(crate) fn opened_recv(
    transport: tst_rtp::RtpRecvTransport,
) -> tst_pipeline::binding::Opened<Box<dyn tst_core::transport::RecvTransport>> {
    let cancel: std::sync::Arc<dyn tst_core::transport::TransportCancel> =
        match tst_core::transport::RecvTransport::cancel_handle(&transport) {
            Some(c) => c,
            None => std::sync::Arc::new(tst_core::cancel::FlagCancel::new()),
        };
    tst_pipeline::binding::Opened::new(
        Box::new(transport),
        cancel,
        tst_pipeline::binding::Domain::Rtp,
    )
}

// ---------------------------------------------------------------------------
// PyRtpTransport — wraps tst_rtp::RtpTransport
// ---------------------------------------------------------------------------

/// Python RTP sender — wraps `tst_rtp::RtpTransport`.
///
/// Constructed from an `rtp://host:port` URL plus optional `pkt_size`
/// (188-multiple, default 1316) and `ssrc` (random when omitted)
/// keyword arguments. Other URL query parameters (`ttl=`, `iface=`)
/// can be embedded in the URL itself.
#[pyclass(name = "Transport", module = "tstrans.rtp")]
pub(crate) struct PyRtpTransport {
    /// The binding layer's handle state machine: a `send` in flight
    /// on another thread holds the slot only inside `with_mut`, under
    /// `py.allow_threads`; `close()` (cancel-first) ends it instead of
    /// waiting behind it or tripping PyO3's borrow check.
    owned: Owned<SendHalf<RtpTransport>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

#[pymethods]
impl PyRtpTransport {
    /// Construct a sender connected to `url` (e.g.
    /// `"rtp://127.0.0.1:5004"`).
    ///
    /// `pkt_size` overrides the UDP datagram size (RTP header + TS
    /// payload). `ssrc` pins the RTP synchronization source identifier;
    /// when omitted the transport picks a random one.
    #[new]
    #[pyo3(signature = (url, *, pkt_size = 1316, ssrc = None))]
    fn new(py: Python<'_>, url: &str, pkt_size: usize, ssrc: Option<u32>) -> PyResult<Self> {
        let mut builder = RtpSocketBuilder::from_url(url)
            .map_err(|e| raise(py, &RTP, BindingError::from(tst_rtp::ConnectError::from(e))))?;
        builder.pkt_size(pkt_size);
        if let Some(s) = ssrc {
            builder.ssrc(s);
        }
        let inner = builder
            .build()
            .map_err(|e| raise(py, &RTP, BindingError::from(e)))?;
        // The Arc returned is the same one the transport's send-loop
        // holds — flipping it here wakes a parked send on the next
        // 100 ms cancel-poll tick.
        let cancel = rtp_cancel_source(py, Transport::cancel_handle(&inner))?;
        Ok(Self {
            owned: Owned::new(SendHalf(inner), cancel.as_dyn(), ()),
            cancel,
        })
    }

    /// Send one MPEG-TS payload chunk over RTP. Accepts any bytes-like
    /// input: `bytes`, `bytearray`, `memoryview` (over either), and
    /// any object implementing the buffer protocol.
    ///
    /// Releases the GIL during the underlying `sendto` call so other
    /// Python threads can run while this thread blocks on the kernel.
    fn send(&self, py: Python<'_>, ts_bytes: &Bound<'_, PyAny>) -> PyResult<()> {
        // Zero-copy for real `bytes`; one C copy through `bytes()` for
        // bytearray / memoryview / numpy (PyBuffer is unavailable under
        // abi3-py310).
        let coerced = crate::util::coerce_bytes_like(py, ts_bytes)?;
        let slice: &[u8] = coerced.as_bytes();
        pyres(
            py,
            &RTP,
            crate::util::allow_threads_parking(py, || {
                self.owned.with_mut(|t| t.0.send_bytes(slice))
            }),
        )
    }

    /// Snapshot of wire-level statistics. Returns a frozen `SocketStats`
    /// dataclass; the `bytes_sent` / `packets_sent` counters tick on
    /// each successful `.send()`.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core_stats = pyok(
            py,
            &RTP,
            crate::util::allow_threads_parking(py, || {
                self.owned
                    // counters live inside the shell and have no value to report while a parked call holds it
                    .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |t| {
                        t.0.socket_stats().unwrap_or_default()
                    })
            }),
        )?;
        Py::new(py, PySocketStats::from_core(core_stats))
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `.send()`;
    /// that call returns `RtpError(kind=CLOSED)`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Close the sender. Fires the cancel handle BEFORE taking the slot
    /// (a `send()` in flight on another thread ends with
    /// `RtpError(CLOSED)`), then drops the transport. After close,
    /// further `.send()` calls raise `RtpError(kind=CLOSED)`. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &RTP, &self.owned)
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
        Ok(false) // do not suppress exceptions
    }

    fn __repr__(&self) -> String {
        if self.owned.is_closed() {
            "Transport(closed)".to_string()
        } else {
            "Transport(open)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyRtpRecvTransport — wraps tst_rtp::RtpRecvTransport
// ---------------------------------------------------------------------------

/// Transport + reusable scratch buffer, kept under one lock so a `&self`
/// `recv` can fill the scratch without a second borrow.
pub(crate) struct RtpRecvInner {
    transport: RtpRecvTransport,
    /// Sized to the transport's `max_payload()` at construction. Reused
    /// across calls to avoid a per-recv malloc.
    scratch: Vec<u8>,
}

impl tst_pipeline::binding::Close for RtpRecvInner {
    type Error = core::convert::Infallible;

    fn close(&mut self) -> Result<(), Self::Error> {
        RecvTransport::close(&mut self.transport);
        Ok(())
    }
}

/// Python RTP receiver — wraps `tst_rtp::RtpRecvTransport`.
///
/// Binds to `url` (literal IP:port). For multicast URLs, joins the
/// group automatically. The receive buffer sizes itself to the
/// transport's deliverable ceiling; `?pkt_size=` on a receiver URL is
/// rejected. The 12-byte RTP header is stripped internally so `.recv()`
/// returns just the TS payload bytes.
#[pyclass(name = "RecvTransport", module = "tstrans.rtp")]
pub(crate) struct PyRtpRecvTransport {
    /// The binding layer's handle state machine: a parked `recv`
    /// holds the slot only inside `with_mut`, under `py.allow_threads`;
    /// `close()` (cancel-first) ends it instead of waiting behind it.
    owned: Owned<RtpRecvInner>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// Handle onto the transport's [`EndReasonHandle`], pulled at
    /// construction — before the slot is ever emptied by `close()`, so
    /// `end_reason()` / `end_detail()` keep working after close (`close()`
    /// records `Cancelled` on the transport before dropping it, and this
    /// handle shares the same underlying cell). Lock-free: never touches
    /// the slot, so a watchdog can read it while a recv is parked.
    end_reason: EndReasonHandle,
}

#[pymethods]
impl PyRtpRecvTransport {
    /// Bind a receiver to `url` (e.g. `"rtp://127.0.0.1:5004"` for
    /// unicast or `"rtp://239.0.0.1:5004"` for multicast).
    ///
    /// The receive buffer sizes itself to the transport's deliverable
    /// ceiling; `?pkt_size=` on a receiver URL is rejected.
    #[new]
    fn new(py: Python<'_>, url: &str) -> PyResult<Self> {
        let builder = RtpRecvSocketBuilder::from_url(url)
            .map_err(|e| raise(py, &RTP, BindingError::from(tst_rtp::ConnectError::from(e))))?;
        let transport = builder
            .build()
            .map_err(|e| raise(py, &RTP, BindingError::from(e)))?;
        let scratch_len = transport.max_payload();
        // Both pulled BEFORE the transport moves into the slot.
        let cancel = rtp_cancel_source(py, RecvTransport::cancel_handle(&transport))?;
        let end_reason = transport.end_reason_handle();
        Ok(Self {
            owned: Owned::new(
                RtpRecvInner {
                    transport,
                    scratch: vec![0u8; scratch_len],
                },
                cancel.as_dyn(),
                (),
            ),
            cancel,
            end_reason,
        })
    }

    /// Receive one MPEG-TS payload chunk. Blocks until a packet arrives
    /// (releases the GIL while parked) or the cancel handle fires.
    ///
    /// `timeout_ms=None` (the default) blocks indefinitely. `timeout_ms=N`
    /// bounds this single call to `N` milliseconds via the one-shot
    /// `RtpRecvTransport::recv_timeout`; on expiry it raises
    /// `RtpError(BACKPRESSURE)` and the receiver stays open — call again to
    /// keep waiting. The one-shot's `Ok(None)` expiry never leaks to
    /// Python: `.recv()` always either returns `bytes` or raises.
    ///
    /// Returns a fresh `bytes` object containing the TS bundle (RTP
    /// header already stripped).
    #[pyo3(signature = (timeout_ms = None))]
    fn recv(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<Py<PyBytes>> {
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut(|s| {
                let n = match timeout_ms {
                    None => s.transport.recv_bytes(&mut s.scratch).map(Some),
                    Some(ms) => s
                        .transport
                        .recv_timeout(&mut s.scratch, Duration::from_millis(ms)),
                };
                // Copy out under the lock; the PyBytes is built once the GIL
                // is back.
                n.map(|n| n.map(|n| s.scratch[..n].to_vec()))
            })
        });
        // The peer-EOS rule lives on the pipeline SHELL impls; this
        // class holds a raw `RtpRecvTransport`, so `From<TransportError>`
        // would flatten a peer EOS to `CLOSED`. Apply the shell rule here
        // instead, so `END_OF_STREAM` is reachable and a caller can tell a
        // clean peer end (an RTSP teardown on a session-derived receiver)
        // from their own `close()`.
        //
        // GATED on the recorded end reason: `recv_bytes` also answers
        // `Closed` when the pump's source is gone after a wire break
        // (tst-rtp transport.rs:1086/1118/1122), and calling that "peer
        // ended the stream" would be a lie. Only a recorded
        // `CleanTeardown` is a peer EOS; everything else keeps `CLOSED`.
        let clean_eos = matches!(
            self.end_reason.get(),
            Some(tst_core::transport::EndReason::CleanTeardown)
        );
        let res = res.map(|r| {
            r.map_err(|e| match e {
                TransportError::Closed if clean_eos => {
                    BindingError::new(BindingErrorKind::EndOfStream, "peer ended the stream")
                }
                other => BindingError::from(other),
            })
        });
        match pyres(py, &RTP, res)? {
            Some(bytes) => Ok(PyBytes::new_bound(py, &bytes).unbind()),
            // One-shot deadline expiry (`recv_timeout` -> Ok(None)): the
            // transport is alive — BACKPRESSURE, retryable. Same member the
            // JVM raises, so the cross-binding golden sees ONE kind.
            None => Err(raise(
                py,
                &RTP,
                BindingError::new(BindingErrorKind::Backpressure, "recv deadline elapsed"),
            )),
        }
    }

    /// Snapshot of wire-level statistics. Returns a frozen `SocketStats`
    /// dataclass; the `bytes_received` / `packets_received` counters
    /// tick on each successful `.recv()`. Waits (GIL released) for a
    /// recv parked on another thread.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core_stats = pyok(
            py,
            &RTP,
            crate::util::allow_threads_parking(py, || {
                self.owned
                    // counters live inside the shell and have no value to report while a parked call holds it
                    .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |s| {
                        s.transport.socket_stats().unwrap_or_default()
                    })
            }),
        )?;
        Py::new(py, PySocketStats::from_core(core_stats))
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `.recv()`;
    /// that call returns `RtpError(kind=CLOSED)`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Why the receive session ended, or `None` if it hasn't ended yet
    /// (or ended through a path this type doesn't instrument). Still
    /// readable after `close()` — `end_reason` is a
    /// [`EndReasonHandle`] captured at construction, independent of
    /// the slot's lifetime.
    ///
    /// `EndReasonHandle::get` is a lock-free `Arc<OnceLock<_>>`
    /// read — no blocking, so no `py.allow_threads` is needed here even
    /// though this touches Rust-owned shared state.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.end_reason.get() {
            Some(r) => crate::shells::end_reason::end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Free-text detail for `end_reason()` — the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`; `None`
    /// for every other reason (including "hasn't ended yet").
    fn end_detail(&self) -> Option<String> {
        let reason = self.end_reason.get()?;
        crate::shells::end_reason::end_detail(&reason).map(str::to_owned)
    }

    /// Close the receiver. Fires the cancel handle BEFORE taking the slot
    /// (a `recv()` parked on another thread ends with
    /// `RtpError(CLOSED)`), then drops the transport. After close,
    /// further `.recv()` calls raise `RtpError(kind=CLOSED)`. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &RTP, &self.owned)
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
        if !self.owned.is_closed() {
            "RecvTransport(open)".to_string()
        } else {
            "RecvTransport(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRtpTransport>()?;
    m.add_class::<PyRtpRecvTransport>()?;
    Ok(())
}
