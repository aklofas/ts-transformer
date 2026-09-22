//! `Sender`, `Receiver`, `SocketStats`, `SrtStats`, `CancelHandle`
//! (the basic user-visible transports for `tstrans.srt`).
//!
//! Mirrors the `tstrans.rtp` binding shape:
//! - Per-direction concrete PyClass (not generic over `T: Transport`).
//! - GIL released around `connect` / `accept` / `send` / `recv`.
//! - Every wrapper holds a `tst_pipeline::binding::Owned<T>`: the slot is
//!   taken only inside `with_mut` / `with_ref`, always under
//!   `py.allow_threads`, and `close()` is `Owned::close` — cancel first,
//!   then take, then `Close::close` inside a panic boundary. A call parked
//!   on another thread therefore ends promptly instead of blocking the
//!   close or tripping PyO3's borrow check.
//! - Bytes-like extraction follows audit-backlog #10's two-path pattern:
//!   fast `&[u8]` for real `bytes`, fallback through `builtins.bytes(x)`
//!   for `bytearray` / `memoryview` (gated under PyO3's abi3-py310
//!   because `PyBuffer` is hidden behind `not(Py_LIMITED_API)`).
//! - Errors take the one raise path (`crate::raise`): every failure is a
//!   `tst_pipeline::binding::BindingError` whose kind is resolved on
//!   `tstrans.exceptions.SrtErrorKind` by name, checked at import.
//!
//! URL dispatch (the composition itself lives in `tst_srt`, Arc 2 A3):
//! - `Sender::from_url` requires `?mode=caller` (the SrtUrl default) and
//!   dials through `SrtUrl::connect_recv` — overlay only, no sender
//!   preset, which is what this open composed before Arc 2.
//! - `Receiver::from_url` requires `?mode=listener` and goes through
//!   `SrtUrl::accept_one`: bind, accept ONE peer, drop the listener. The
//!   empty-host `0.0.0.0` rule and IPv6 bracketing live there, so this
//!   module formats no address at all.
//!
//! The Receiver one-shot semantics mirror libsrt: each accepted Socket
//! is its own connection; for a listener that hosts many peers, callers
//! should use the lower-level `Listener` PyClass (T3) and iterate.
//!
//! There is NO separate receive-only transport type in the Rust crate —
//! `tst_srt::SrtTransport` implements both `Transport` (send) and
//! `RecvTransport` (recv). Construction is identical for both
//! directions; the only difference is which `tst_pipeline::Sender` /
//! `Receiver` shell wraps it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]
use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::SocketStats;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig, Sender as PlSender, SenderConfig};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::raise::{SRT, pyok, pyres, raise};
use crate::util::{CancelSource, alive_probe, close_owned};
// ---------------------------------------------------------------------------
// PySocketStats — frozen mirror of tst_core::transport::SocketStats
// ---------------------------------------------------------------------------

/// Mirror of `tst_core::transport::SocketStats` exposed to Python as a
/// frozen, `get_all`-decorated PyClass. Property names match
/// `tstrans.rtp.SocketStats` 1:1 so cross-transport code can read the
/// same dataclass-shape from both.
///
/// For SRT-specific extras (`mbps_estimated_bandwidth`, RTT as
/// `Duration`, the symmetric send/recv-side byte-loss split), use
/// `srt_stats()` which returns `SrtStats`.
#[pyclass(frozen, get_all, name = "SocketStats", module = "tstrans.srt")]
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
// PySrtStats — frozen mirror of tst_srt::Stats (17 fields)
// ---------------------------------------------------------------------------

