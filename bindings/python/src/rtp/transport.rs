//! `Sender`, `Receiver`, `SocketStats`, `CancelHandle` for RTP.
//!
//! PyO3 wrappers for `tst_rtp::RtpTransport` (send) and
//! `tst_rtp::RtpRecvTransport` (recv).  Each PyClass wraps a single
//! concrete transport — NOT generic over `T: Transport` — matching the
//! Stage 1 tst-c lesson #1 (handles concrete per-transport).
//!
//! GIL boundaries (per `docs/specs/2026-05-26-tst-rtp-phase-4-binding-exposure-design.md`):
//! - `send`, `recv` → wrapped in `py.allow_threads(|| ...)` so concurrent
//!   Python threads can keep working while UDP I/O blocks on the kernel.
//! - `stats` → also releases the GIL: it waits for the slot a parked
//!   `recv` / in-flight `send` on another thread holds.
//! - `cancel_handle`, `cancel`, `end_reason`, `__enter__`, `__exit__` →
//!   fast read-only / atomic operations; no GIL release.
//! - Concurrency (Arc 2):
//!   Every wrapper holds a `tst_pipeline::binding::Owned`, which takes
//!   the slot only inside `with_mut` / `with_ref` (GIL released) and
//!   makes `close()` cancel-first.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::time::Duration;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::{RecvTransport, SocketStats, Transport, TransportCancel, TransportError};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned, SendHalf};
use tst_rtp::builder::RtpRecvSocketBuilder;
use tst_rtp::{RtpRecvTransport, RtpSocketBuilder, RtpTransport, StreamEndReasonHandle};

use crate::raise::{RTP, pyok, pyres, raise};
use crate::util::{CancelSource, close_owned};

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Every live `RtpTransport` / `RtpRecvTransport` has a cancel handle;
/// the trait accessor is nonetheless an `Option`, so this converts the
/// `None` that cannot happen into a loud `Internal` instead of `.expect`.
pub(crate) fn rtp_cancel_source(
    py: Python<'_>,
    handle: Option<Arc<dyn TransportCancel + Send + Sync>>,
) -> PyResult<Arc<CancelSource>> {
    let handle = handle.ok_or_else(|| {
        raise(
            py,
            &RTP,
            BindingError {
                kind: BindingErrorKind::Internal,
                detail: "RTP transport returned no cancel handle".into(),
            },
        )
    })?;
    Ok(CancelSource::new(handle))
}

// ---------------------------------------------------------------------------
// PySocketStats — frozen mirror of tst_core::transport::SocketStats
// ---------------------------------------------------------------------------

/// Mirror of `tst_core::transport::SocketStats` exposed to Python as
/// a frozen, get_all-decorated PyClass. Fields match the Rust struct
/// 1:1 — `RtpTransport` populates `bytes_sent` / `packets_sent` only
/// in Phase 1; `RtpRecvTransport` populates the receive-side counters.
/// The RTCP-derived fields (`rtt_us`, `packets_lost_*`) stay zero
/// until RTCP RR/SR ingest is wired.
#[pyclass(frozen, get_all, name = "SocketStats", module = "tstrans.rtp")]
pub(crate) struct PySocketStats {
    pub rtt_us: u32,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub link_bandwidth_bps: u64,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub bytes_lost_recv: u64,
    pub packets_lost_recv: u64,
    pub packets_lost_send: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_send: u64,
    pub packets_dropped_recv: u64,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
}

impl PySocketStats {
    pub(crate) fn from_core(s: SocketStats) -> Self {
        Self {
            rtt_us: s.rtt_us,
            send_bandwidth_bps: s.send_bandwidth_bps,
            recv_bandwidth_bps: s.recv_bandwidth_bps,
            link_bandwidth_bps: s.link_bandwidth_bps,
            bytes_sent: s.bytes_sent,
            packets_sent: s.packets_sent,
            bytes_received: s.bytes_received,
            packets_received: s.packets_received,
            bytes_lost_recv: s.bytes_lost_recv,
            packets_lost_recv: s.packets_lost_recv,
            packets_lost_send: s.packets_lost_send,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped_send: s.packets_dropped_send,
            packets_dropped_recv: s.packets_dropped_recv,
            send_buffer_packets: s.send_buffer_packets,
            recv_buffer_packets: s.recv_buffer_packets,
        }
    }
}

#[pymethods]
impl PySocketStats {
    fn __repr__(&self) -> String {
        format!(
            "SocketStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={})",
            self.bytes_sent,
            self.packets_sent,
            self.bytes_received,
            self.packets_received,
            self.rtt_us,
        )
    }
}

// ---------------------------------------------------------------------------
// PyCancelHandle — Arc-shared so multiple Python refs share one target
// ---------------------------------------------------------------------------

