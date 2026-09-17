//! Python bindings for tst-udp (`tstrans.udp`). Gated on `feature = "udp"`.
//!
//! Populated by Plan A5b Wave A (Tasks 3-5). Mirrors the rtp/ module
//! structure: concrete transport wrappers + builder PyClasses + SocketStats
//! + error mapping.
//!
//! GIL boundaries:
//! - `send`, `recv`, builder `build`, `close`, `stats` → the slot lock is
//!   taken inside `py.allow_threads(...)`, so concurrent Python threads
//!   keep running while UDP I/O blocks on the kernel and while a getter
//!   waits for a parked call.
//!
//! Cross-thread close: neither `tst_udp::UdpTransport` nor
//! `UdpRecvTransport` exposes a cancel handle, so this binding owns one —
//! `RecvTransport.recv()` polls the socket in ≤100 ms slices and checks a
//! `stop` flag that `close()` sets BEFORE taking the slot; a `recv()`
//! parked on another thread therefore ends with `UdpError(CLOSED)` within
//! about one slice. Every wrapper borrows `&self` over an
//! `Arc<Mutex<Option<_>>>` slot (the PR #209 shape).
//!
//! Bytes-like extraction in `Transport.send(payload)` follows the abi3-py310
//! two-path pattern from rtp/transport.rs: fast zero-copy `&[u8]` extract
//! for `bytes`, fallback through Python `bytes()` builtin for
//! `bytearray`/`memoryview`.
//!
//! Error mapping: `tst_udp::UdpError` → `tstrans.exceptions.UdpError`.
//! `.kind` is populated from two sources: `tst_udp::UdpErrorKind`'s own
//! variants (URL, IO, INVALID_CONFIG), and the transport-level kinds this
//! binding maps from `tst_core::transport::TransportError` onto the same
//! exception (CLOSED, PAYLOAD_TOO_LARGE — see `transport_error_to_pyerr`
//! below). The Rust `UdpErrorKind` numeric codes are 1-indexed; the
//! Python `UdpErrorKind` enum is 0-indexed — the mapping uses enum
//! *names*, not numeric codes, so there is no off-by-one issue.
//!
//! The consolidated `scripts/check/python/error-mapping-coverage.sh`
//! ratchet enforces that every `UdpErrorKind` variant has at least one
//! literal `make_udp_error(py, "<VARIANT>", ...)` call site in this crate.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_udp::{UdpError, UdpErrorKind, UdpRecvTransport, UdpTransport};

use crate::errors::make_udp_error;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map a `tst_udp::UdpError` to a `tstrans.exceptions.UdpError` PyErr.
///
/// `UdpErrorKind` is `#[non_exhaustive]`; the wildcard arm routes any
/// unknown future variant to `IO` so this fn never panics on a Rust-side
/// enum addition. The bash ratchet will surface the omission in CI.
///
/// Each of `UdpErrorKind`'s Rust-enum variants gets a literal call site
/// below so the consolidated `scripts/check/python/error-mapping-coverage.sh`
/// ratchet stays green. `transport_error_to_pyerr` below maps the remaining
/// observable kinds (`CLOSED`, `PAYLOAD_TOO_LARGE`) from `TransportError`
/// instead — those never reach this function.
fn map_udp_error(py: Python<'_>, e: UdpError) -> PyErr {
    let msg = e.to_string();
    match e.kind() {
        UdpErrorKind::Url => make_udp_error(py, "URL", &msg),
        UdpErrorKind::Io => make_udp_error(py, "IO", &msg),
        UdpErrorKind::InvalidConfig => make_udp_error(py, "INVALID_CONFIG", &msg),
        // Wildcard for #[non_exhaustive] additions not yet mapped.
        _ => make_udp_error(py, "IO", &msg),
    }
}

/// Map a `tst_udp::url::UdpUrlError` (from builder `from_url`) to a
/// `tstrans.exceptions.UdpError` with `kind=URL`.
fn map_udp_url_error(py: Python<'_>, e: tst_udp::UdpUrlError) -> PyErr {
    make_udp_error(py, "URL", &e.to_string())
}