/// Mirror of `tst_srt::Stats` — the libsrt-flavored 17-field stats
/// struct. Exposes the SRT-rich fields that don't fit the abstract
/// `SocketStats` shape:
/// - `mbps_estimated_bandwidth` (libsrt's estimate; bps view lives in
///   `SocketStats::link_bandwidth_bps`).
/// - Symmetric send/recv-side byte-loss split
///   (`bytes_lost_send_side` + `bytes_lost_recv_side`).
/// - Symmetric send/recv-side packet drop split.
///
/// `rtt_us` is the `Duration` converted to microseconds, saturating at
/// `u32::MAX` — matches the `SocketStats::rtt_us` projection so callers
/// can pin either accessor and get the same view.
#[pyclass(frozen, get_all, name = "SrtStats", module = "tstrans.srt")]
pub(crate) struct PySrtStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bytes_lost_recv_side: u64,
    pub bytes_lost_send_side: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_lost_recv_side: u64,
    pub packets_lost_send_side: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_recv_side: u64,
    pub packets_dropped_send_side: u64,
    pub rtt_us: u32,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub mbps_estimated_bandwidth: f64,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
}

impl PySrtStats {
    fn from_srt(s: &tst_srt::Stats) -> Self {
        let rtt_us = u32::try_from(s.rtt.as_micros()).unwrap_or(u32::MAX);
        Self {
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
            bytes_lost_recv_side: s.bytes_lost_recv_side,
            bytes_lost_send_side: s.bytes_lost_send_side,
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            packets_lost_recv_side: s.packets_lost_recv_side,
            packets_lost_send_side: s.packets_lost_send_side,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped_recv_side: s.packets_dropped_recv_side,
            packets_dropped_send_side: s.packets_dropped_send_side,
            rtt_us,
            send_bandwidth_bps: s.send_bandwidth_bps,
            recv_bandwidth_bps: s.recv_bandwidth_bps,
            mbps_estimated_bandwidth: s.mbps_estimated_bandwidth,
            send_buffer_packets: s.send_buffer_packets,
            recv_buffer_packets: s.recv_buffer_packets,
        }
    }
}

#[pymethods]
impl PySrtStats {
    fn __repr__(&self) -> String {
        format!(
            "SrtStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={}, mbps_estimated_bandwidth={:.3})",
            self.bytes_sent,
            self.packets_sent,
            self.bytes_received,
            self.packets_received,
            self.rtt_us,
            self.mbps_estimated_bandwidth,
        )
    }
}

// ---------------------------------------------------------------------------
// PyCancelHandle
// ---------------------------------------------------------------------------

/// Python-side cancel handle. Wraps the shell's shared
/// [`crate::util::CancelSource`]: every clone obtained from the same
/// shell — and the shell's own `close()` — forwards into one
/// `Arc<dyn TransportCancel>` and flips one flag, so `is_cancelled()`
/// reports the shell's state, not this wrapper's history (Arc 2).
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.srt")]
pub(crate) struct PyCancelHandle {
    src: Arc<crate::util::CancelSource>,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent. Wakes a thread parked in
    /// `send_bytes` / `recv_bytes` / `accept` / `__next__`; that call
    /// raises `SrtError(CLOSED)` — on the plain shells still `BROKEN`
    /// until the SRT transport-level cancel change (later in 0.7.0).
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
// ---------------------------------------------------------------------------
// PySender — wraps tst_pipeline::Sender<tst_srt::SrtTransport>
// ---------------------------------------------------------------------------

/// Python SRT sender — wraps `tst_pipeline::Sender<SrtTransport>`.
///
/// Constructed via `Sender.from_url("srt://host:port?...")`. The URL
/// must use `mode=caller` (default when omitted); query parameters
/// apply through `SrtUrl::connect_recv` (passphrase, latency, streamid, …).
#[pyclass(name = "Sender", module = "tstrans.srt")]
pub(crate) struct PySender {
    /// The binding layer's handle state machine (Arc 2): the slot is
    /// locked only inside `with_mut` / `with_ref`, always under
    /// `py.allow_threads`, so a `send_bytes` parked on another thread
    /// never trips PyO3's borrow check and `close()` (cancel-first)
    /// ends it instead of waiting behind it.
    owned: Owned<PlSender<SrtTransport>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

impl PySender {
    /// Wrap an already-connected transport (the `from_url` and
    /// `Socket.into_sender()` paths meet here).
    pub(crate) fn from_transport(transport: SrtTransport) -> Self {
        let cancel = srt_cancel_source(&transport);
        let inner = PlSender::new(transport, SenderConfig::default());
        Self {
            owned: Owned::new(inner, cancel.as_dyn(), ()),
            cancel,
        }
    }
}

/// A3's non-`Option` accessor (`SrtTransport::srt_cancel_handle()`, taken
/// BEFORE the transport moves into the shell): no `.expect`, no `Option`.
pub(crate) fn srt_cancel_source(t: &SrtTransport) -> Arc<CancelSource> {
    CancelSource::new(Arc::new(t.srt_cancel_handle()))
}

#[pymethods]
impl PySender {
    /// Connect as a caller. Releases the GIL during the handshake.
    /// Raises `SrtError(CONFIG_INVALID)` for a bad URL or a non-caller
    /// mode, `SrtError(CONNECT_FAILED | TIMEOUT)` on handshake failure.
    ///
    /// Dials through `SrtUrl::connect_recv` — overlay only, no sender
    /// preset — because that is exactly what this open composed before
    /// Arc 2 (`SocketConfig::default()` + `apply_to_socket`). Routing it
    /// through `SrtUrl::connect` would newly set `SRTO_SENDER`, a 15 s
    /// connect timeout and a 5 s linger on every plain sender.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Caller {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "Sender.from_url requires ?mode=caller (default); got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        let transport = py
            .allow_threads(|| parsed.connect_recv())
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        Ok(Self::from_transport(transport))
    }