/// Python-side cancel handle. Wraps the shell's shared
/// [`crate::util::CancelSource`]: every clone obtained from the same
/// shell — and the shell's own `close()` — forwards into one
/// `Arc<dyn TransportCancel>` and flips one flag, so `is_cancelled()`
/// reports the shell's state, not this wrapper's history (Arc 2).
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.rtp")]
pub(crate) struct PyCancelHandle {
    src: Arc<crate::util::CancelSource>,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent — repeated calls are a no-op.
    /// Wakes a thread parked in `Sender.send` / `Receiver.recv` /
    /// `H264Receiver.recv_au` at the next 100 ms cancel-poll tick; that
    /// call raises `RtpError(CLOSED)` (detail "cancelled from another
    /// thread").
    fn cancel(&self) {
        tst_core::transport::TransportCancel::cancel(&*self.src);
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
    /// The single constructor every rtp class uses.
    pub(crate) fn from_source(src: &Arc<crate::util::CancelSource>) -> Self {
        Self {
            src: Arc::clone(src),
        }
    }
}

// ---------------------------------------------------------------------------
// PySender — wraps tst_rtp::RtpTransport
// ---------------------------------------------------------------------------

/// Python RTP sender — wraps `tst_rtp::RtpTransport`.
///
/// Constructed from an `rtp://host:port` URL plus optional `pkt_size`
/// (188-multiple, default 1316) and `ssrc` (random when omitted)
/// keyword arguments. Other URL query parameters (`ttl=`, `iface=`)
/// can be embedded in the URL itself.
#[pyclass(name = "Sender", module = "tstrans.rtp")]
pub(crate) struct PySender {
    /// The binding layer's handle state machine (Arc 2): a `send` in flight
    /// on another thread holds the slot only inside `with_mut`, under
    /// `py.allow_threads`; `close()` (cancel-first) ends it instead of
    /// waiting behind it or tripping PyO3's borrow check.
    owned: Owned<SendHalf<RtpTransport>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

#[pymethods]
impl PySender {
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
            py.allow_threads(|| self.owned.with_mut(|t| t.0.send_bytes(slice))),
        )
    }

    /// Snapshot of wire-level statistics. Returns a frozen `SocketStats`
    /// dataclass; the `bytes_sent` / `packets_sent` counters tick on
    /// each successful `.send()`.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core_stats = pyok(
            py,
            &RTP,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|t| t.0.socket_stats().unwrap_or_default())
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
            "Sender(closed)".to_string()
        } else {
            "Sender(open)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyReceiver — wraps tst_rtp::RtpRecvTransport
// ---------------------------------------------------------------------------

/// Python RTP receiver — wraps `tst_rtp::RtpRecvTransport`.
///
/// Binds to `url` (literal IP:port). For multicast URLs, joins the
/// group automatically. The receive buffer sizes itself to the
/// transport's deliverable ceiling; `?pkt_size=` on a receiver URL is
/// rejected. The 12-byte RTP header is stripped internally so `.recv()`
/// returns just the TS payload bytes.
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

#[pyclass(name = "Receiver", module = "tstrans.rtp")]
pub(crate) struct PyReceiver {
    /// The binding layer's handle state machine (Arc 2): a parked `recv`
    /// holds the slot only inside `with_mut`, under `py.allow_threads`;
    /// `close()` (cancel-first) ends it instead of waiting behind it.
    owned: Owned<RtpRecvInner>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// Handle onto the transport's [`StreamEndReasonHandle`], pulled at
    /// construction — before the slot is ever emptied by `close()`, so
    /// `end_reason()` / `end_detail()` keep working after close (`close()`
    /// records `Cancelled` on the transport before dropping it, and this
    /// handle shares the same underlying cell). Lock-free: never touches
    /// the slot, so a watchdog can read it while a recv is parked.
    end_reason: StreamEndReasonHandle,
}

#[pymethods]
impl PyReceiver {
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
        let res = py.allow_threads(|| {
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
        // A2's K6 peer-EOS rule lives on the pipeline SHELL impls; this
        // class holds a raw `RtpRecvTransport`, so `From<TransportError>`
        // would flatten a peer EOS to `CLOSED`. Apply the shell rule here
        // instead, so `END_OF_STREAM` is reachable and a caller can tell a
        // clean peer end (an RTSP teardown on a session-derived receiver)
        // from their own `close()`.
        let res = res.map(|r| {
            r.map_err(|e| match e {
                TransportError::Closed => {
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
                BindingError {
                    kind: BindingErrorKind::Backpressure,
                    detail: "recv deadline elapsed".into(),
                },
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
            py.allow_threads(|| {
                self.owned
                    .with_ref(|s| s.transport.socket_stats().unwrap_or_default())
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
    /// (or ended through a path this arc doesn't instrument). Still
    /// readable after `close()` — `end_reason` is a
    /// [`StreamEndReasonHandle`] captured at construction, independent of
    /// the slot's lifetime.
    ///
    /// `StreamEndReasonHandle::get` is a lock-free `Arc<OnceLock<_>>`
    /// read — no blocking, so no `py.allow_threads` is needed here even
    /// though this touches Rust-owned shared state.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.end_reason.get() {
            Some(r) => crate::rtp::end_reason::end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Free-text detail for `end_reason()` — the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`; `None`
    /// for every other reason (including "hasn't ended yet").
    fn end_detail(&self) -> Option<String> {
        let reason = self.end_reason.get()?;
        crate::rtp::end_reason::end_reason_detail(&reason).map(str::to_owned)
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
            "Receiver(open)".to_string()
        } else {
            "Receiver(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySocketStats>()?;
    m.add_class::<PyCancelHandle>()?;
    m.add_class::<PySender>()?;
    m.add_class::<PyReceiver>()?;
    Ok(())
}