/// Map a `tst_core::transport::TransportError` from `send_bytes` to a
/// `tstrans.exceptions.UdpError`. Routing:
/// - `Closed` / `ExplicitClose` → `CLOSED`
/// - `TooLarge` → `PAYLOAD_TOO_LARGE`
/// - all others (`Broken`, `Backpressure`) → `IO`
fn transport_error_to_pyerr(py: Python<'_>, e: TransportError) -> PyErr {
    match e {
        TransportError::Closed | TransportError::ExplicitClose => {
            make_udp_error(py, "CLOSED", "transport closed by caller")
        }
        TransportError::TooLarge { len, max } => {
            let msg = format!("payload {len} exceeds max {max} bytes per datagram");
            make_udp_error(py, "PAYLOAD_TOO_LARGE", &msg)
        }
        other => make_udp_error(py, "IO", &other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// PyUdpStats — frozen mirror of UdpStats
// ---------------------------------------------------------------------------

/// Cumulative stats snapshot for a UDP transport handle.
///
/// Returned by `Transport.stats()` and `RecvTransport.stats()`. Send-side
/// counters are zero on a receive-only handle and vice-versa.
#[pyclass(frozen, get_all, name = "SocketStats", module = "tstrans.udp")]
pub(crate) struct PyUdpStats {
    /// Datagrams successfully sent (sender only).
    pub datagrams_sent: u64,
    /// Bytes successfully sent (sender only).
    pub bytes_sent: u64,
    /// Datagrams successfully received (receiver only).
    pub datagrams_received: u64,
    /// Bytes successfully received (receiver only).
    pub bytes_received: u64,
    /// Send-side I/O errors (sender only).
    pub send_errors: u64,
    /// Receive-side I/O errors (receiver only).
    pub recv_errors: u64,
}

impl From<tst_udp::UdpStats> for PyUdpStats {
    fn from(s: tst_udp::UdpStats) -> Self {
        Self {
            datagrams_sent: s.datagrams_sent,
            bytes_sent: s.bytes_sent,
            datagrams_received: s.datagrams_received,
            bytes_received: s.bytes_received,
            send_errors: s.send_errors,
            recv_errors: s.recv_errors,
        }
    }
}

#[pymethods]
impl PyUdpStats {
    fn __repr__(&self) -> String {
        format!(
            "SocketStats(datagrams_sent={}, bytes_sent={}, \
             datagrams_received={}, bytes_received={})",
            self.datagrams_sent, self.bytes_sent, self.datagrams_received, self.bytes_received,
        )
    }
}

// ---------------------------------------------------------------------------
// PyUdpTransport — wraps tst_udp::UdpTransport
// ---------------------------------------------------------------------------

/// Raw UDP sender — wraps `tst_udp::UdpTransport`.
///
/// Construct via `Transport.builder().url("udp://host:port").build()`.
/// A single transport sends to a fixed peer; to change the destination
/// close this one and build a new transport.
///
/// GIL is released during `send` so other Python threads remain live while
/// the kernel `sendto` blocks.
#[pyclass(name = "Transport", module = "tstrans.udp")]
pub(crate) struct PyUdpTransport {
    /// Shared slot (PR #209 shape): a `send` in flight on another thread
    /// holds it with the GIL released; `close()` takes it afterwards, so
    /// the next `send` raises `UdpError(CLOSED)` instead of the close
    /// raising `RuntimeError: Already borrowed`.
    inner: Arc<Mutex<Option<UdpTransport>>>,
}

#[pymethods]
impl PyUdpTransport {
    /// Return a builder for configuring and constructing a `Transport`.
    #[staticmethod]
    fn builder() -> PyUdpTransportBuilder {
        PyUdpTransportBuilder::default()
    }

    /// Send one datagram payload. Accepts any bytes-like object:
    /// `bytes`, `bytearray`, `memoryview`, or any buffer-protocol object.
    ///
    /// Raises `UdpError(kind=PAYLOAD_TOO_LARGE)` if `len(payload)` exceeds
    /// the configured `pkt_size` (default 1316 bytes / 7 TS packets).
    ///
    /// Releases the GIL during the kernel send call.
    fn send(&self, py: Python<'_>, payload: &Bound<'_, PyAny>) -> PyResult<()> {
        // Zero-copy for `bytes`; one C copy through `bytes()` otherwise
        // (PyBuffer is unavailable under abi3-py310).
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice: &[u8] = coerced.as_bytes();
        match crate::util::with_slot(py, &self.inner, |t| t.send_bytes(slice)) {
            None => Err(make_udp_error(py, "CLOSED", "transport closed")),
            Some(res) => res.map_err(|e| transport_error_to_pyerr(py, e)),
        }
    }

    /// Close the sender. Idempotent and safe from any thread — further
    /// `.send()` calls raise `UdpError(kind=CLOSED)`.
    fn close(&self, py: Python<'_>) {
        crate::util::close_slot(py, &self.inner, |mut t| t.close());
    }

    /// Snapshot of wire-level statistics. `datagrams_sent` / `bytes_sent`
    /// tick on each successful `.send()`.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyUdpStats>> {
        let s = crate::util::with_slot(py, &self.inner, |t| t.stats())
            .ok_or_else(|| make_udp_error(py, "CLOSED", "transport closed"))?;
        Py::new(py, PyUdpStats::from(s))
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
    ) -> bool {
        self.close(py);
        false
    }

    fn __repr__(&self) -> String {
        if crate::util::slot_alive(&self.inner, |_| true) {
            "Transport(open)".to_string()
        } else {
            "Transport(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyUdpTransportBuilder — builder for PyUdpTransport
// ---------------------------------------------------------------------------

/// Builder for `Transport`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// tx = udp.Transport.builder() \
///     .url("udp://127.0.0.1:5004") \
///     .pkt_size(1316) \
///     .build()
/// ```
#[pyclass(name = "TransportBuilder", module = "tstrans.udp")]
#[derive(Default)]
pub(crate) struct PyUdpTransportBuilder {
    url: Option<String>,
    pkt_size: Option<usize>,
    tos: Option<u8>,
    sndbuf: Option<usize>,
    ttl: Option<u8>,
}

#[pymethods]
impl PyUdpTransportBuilder {
    /// Set the destination URL. Required. Must be `udp://host:port`.
    fn url<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.url = Some(s.to_string());
        slf
    }

    /// Override UDP datagram payload size (default 1316 = 7 × 188 TS bytes).
    fn pkt_size(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.pkt_size = Some(v);
        slf
    }

    /// IP TOS / DSCP byte (e.g. `0xb8` for Expedited Forwarding).
    fn tos(mut slf: PyRefMut<'_, Self>, v: u8) -> PyRefMut<'_, Self> {
        slf.tos = Some(v);
        slf
    }

    /// `SO_SNDBUF` size in bytes.
    fn sndbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.sndbuf = Some(v);
        slf
    }

    /// Multicast TTL / IPv6 hop limit (1–255).
    fn ttl(mut slf: PyRefMut<'_, Self>, v: u8) -> PyRefMut<'_, Self> {
        slf.ttl = Some(v);
        slf
    }

    /// Build the `Transport`. Raises `UdpError(kind=URL)` for a bad URL,
    /// `UdpError(kind=IO)` for socket bind/connect failures.
    fn build(&self, py: Python<'_>) -> PyResult<PyUdpTransport> {
        let url_str = self
            .url
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("url(...) is required before build()"))?;
        let mut b = tst_udp::UdpTransportBuilder::from_url(url_str)
            .map_err(|e| map_udp_url_error(py, e))?;
        if let Some(v) = self.pkt_size {
            b.pkt_size(v);
        }
        if let Some(v) = self.tos {
            b.tos(v);
        }
        if let Some(v) = self.sndbuf {
            b.sndbuf(v);
        }
        if let Some(v) = self.ttl {
            b.ttl(v);
        }
        let t = b.build().map_err(|e| map_udp_error(py, e))?;
        Ok(PyUdpTransport {
            inner: Arc::new(Mutex::new(Some(t))),
        })
    }

    fn __repr__(&self) -> String {
        format!("TransportBuilder(url={:?})", self.url)
    }
}

// ---------------------------------------------------------------------------
// PyUdpRecvTransport — wraps tst_udp::UdpRecvTransport
// ---------------------------------------------------------------------------

/// Raw UDP receiver — wraps `tst_udp::UdpRecvTransport`.
///
/// Construct via `RecvTransport.builder().bind_url("udp://0.0.0.0:0").build()`.
/// Binding to port 0 lets the kernel pick a free port; read it back via
/// `.local_addr_port()`.
///
/// GIL is released during `recv` so other Python threads remain live while
/// waiting for a datagram.
/// Transport + reusable scratch buffer under one lock (a `&self` `recv`
/// cannot borrow a `scratch` field mutably).
struct UdpRecvInner {
    transport: UdpRecvTransport,
    scratch: Vec<u8>,
}

/// Longest single kernel wait inside `recv()`: the binding's own
/// cancel-poll interval (same 100 ms cadence `tst_udp` uses internally),
/// so a `close()` from another thread is observed within one slice.
const RECV_POLL_SLICE: Duration = Duration::from_millis(100);

/// Outcome of the polled receive loop, mapped to a `PyErr` once the GIL
/// is back (nothing Python-typed may cross `allow_threads`).
enum UdpRecvOutcome {
    Data(Vec<u8>),
    Closed,
    TimedOut,
    Failed(UdpError),
}

#[pyclass(name = "RecvTransport", module = "tstrans.udp")]
pub(crate) struct PyUdpRecvTransport {
    /// Shared slot (PR #209 shape): a parked `recv` holds it with the GIL
    /// released; `close()` sets `stop` BEFORE taking it, so the parked
    /// recv ends with `UdpError(CLOSED)` within one poll slice.
    inner: Arc<Mutex<Option<UdpRecvInner>>>,
    /// Binding-level cancel: `tst_udp` has no cancel handle, so the
    /// polled `recv` loop checks this between slices.
    stop: Arc<AtomicBool>,
    /// Bound port read once at `build()`, so `local_addr_port()` never
    /// waits behind a parked `recv()` — the thread asking for the port is
    /// usually the one that has to send the datagram that ends that park.
    local_port: u16,
}

#[pymethods]
impl PyUdpRecvTransport {
    /// Return a builder for configuring and constructing a `RecvTransport`.
    #[staticmethod]
    fn builder() -> PyUdpRecvTransportBuilder {
        PyUdpRecvTransportBuilder::default()
    }

    /// Receive one datagram. Returns `(payload_bytes, sender_addr_str)`.
    ///
    /// `timeout_ms`: milliseconds to wait. `None` (default) blocks until a
    /// datagram arrives. On timeout, raises `UdpError(kind=IO)` with the
    /// message "recv timed out".
    ///
    /// Note: `sender_addr_str` is currently always an empty string; the
    /// underlying `recv_bytes` API does not expose the sender address.
    ///
    /// Cross-thread `close()` is supported: the kernel wait is sliced into
    /// ≤100 ms polls and a `close()` from another thread ends a parked
    /// `recv()` with `UdpError(kind=CLOSED)` within about one slice.
    ///
    /// Releases the GIL while waiting on the kernel.
    #[pyo3(signature = (timeout_ms = None))]
    fn recv(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<(Py<PyBytes>, String)> {
        let stop = Arc::clone(&self.stop);
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let outcome = crate::util::with_slot(py, &self.inner, move |s| {
            loop {
                if stop.load(Ordering::Acquire) {
                    return UdpRecvOutcome::Closed;
                }
                let slice = match deadline {
                    None => RECV_POLL_SLICE,
                    Some(dl) => {
                        let now = Instant::now();
                        if now >= dl {
                            return UdpRecvOutcome::TimedOut;
                        }
                        // A sub-millisecond SO_RCVTIMEO rounds to 0 on
                        // Linux, which means "block forever" — floor it.
                        RECV_POLL_SLICE.min(dl - now).max(Duration::from_millis(1))
                    }
                };
                match s.transport.recv_timeout(&mut s.scratch, slice) {
                    Ok(Some(n)) => return UdpRecvOutcome::Data(s.scratch[..n].to_vec()),
                    Ok(None) => continue, // slice elapsed: re-check stop / deadline
                    Err(e) => return UdpRecvOutcome::Failed(e),
                }
            }
        });
        let bytes = match outcome {
            None | Some(UdpRecvOutcome::Closed) => {
                return Err(make_udp_error(py, "CLOSED", "transport closed"));
            }
            Some(UdpRecvOutcome::TimedOut) => {
                return Err(make_udp_error(py, "IO", "recv timed out"));
            }
            Some(UdpRecvOutcome::Failed(e)) => return Err(map_udp_error(py, e)),
            Some(UdpRecvOutcome::Data(b)) => b,
        };
        // recv_bytes doesn't expose the sender address; callers that need
        // the source addr should use a raw socket or filter at the IP layer.
        Ok((PyBytes::new_bound(py, &bytes).unbind(), String::new()))
    }

    /// Local bound port. Useful when the transport was bound to port 0
    /// (kernel picks a free port). Answered from the `build()`-time
    /// snapshot, so it never waits behind a `recv()` parked on another
    /// thread; raises `UdpError(CLOSED)` once the transport is closed.
    fn local_addr_port(&self, py: Python<'_>) -> PyResult<u16> {
        if crate::util::slot_alive(&self.inner, |_| true) {
            Ok(self.local_port)
        } else {
            Err(make_udp_error(py, "CLOSED", "transport closed"))
        }
    }

    /// Close the receiver. Sets the stop flag BEFORE taking the slot, so a
    /// `recv()` parked on another thread ends with `UdpError(kind=CLOSED)`
    /// within ~100 ms; further `.recv()` calls raise the same. Idempotent.
    fn close(&self, py: Python<'_>) {
        self.stop.store(true, Ordering::Release);
        crate::util::close_slot(py, &self.inner, |mut s| s.transport.close());
    }

    /// Snapshot of wire-level statistics. `datagrams_received` /
    /// `bytes_received` tick on each successful `.recv()`.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyUdpStats>> {
        let s = crate::util::with_slot(py, &self.inner, |s| s.transport.stats())
            .ok_or_else(|| make_udp_error(py, "CLOSED", "transport closed"))?;
        Py::new(py, PyUdpStats::from(s))
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
    ) -> bool {
        self.close(py);
        false
    }

    fn __repr__(&self) -> String {
        if crate::util::slot_alive(&self.inner, |_| true) {
            "RecvTransport(open)".to_string()
        } else {
            "RecvTransport(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyUdpRecvTransportBuilder — builder for PyUdpRecvTransport
// ---------------------------------------------------------------------------

/// Builder for `RecvTransport`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// rx = udp.RecvTransport.builder() \
///     .bind_url("udp://0.0.0.0:5004") \
///     .rcvbuf(8 * 1024 * 1024) \
///     .build()
/// ```
#[pyclass(name = "RecvTransportBuilder", module = "tstrans.udp")]
#[derive(Default)]
pub(crate) struct PyUdpRecvTransportBuilder {
    url: Option<String>,
    rcvbuf: Option<usize>,
    iface: Option<String>,
}

#[pymethods]
impl PyUdpRecvTransportBuilder {
    /// Set the bind URL. Required. Must be `udp://bind_addr:port` or
    /// `udp://@group:port` for multicast recv.
    fn bind_url<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.url = Some(s.to_string());
        slf
    }

    /// `SO_RCVBUF` size in bytes.
    fn rcvbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.rcvbuf = Some(v);
        slf
    }

    /// Multicast interface name or literal IP for the join call.
    fn iface<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.iface = Some(s.to_string());
        slf
    }

    /// Build the `RecvTransport`. Raises `UdpError(kind=URL)` for a bad
    /// bind URL, `UdpError(kind=IO)` for socket bind failures.
    fn build(&self, py: Python<'_>) -> PyResult<PyUdpRecvTransport> {
        let url_str = self
            .url
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("bind_url(...) is required before build()"))?;
        let mut b = tst_udp::UdpRecvTransportBuilder::from_url(url_str)
            .map_err(|e| map_udp_url_error(py, e))?;
        if let Some(v) = self.rcvbuf {
            b.rcvbuf(v);
        }
        if let Some(ref s) = self.iface {
            b.iface(s.as_str());
        }
        let t = b.build().map_err(|e| map_udp_error(py, e))?;
        // Size the scratch buffer to hold the largest legal datagram.
        // Recv max_payload() is a flat 65535 deliverable ceiling; 65_536
        // keeps the historical scratch size.
        let scratch_len = t.max_payload().max(65_536);
        let local_port = t.local_addr().port();
        Ok(PyUdpRecvTransport {
            inner: Arc::new(Mutex::new(Some(UdpRecvInner {
                transport: t,
                scratch: vec![0u8; scratch_len],
            }))),
            stop: Arc::new(AtomicBool::new(false)),
            local_port,
        })
    }

    fn __repr__(&self) -> String {
        format!("RecvTransportBuilder(url={:?})", self.url)
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "udp")?;
    m.add_class::<PyUdpStats>()?;
    m.add_class::<PyUdpTransport>()?;
    m.add_class::<PyUdpTransportBuilder>()?;
    m.add_class::<PyUdpRecvTransport>()?;
    m.add_class::<PyUdpRecvTransportBuilder>()?;
    parent.add_submodule(&m)?;
    Ok(())
}