    /// Send one pre-muxed TS chunk (any bytes-like). Releases the GIL
    /// while the libsrt send blocks; a concurrent `close()` cancels first,
    /// so the call ends with `SrtError(CLOSED)` (plain SRT may still
    /// report `BROKEN` until the transport-level cancel change lands).
    fn send_bytes(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice: &[u8] = coerced.as_bytes();
        let res = py.allow_threads(|| self.owned.with_mut(|s| s.send_ts(slice)));
        pyres(py, &SRT, res)
    }

    /// Flush a partial 7-packet bundle (see `Sender::send_ts`). Releases
    /// the GIL. Call it before `close()` when the tail matters — `close()`
    /// cancels first and does not drain the framing buffer.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        let res = py.allow_threads(|| self.owned.with_mut(|s| s.flush()));
        pyres(py, &SRT, res)
    }

    /// Shareable cancel handle (shared state — see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Scheme-neutral 16-field wire stats. Waits (GIL released) for an
    /// in-flight `send_bytes` on another thread to release the slot.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.socket_stats().unwrap_or_default())
        });
        Py::new(py, PySocketStats::from_core(pyok(py, &SRT, core)?))
    }

    /// SRT-rich 17-field stats. `IoError::SocketClosed` surfaces as `CLOSED`.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let stats = py.allow_threads(|| self.owned.with_ref(|s| s.transport().stats()));
        let stats = pyres(py, &SRT, stats)?;
        Py::new(py, PySrtStats::from_srt(&stats))
    }

    /// Close: cancel first (a parked `send_bytes` on another thread ends
    /// promptly), then take the slot and close the transport. A partial
    /// bundle in the framing buffer is not delivered — `flush()` first
    /// when the tail matters. Idempotent; later calls raise `CLOSED`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the sender owns a live transport (a send in flight on
    /// another thread counts as live — the probe never waits).
    fn is_alive(&self) -> bool {
        alive_probe(&self.owned, |s| s.is_alive())
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

// ---------------------------------------------------------------------------
// PyReceiver — wraps tst_pipeline::Receiver<tst_srt::SrtTransport>
// ---------------------------------------------------------------------------

/// Python SRT receiver — wraps `tst_pipeline::Receiver<SrtTransport>`.
/// `Receiver.from_url("srt://...?mode=listener")` binds, listens and
/// accepts ONE peer (subsequent peers need a fresh `from_url` or the
/// low-level `Listener`).
#[pyclass(name = "Receiver", module = "tstrans.srt")]
pub(crate) struct PyReceiver {
    owned: Owned<PlReceiver<SrtTransport>>,
    cancel: Arc<CancelSource>,
}

impl PyReceiver {
    pub(crate) fn from_transport(transport: SrtTransport) -> Self {
        let cancel = srt_cancel_source(&transport);
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        Self {
            owned: Owned::new(inner, cancel.as_dyn(), ()),
            cancel,
        }
    }
}

#[pymethods]
impl PyReceiver {
    /// Bind + accept one peer (GIL released). An empty host binds
    /// `0.0.0.0`.
    ///
    /// Raises `SrtError(CONFIG_INVALID)` for a bad URL or a non-listener
    /// mode, and `SrtError(BROKEN)` for any bind or accept fault — the
    /// message is prefixed `bind: ` or `accept: `. Before 0.7.0 this open
    /// had its own mapping (`CONNECT_FAILED` / `CONFIG_INVALID` for bind,
    /// `ACCEPT_FAILED` / `TIMEOUT` for accept); it now shares
    /// `SrtUrl::accept_one` with the C ABI, which classifies both as
    /// transport faults.
    ///
    /// The first accept is not cancellable — no handle exists until this
    /// returns.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Listener {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "Receiver.from_url requires ?mode=listener; got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        let slot = tst_core::cancel::CancelSlot::new();
        let transport = py
            .allow_threads(|| parsed.accept_one(&slot))
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        Ok(Self::from_transport(transport))
    }

    /// One 188-byte TS packet per call (SRT live mode's natural quantum);
    /// `max_len` is accepted for API symmetry and floored at 188. Releases
    /// the GIL while parked; `close()` on another thread cancels first.
    ///
    /// SRT live mode delivers in 188-byte units, so a caller asking for
    /// `max_len=1500` must not be made to wait for 1500 bytes — the call
    /// returns after the first packet so it stays deterministic.
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let _cap = max_len.max(188);
        let pkt = py.allow_threads(|| self.owned.with_mut(|r| r.next_packet()));
        let bytes = pyres(py, &SRT, pkt)?;
        Ok(PyBytes::new_bound(py, &bytes).unbind())
    }

    /// Shareable cancel handle (shared state — see `CancelHandle`).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = py.allow_threads(|| {
            self.owned
                .with_ref(|r| r.socket_stats().unwrap_or_default())
        });
        Py::new(py, PySocketStats::from_core(pyok(py, &SRT, core)?))
    }

    /// Snapshot of the SRT-rich 17-field stats.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let stats = py.allow_threads(|| self.owned.with_ref(|r| r.transport().stats()));
        let stats = pyres(py, &SRT, stats)?;
        Py::new(py, PySrtStats::from_srt(&stats))
    }

    /// Close: cancel first, so a `recv_bytes()` parked on another thread
    /// ends promptly (`CLOSED`; plain SRT may still say `BROKEN` until the
    /// transport-level change), then close the socket. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the receiver owns a live transport (a parked
    /// `recv_bytes` on another thread counts as live — never waits).
    fn is_alive(&self) -> bool {
        alive_probe(&self.owned, |r| r.is_alive())
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

impl PyCancelHandle {
    /// The single constructor every srt class uses: clone the shell's
    /// shared cancel state so this handle observes — and contributes to —
    /// the same cancel.
    pub(crate) fn from_source(src: &Arc<crate::util::CancelSource>) -> Self {
        Self {
            src: Arc::clone(src),
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySocketStats>()?;
    m.add_class::<PySrtStats>()?;
    m.add_class::<PyCancelHandle>()?;
    m.add_class::<PySender>()?;
    m.add_class::<PyReceiver>()?;
    Ok(())
}
