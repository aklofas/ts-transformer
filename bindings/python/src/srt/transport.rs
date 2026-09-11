//! `Sender`, `Receiver`, `SocketStats`, `SrtStats`, `CancelHandle`
//! (the basic user-visible transports for `tstrans.srt`).
//!
//! Mirrors the `tstrans.rtp` binding shape:
//! - Per-direction concrete PyClass (not generic over `T: Transport`).
//! - GIL released around `connect`/`bind+accept`/`send`/`recv`.
//! - Bytes-like extraction follows audit-backlog #10's two-path pattern:
//!   fast `&[u8]` for real `bytes`, fallback through `builtins.bytes(x)`
//!   for `bytearray` / `memoryview` (gated under PyO3's abi3-py310
//!   because `PyBuffer` is hidden behind `not(Py_LIMITED_API)`).
//! - Error mapping uses `make_srt_error(py, "KIND", &msg)` with the
//!   KIND literal on the same line as the open-paren (required by
//!   the T4 line-based grep ratchet).
//!
//! URL dispatch:
//! - `Sender::from_url` requires `?mode=caller` (the SrtUrl default).
//!   Calls `Socket::connect_with(&cfg, "host:port")`.
//! - `Receiver::from_url` requires `?mode=listener`. Calls
//!   `Listener::bind_with(&cfg, "host:port")` then one-shot `accept()`.
//!
//! The Receiver one-shot semantics mirror libsrt: each accepted Socket
//! is its own connection; for a listener that hosts many peers, callers
//! should use the lower-level `Listener` PyClass (T3) and iterate.
//!
//! There is NO separate `SrtRecvTransport` in the Rust crate —
//! `tst_srt::SrtTransport` implements both `Transport` (send) and
//! `RecvTransport` (recv). Construction is identical for both
//! directions; the only difference is which `tst_pipeline::Sender` /
//! `Receiver` shell wraps it.
//!
//! Cross-crate plumbing added by this task:
//! - `tst_pipeline::Sender::transport(&self) -> &T`
//! - `tst_pipeline::Receiver::transport(&self) -> &R`
//! - `tst_srt::SrtTransport::stats(&self) -> Result<Stats, IoError>`
//!
//! Both are additive; both bump the respective `cargo public-api`
//! baselines.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::{SocketStats, TransportCancel};
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig, Sender as PlSender, SenderConfig};
use tst_srt::error::{AcceptError, BindError};
use tst_srt::{Listener, ListenerConfig, Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use crate::errors::make_srt_error;
use crate::srt::errors::{
    accept_error_to_pyerr, bind_error_to_pyerr, connect_error_to_pyerr, io_error_to_pyerr,
    transport_error_to_pyerr, url_error_to_pyerr,
};

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

/// Python-side cancel handle. Wraps an `Arc<dyn TransportCancel>` so
/// multiple Python references (e.g., one held by the Sender, one
/// stashed in a worker thread) share a single cancellation target.
/// Calling `.cancel()` on any reference wakes the parked send/recv
/// loop on the next libsrt I/O cycle (~3-10 ms; the exact mechanism is
/// libsrt's `srt_close` on the paired socket handle).
///
/// The `TransportCancel` trait deliberately exposes only `cancel()` —
/// no query path. To surface a Python-visible `is_cancelled()` we track
/// a local flag here. Each `PyCancelHandle` clone has its own flag,
/// but they all forward `cancel()` into the same `Arc<dyn>` so calling
/// `.cancel()` on any clone still wakes the parked socket; only the
/// flag observation is per-clone. Tests use the returned handle's own
/// `is_cancelled()` to wait for the cancel signal.
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.srt")]
pub(crate) struct PyCancelHandle {
    inner: Arc<dyn TransportCancel + Send + Sync>,
    /// Per-handle observation of whether `cancel()` was invoked on
    /// this Python wrapper. Frozen PyClass requires interior
    /// mutability — `AtomicBool` is the cheapest shape.
    flag: AtomicBool,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent — repeated calls are a no-op.
    /// Wakes a thread parked in `Sender.send_bytes` / `Receiver.recv_bytes`;
    /// that call returns an `SrtError` with `.kind == SrtErrorKind.BROKEN`
    /// or `CLOSED` depending on which libsrt path the cancel races.
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.inner.cancel();
    }

    /// Returns `True` once `.cancel()` has been called on **this**
    /// Python handle. Advisory — the underlying socket close may not
    /// have completed yet on another thread, and other clones obtained
    /// via separate `cancel_handle()` calls track their own flags.
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
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
/// must use `mode=caller` (default when omitted). Query parameters
/// apply through `UrlOverlay::apply_to_socket` — passphrase, latency,
/// streamid, mss, payloadsize, etc.
#[pyclass(name = "Sender", module = "tstrans.srt")]
pub(crate) struct PySender {
    inner: Option<PlSender<SrtTransport>>,
    /// Trait-erased cancel handle pulled from the transport at
    /// construction. Shared with any Python-side `CancelHandle` clones;
    /// calling `.cancel()` here closes the paired libsrt socket and
    /// wakes any thread parked in `send_ts`.
    cancel: Arc<dyn TransportCancel + Send + Sync>,
}

#[pymethods]
impl PySender {
    /// Construct a sender from a `srt://...` URL with `mode=caller`
    /// (the default). Resolves the host, opens a libsrt socket, applies
    /// any query-string options via `UrlOverlay::apply_to_socket`, and
    /// blocks on the SRT handshake.
    ///
    /// Releases the GIL during the handshake (`srt_connect`) so other
    /// Python threads can run.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "Sender.from_url requires ?mode=caller (default); got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }
        let mut cfg = SocketConfig::default();
        parsed.overlay.apply_to_socket(&mut cfg);
        let addr = if parsed.host.contains(':') && !parsed.host.starts_with('[') {
            // IPv6 literal without brackets — must bracket for SocketAddr parse.
            format!("[{}]:{}", parsed.host, parsed.port)
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };
        let socket = py
            .allow_threads(|| Socket::connect_with(&cfg, addr.as_str()))
            .map_err(|e| connect_error_to_pyerr(py, e))?;
        let transport = SrtTransport::new(socket);
        let inner = PlSender::new(transport, SenderConfig::default());
        // Pull the cancel handle. `SrtTransport::cancel_handle` always
        // returns `Some` for a live socket.
        let cancel = inner
            .cancel_handle()
            .expect("SrtTransport with a live socket always returns Some(cancel_handle)");
        Ok(Self {
            inner: Some(inner),
            cancel,
        })
    }

    /// Send one pre-muxed TS-bytes chunk over SRT. Accepts any
    /// bytes-like input (`bytes`, `bytearray`, `memoryview`, numpy
    /// uint8). Releases the GIL during the underlying `srt_sendmsg2`
    /// call so other Python threads can run while this thread blocks
    /// on the kernel.
    fn send_bytes(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "sender is closed"))?;
        // Fast path: real `bytes` extracts to a zero-copy &[u8].
        if let Ok(slice) = data.extract::<&[u8]>() {
            let res = py.allow_threads(|| inner.send_ts(slice));
            return res.map_err(|e| match e.source {
                tst_pipeline::sender::SenderErrorSource::Transport(t) => {
                    transport_error_to_pyerr(py, t)
                }
                tst_pipeline::sender::SenderErrorSource::Framing(f) => {
                    make_srt_error(py, "CONFIG_INVALID", &f.to_string())
                }
                _ => make_srt_error(py, "IO", &e.to_string()),
            });
        }
        // Fallback for bytearray / memoryview / numpy etc.
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice: &[u8] = coerced.as_bytes();
        let res = py.allow_threads(|| inner.send_ts(slice));
        res.map_err(|e| match e.source {
            tst_pipeline::sender::SenderErrorSource::Transport(t) => {
                transport_error_to_pyerr(py, t)
            }
            tst_pipeline::sender::SenderErrorSource::Framing(f) => {
                make_srt_error(py, "CONFIG_INVALID", &f.to_string())
            }
            _ => make_srt_error(py, "IO", &e.to_string()),
        })
    }

    /// Flush any buffered partial TS bundle. `Sender::send_ts` bundles
    /// 188-byte packets into 7-packet (1316-byte) chunks before pushing
    /// to the SRT socket; sending fewer than 7 packets leaves a
    /// partial bundle stuck in the framing buffer until `flush()` is
    /// called (or until enough additional packets arrive to fill the
    /// bundle).
    ///
    /// Releases the GIL during the underlying transport send.
    fn flush(&mut self, py: Python<'_>) -> PyResult<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "sender is closed"))?;
        let res = py.allow_threads(|| inner.flush());
        res.map_err(|e| match e.source {
            tst_pipeline::sender::SenderErrorSource::Transport(t) => {
                transport_error_to_pyerr(py, t)
            }
            tst_pipeline::sender::SenderErrorSource::Framing(f) => {
                make_srt_error(py, "CONFIG_INVALID", &f.to_string())
            }
            _ => make_srt_error(py, "IO", &e.to_string()),
        })
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in
    /// `.send_bytes()`; that call returns `SrtError(kind=BROKEN)` or
    /// `SrtError(kind=CLOSED)` depending on which libsrt path the
    /// cancel races.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(
            py,
            PyCancelHandle {
                inner: self.cancel.clone(),
                flag: AtomicBool::new(false),
            },
        )
    }

    /// Snapshot of the scheme-neutral 16-field wire stats (matches
    /// `tstrans.rtp.SocketStats`). For SRT-specific extras, use
    /// `srt_stats()`.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "sender is closed"))?;
        let core = inner.socket_stats().unwrap_or_default();
        Py::new(py, PySocketStats::from_core(core))
    }

    /// Snapshot of the SRT-rich 17-field stats. Includes RTT, the
    /// symmetric send/recv-side byte-loss split, and libsrt's bandwidth
    /// estimate (`mbps_estimated_bandwidth`).
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "sender is closed"))?;
        // Reach through the new `Sender::transport()` accessor (added
        // by this task) to call `SrtTransport::stats()` (also added by
        // this task). `IoError::SocketClosed` means the transport was
        // torn down mid-send; surface as CLOSED so callers can tell
        // it apart from the more general IO catchall.
        let stats = inner
            .transport()
            .stats()
            .map_err(|e| io_error_to_pyerr(py, e))?;
        Py::new(py, PySrtStats::from_srt(&stats))
    }

    /// Close the sender. After close, further `.send_bytes()` calls
    /// raise `SrtError(kind=CLOSED)`. Idempotent.
    fn close(&mut self) {
        if let Some(mut t) = self.inner.take() {
            t.close();
        }
    }

    /// `True` while the sender owns a live transport.
    fn is_alive(&self) -> bool {
        self.inner.as_ref().is_some_and(|s| s.is_alive())
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(_) => "Sender(open)".to_string(),
            None => "Sender(closed)".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// PyReceiver — wraps tst_pipeline::Receiver<tst_srt::SrtTransport>
// ---------------------------------------------------------------------------

/// Python SRT receiver — wraps `tst_pipeline::Receiver<SrtTransport>`.
///
/// Constructed via `Receiver.from_url("srt://...?mode=listener")`. The
/// URL must use `mode=listener`. Binds the socket, listens, and blocks
/// on the first incoming SRT handshake. The accepted socket becomes
/// the receive transport; this is a one-shot accept (subsequent peers
/// must use a fresh `from_url` call or the lower-level `Listener` from
/// T3).
#[pyclass(name = "Receiver", module = "tstrans.srt")]
pub(crate) struct PyReceiver {
    /// Shared slot so every method borrows `self` immutably: a `recv_bytes`
    /// parked on another thread holds this lock (GIL released), and `close()`
    /// fires `cancel` BEFORE taking it. With a `&mut self` receive, a
    /// cross-thread `close()` tripped PyO3's borrow check (`RuntimeError:
    /// Already borrowed`) instead of waking the parked call.
    inner: Arc<Mutex<Option<PlReceiver<SrtTransport>>>>,
    /// Trait-erased cancel handle pulled from the transport at
    /// construction. Shared with any Python-side `CancelHandle` clones.
    cancel: Arc<dyn TransportCancel + Send + Sync>,
}

#[pymethods]
impl PyReceiver {
    /// Bind a receiver from a `srt://...?mode=listener` URL.
    ///
    /// Releases the GIL during bind + accept. An empty host
    /// (`srt://:7000?mode=listener`) binds to `0.0.0.0`.
    #[staticmethod]
    fn from_url(py: Python<'_>, url: &str) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Listener {
            let msg = format!(
                "Receiver.from_url requires ?mode=listener; got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }
        let mut cfg = ListenerConfig::default();
        parsed.overlay.apply_to_listener(&mut cfg);
        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else if parsed.host.contains(':') && !parsed.host.starts_with('[') {
            format!("[{}]:{}", parsed.host, parsed.port)
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };
        let (transport, cancel) = py
            .allow_threads(|| -> Result<(SrtTransport, _), AcceptOrBindError> {
                let mut listener =
                    Listener::bind_with(&cfg, addr.as_str()).map_err(AcceptOrBindError::Bind)?;
                let (socket, _peer) = listener.accept().map_err(AcceptOrBindError::Accept)?;
                let transport = SrtTransport::new(socket);
                let cancel =
                    <SrtTransport as tst_core::transport::Transport>::cancel_handle(&transport)
                        .expect(
                            "SrtTransport with a live socket always returns Some(cancel_handle)",
                        );
                Ok((transport, cancel))
            })
            .map_err(|e| match e {
                AcceptOrBindError::Bind(e) => bind_error_to_pyerr(py, e),
                AcceptOrBindError::Accept(e) => accept_error_to_pyerr(py, e),
            })?;
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(inner))),
            cancel,
        })
    }

    /// Receive raw TS bytes from the underlying transport. Blocks until
    /// the first 188-byte TS packet arrives, then opportunistically
    /// drains the next-packet ring until it would overflow `max_len`
    /// — but does NOT re-block for additional packets after the first.
    ///
    /// This avoids the surprising semantic of "block until max_len
    /// bytes arrive": SRT live mode delivers in 188-byte (or 1316-byte
    /// bundle) units, and a caller asking for `max_len=1500` would
    /// hang indefinitely if the peer only sent one packet.
    ///
    /// Releases the GIL on the first (blocking) `next_packet` call.
    /// Returns a fresh `bytes` object whose length is a multiple of
    /// 188 (typically 188; up to `max_len // 188 * 188` when the SRT
    /// receive queue had more packets ready).
    ///
    /// Note: subsequent "opportunistic drain" calls today still go
    /// through the same blocking `next_packet` API — there is no
    /// non-blocking variant exposed by `Receiver`. For now we return
    /// after the first packet so the call is deterministic; richer
    /// drain semantics can land later if profiling shows value.
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let cap = max_len.max(188);
        // Receive one packet (188 bytes). Releases the GIL while
        // parked. SRT live mode delivers in 188-byte units, so a
        // single next_packet is the natural quantum. The inner lock is
        // held for the whole park; `close()` cancels before taking it.
        let inner = self.inner.clone();
        let pkt = py.allow_threads(move || {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_mut().map(|r| r.next_packet())
        });
        let pkt = pkt.ok_or_else(|| make_srt_error(py, "CLOSED", "receiver is closed"))?;
        let bytes = pkt.map_err(|e| match e.source {
            tst_pipeline::receiver::ReceiverErrorSource::Transport(t) => {
                transport_error_to_pyerr(py, t)
            }
            _ => make_srt_error(py, "IO", &e.to_string()),
        })?;
        let mut accumulated: Vec<u8> = Vec::with_capacity(cap);
        accumulated.extend_from_slice(&bytes);
        Ok(PyBytes::new_bound(py, &accumulated).unbind())
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in
    /// `.recv_bytes()`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(
            py,
            PyCancelHandle {
                inner: self.cancel.clone(),
                flag: AtomicBool::new(false),
            },
        )
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        // GIL released while waiting for the inner lock (a parked
        // recv_bytes holds it until data or a cancel arrives).
        let inner = self.inner.clone();
        let core = py.allow_threads(move || {
            let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|r| r.socket_stats().unwrap_or_default())
        });
        let core = core.ok_or_else(|| make_srt_error(py, "CLOSED", "receiver is closed"))?;
        Py::new(py, PySocketStats::from_core(core))
    }

    /// Snapshot of the SRT-rich 17-field stats.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let inner = self.inner.clone();
        let stats = py.allow_threads(move || {
            let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|r| r.transport().stats())
        });
        let stats = stats
            .ok_or_else(|| make_srt_error(py, "CLOSED", "receiver is closed"))?
            .map_err(|e| io_error_to_pyerr(py, e))?;
        Py::new(py, PySrtStats::from_srt(&stats))
    }

    /// Close the receiver. After close, further `.recv_bytes()` calls
    /// raise `SrtError(kind=CLOSED)`. Idempotent. Fires the cancel handle
    /// BEFORE acquiring the inner lock so a concurrent `recv_bytes()`
    /// parked on another thread unparks promptly (with `SrtError(BROKEN)`
    /// — the cancel closes the socket under it).
    fn close(&self, py: Python<'_>) {
        self.cancel.cancel();
        let inner = self.inner.clone();
        py.allow_threads(move || {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut r) = guard.take() {
                r.close();
            }
        });
    }

    /// `True` while the receiver owns a live transport.
    fn is_alive(&self) -> bool {
        match self.inner.try_lock() {
            Ok(g) => g.as_ref().is_some_and(|r| r.is_alive()),
            // Lock currently held by a parked recv_bytes — the receiver
            // is still alive (the parked recv hasn't released the
            // inner). Same optimistic shape as DemuxReceiver.
            Err(_) => true,
        }
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
        if self.is_alive() {
            "Receiver(open)".to_string()
        } else {
            "Receiver(closed)".to_string()
        }
    }
}

/// Internal helper for `Receiver::from_url` — combines bind + accept
/// failure paths inside one `allow_threads` block. Each variant maps
/// to a distinct user-visible `SrtErrorKind` in the outer match.
enum AcceptOrBindError {
    Bind(BindError),
    Accept(AcceptError),
}

// ---------------------------------------------------------------------------
// Cross-task helpers used by T3 (`lowlevel.rs`) to promote a low-level
// `Socket` / concrete `SrtCancelHandle` into the T2 PyClass shapes.
// ---------------------------------------------------------------------------

impl PySender {
    /// Build a `PySender` from a connected libsrt `Socket`. Used by
    /// `Socket::into_sender` (T3) so the Builder→Socket→Sender promotion
    /// path doesn't have to know about T2's internal field shape.
    pub(crate) fn from_socket(socket: Socket) -> Self {
        let transport = SrtTransport::new(socket);
        let inner = PlSender::new(transport, SenderConfig::default());
        let cancel = inner
            .cancel_handle()
            .expect("SrtTransport with a live socket always returns Some(cancel_handle)");
        Self {
            inner: Some(inner),
            cancel,
        }
    }
}

impl PyReceiver {
    /// Build a `PyReceiver` from a connected libsrt `Socket`. Used by
    /// `Socket::into_receiver` (T3). The caller has already done the
    /// accept (listener side) or completed the handshake (caller side).
    pub(crate) fn from_socket(socket: Socket) -> Self {
        let transport = SrtTransport::new(socket);
        let cancel = <SrtTransport as tst_core::transport::Transport>::cancel_handle(&transport)
            .expect("SrtTransport with a live socket always returns Some(cancel_handle)");
        let inner = PlReceiver::new(transport, ReceiverConfig::default());
        Self {
            inner: Arc::new(Mutex::new(Some(inner))),
            cancel,
        }
    }
}

impl PyCancelHandle {
    /// Build a `PyCancelHandle` from a concrete `SrtCancelHandle` (the
    /// type `Listener::cancel_handle()` returns directly). `SrtCancelHandle`
    /// is itself a `TransportCancel`, so it drops straight into the
    /// trait-erased slot the rest of the binding uses.
    pub(crate) fn from_concrete(inner: tst_core::SrtCancelHandle) -> Self {
        Self {
            inner: Arc::new(inner) as Arc<dyn TransportCancel + Send + Sync>,
            flag: AtomicBool::new(false),
        }
    }

    /// Build a `PyCancelHandle` from an already-trait-erased
    /// `Arc<dyn TransportCancel>`. Used by `PyDemuxReceiver::cancel_handle`
    /// (T5) where the pipeline shell already returns the trait-erased
    /// shape — no need to re-wrap.
    pub(crate) fn from_arc(inner: Arc<dyn TransportCancel + Send + Sync>) -> Self {
        Self {
            inner,
            flag: AtomicBool::new(false),
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
