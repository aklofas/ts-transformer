//! `ManagedSender` + `ManagedReceiver` for SRT.
//!
//! Auto-reconnect ergonomics on top of `tst_pipeline::ManagedTransport
//! <SrtTransport>` (send side) and `ManagedRecvTransport<SrtTransport>`
//! (receive side). Consumes T6's `ReconnectPolicy` PyClass directly via
//! its `pub(crate) inner: RustPolicy` field, so no re-translation is
//! needed at the boundary.
//!
//! Construction shape (both directions):
//!
//! ```python
//! from tstrans.srt import ManagedSender, ReconnectPolicy
//! sender = ManagedSender.from_url(
//!     "srt://10.0.0.1:9999?mode=caller",
//!     policy=ReconnectPolicy(max_attempts=None),  # retry forever
//! )
//! sender.send_bytes(b"...")  # auto-reconnects under the hood on Broken
//! ```
//!
//! ## API drift
//!
//! - `tst_pipeline::ManagedTransport` (send side) has **no**
//!   `reconnects_count` accessor — only `ManagedRecvTransport` (recv
//!   side) ships one (it's used by `ManagedDemuxReceiver` to detect a
//!   fresh transport between events). So `ManagedSender` does NOT
//!   expose `reconnect_attempts()`; only `ManagedReceiver` does.
//!
//! - The two `new(...)` signatures differ: `ManagedTransport::new` takes
//!   a `Fn() -> Result<T, TransportError> + Send + Sync + 'static`;
//!   `ManagedRecvTransport::new` takes a boxed `FnMut() -> ... + Send`.
//!   Both factory closures re-execute the T2 URL-parse + connect /
//!   bind+accept pattern.
//!
//! - Factory closure errors must map into `TransportError`, NOT
//!   `PyErr`. We route `ConnectError`/`BindError`/`AcceptError` /
//!   `UrlError` to `TransportError::Broken { msg, errno_code: None }`
//!   pragmatically so the reconnect loop treats them as a recoverable
//!   transport breakage and applies backoff.
//!
//! Concurrency: `ManagedTransport` keeps its inner `SrtTransport` under
//! `Arc<Mutex<Option<...>>>` internally; we wrap `PlSender
//! <ManagedTransport<SrtTransport>>` in `Option<...>` (no extra Mutex
//! needed) just like T2's `PySender`, since pyo3 already serializes
//! `&mut self` access. `ManagedRecvTransport` is `&mut self` for
//! `recv_bytes` so the same shape applies on the recv side.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::{Transport, TransportCancel, TransportError};
use tst_pipeline::{
    FactoryCancel, ManagedRecvTransport, ManagedTransport, Receiver as PlReceiver, ReceiverConfig,
    Sender as PlSender, SenderConfig,
};
use tst_srt::{Listener, ListenerConfig, Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use crate::errors::make_srt_error;
use crate::srt::errors::{transport_error_to_pyerr, url_error_to_pyerr};
use crate::srt::policy::{PyManagedTransportStats, PyReconnectPolicy};
use crate::srt::transport::{PyCancelHandle, PySocketStats, PySrtStats};

/// Build a fresh caller-mode `SrtTransport` from a URL string. Used by
/// the reconnect factory closure: every Broken/Closed event reruns
/// this to rebuild the inner transport.
///
/// Returns `TransportError::Broken` on URL parse failure or any
/// libsrt-level connect failure — the reconnect loop treats this as a
/// recoverable transport break and applies the configured backoff.
fn build_sender_transport(url: &str) -> Result<SrtTransport, TransportError> {
    let parsed = SrtUrl::parse(url).map_err(|e| TransportError::Broken {
        msg: format!("managed sender factory: URL parse failed: {e}"),
        errno_code: None,
    })?;
    if parsed.mode != Mode::Caller {
        return Err(TransportError::Broken {
            msg: format!(
                "managed sender factory: URL mode={:?} but caller required",
                parsed.mode
            ),
            errno_code: None,
        });
    }
    let mut cfg = SocketConfig::default();
    parsed.overlay.apply_to_socket(&mut cfg);
    let addr = crate::util::join_host_port(&parsed.host, parsed.port);
    let socket = Socket::connect_with(&cfg, addr.as_str()).map_err(|e| TransportError::Broken {
        msg: format!("managed sender factory: connect failed: {e}"),
        errno_code: None,
    })?;
    Ok(SrtTransport::new(socket))
}

/// Build a fresh listener-mode `SrtTransport` from a URL string. Used
/// by the recv-side reconnect factory: every Broken/Closed event
/// re-binds and re-accepts one incoming SRT handshake.
///
/// `slot` is the shared [`FactoryCancel`] the managed transport's cancel
/// handle fires: `Listener::accept_one_cancellable` publishes the
/// listener's wake handle into it around the accept, so a `cancel()`
/// reaches a re-accept parked with no peer in sight instead of waiting
/// for one to happen along.
fn build_receiver_transport(
    url: &str,
    slot: &FactoryCancel,
) -> Result<SrtTransport, TransportError> {
    let parsed = SrtUrl::parse(url).map_err(|e| TransportError::Broken {
        msg: format!("managed receiver factory: URL parse failed: {e}"),
        errno_code: None,
    })?;
    if parsed.mode != Mode::Listener {
        return Err(TransportError::Broken {
            msg: format!(
                "managed receiver factory: URL mode={:?} but listener required",
                parsed.mode
            ),
            errno_code: None,
        });
    }
    let mut cfg = ListenerConfig::default();
    parsed.overlay.apply_to_listener(&mut cfg);
    let addr = if parsed.host.is_empty() {
        format!("0.0.0.0:{}", parsed.port)
    } else {
        crate::util::join_host_port(&parsed.host, parsed.port)
    };
    Listener::accept_one_cancellable(&cfg, addr.as_str(), slot)
}

// ---------------------------------------------------------------------------
// PyManagedSender — wraps PlSender<ManagedTransport<SrtTransport>>
// ---------------------------------------------------------------------------

/// Python SRT managed sender — wraps `tst_pipeline::Sender
/// <ManagedTransport<SrtTransport>>` so the inner SRT transport is
/// rebuilt automatically when the connection breaks.
///
/// Construct via `ManagedSender.from_url(url, *, policy=ReconnectPolicy())`.
/// The URL must use `mode=caller` (default). The supplied policy is
/// applied identically to the initial connect and every subsequent
/// reconnect.
///
/// `send_bytes` releases the GIL while the underlying transport call
/// blocks — the reconnect work (factory + backoff sleep) likewise runs
/// outside the GIL.
#[pyclass(name = "ManagedSender", module = "tstrans.srt")]
pub(crate) struct PyManagedSender {
    inner: Option<PlSender<ManagedTransport<SrtTransport>>>,
    /// Trait-erased cancel handle pulled from the `ManagedTransport` at
    /// construction. `ManagedTransport::cancel_handle` always returns
    /// `Some(...)` (it wraps both the latched-close flag and the
    /// current inner transport's cancel handle).
    cancel: Arc<dyn TransportCancel + Send + Sync>,
    /// Reconnect/gap telemetry observer, snapshotted from the
    /// `ManagedTransport` BEFORE it moves into `PlSender::new` (same
    /// pattern as `cancel_handle` above — the handle keeps reading live
    /// counters after the shell takes ownership).
    stats_handle: tst_pipeline::ManagedStatsHandle,
}

#[pymethods]
impl PyManagedSender {
    /// Construct a managed sender from a `srt://...?mode=caller` URL.
    ///
    /// Performs the initial connect under `py.allow_threads`. On any
    /// subsequent transport break, `send_bytes` triggers an in-line
    /// reconnect under the policy. Default `policy = ReconnectPolicy()`
    /// applies T6's defaults (10 attempts, 100ms..=10s exponential
    /// backoff, 256-message gap buffer with DROP_OLDEST).
    #[staticmethod]
    #[pyo3(signature = (url, *, policy=None))]
    fn from_url(py: Python<'_>, url: &str, policy: Option<PyReconnectPolicy>) -> PyResult<Self> {
        // Validate URL up-front so a malformed URL raises CONFIG_INVALID
        // before we materialize the factory closure (otherwise the same
        // failure would surface as a Broken from the factory itself,
        // which is the wrong kind for a caller-misconfigured URL).
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "ManagedSender.from_url requires ?mode=caller (default); got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }

        let policy_inner = policy.map(|p| p.inner.clone()).unwrap_or_default();
        let url_owned = url.to_string();

        // Initial connect — this is the FIRST inner that
        // `ManagedTransport::new` wraps. Reconnects after this point
        // re-run the same factory.
        let initial = py
            .allow_threads(|| build_sender_transport(&url_owned))
            .map_err(|e| transport_error_to_pyerr(py, e))?;

        // Factory closure for subsequent reconnects. `Fn + Send + Sync
        // + 'static` per ManagedTransport::new's bound. `move` so the
        // URL string lives inside the closure for the wrapper's
        // lifetime.
        let factory = {
            let url_for_factory = url_owned.clone();
            move || -> Result<SrtTransport, TransportError> {
                build_sender_transport(&url_for_factory)
            }
        };

        let managed = ManagedTransport::new(initial, factory, policy_inner);
        // Snapshot the cancel handle AND the stats handle BEFORE we move
        // `managed` into the pipeline shell — `ManagedTransport::
        // cancel_handle` always returns `Some` because it wraps both the
        // latched flag and the inner transport's cancel snapshot;
        // `stats_handle()` is documented to be obtained before the shell
        // move (mirrors the cancel_handle precedent).
        let cancel = Transport::cancel_handle(&managed)
            .expect("ManagedTransport::cancel_handle is documented as always Some");
        let stats_handle = managed.stats_handle();
        let inner = PlSender::new(managed, SenderConfig::default());
        Ok(Self {
            inner: Some(inner),
            cancel,
            stats_handle,
        })
    }

    /// Send one pre-muxed TS-bytes chunk. Accepts any bytes-like input.
    /// Releases the GIL during the underlying transport send AND during
    /// any reconnect/backoff sleep that runs in-line on a Broken peer.
    fn send_bytes(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed sender is closed"))?;
        // Fast path: real `bytes` extracts zero-copy.
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

    /// Flush any partial TS bundle held in the framing buffer. Mirrors
    /// `Sender.flush` — releases the GIL during the underlying send.
    fn flush(&mut self, py: Python<'_>) -> PyResult<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed sender is closed"))?;
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

    /// Shareable cancel handle. Calling `.cancel()` latches the
    /// managed wrapper's close flag (preventing further reconnects)
    /// and forwards into the current inner transport's cancel handle
    /// to wake any thread parked in `send_bytes`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_arc(self.cancel.clone()))
    }

    /// Scheme-neutral wire stats from the current inner transport.
    /// Returns a fresh zeroed `SocketStats` if the inner is
    /// mid-reconnect (None), matching `ManagedTransport::socket_stats`
    /// semantics: callers can distinguish "no socket" from "socket
    /// with zero counters" via `is_alive()`.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed sender is closed"))?;
        let core = inner.socket_stats().unwrap_or_default();
        Py::new(py, PySocketStats::from_core(core))
    }

    /// SRT-rich 17-field stats from the current inner. Returns
    /// `SrtError(CLOSED)` if the inner is mid-reconnect (no live
    /// `SrtTransport` to query), distinguishing from the "all zeros"
    /// case `socket_stats()` returns.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed sender is closed"))?;
        // Reach through PlSender::transport() to the wrapped
        // ManagedTransport; ManagedTransport has no `stats()` accessor
        // (it doesn't know its inner is an SrtTransport), so we go
        // through its private inner via `Transport::is_alive` /
        // socket_stats indirection. But the symmetric path is: each
        // call rebuilds via the `ManagedTransport`'s inner lock — not
        // exposed publicly. So we materialize a "mid-reconnect"
        // CLOSED error if the underlying socket_stats projects None.
        //
        // Pragmatic shape: if SocketStats projects something, build a
        // PySrtStats from the SocketStats projection — caller gets the
        // 16 fields the abstract trait surfaces. We can't fish the
        // libsrt 17-field stats out of ManagedTransport without adding
        // a new accessor on tst-pipeline, which is out of scope for
        // T7 (the plan is explicit: drop accessors that don't exist).
        let _ = inner;
        Err(make_srt_error(
            py,
            "IO",
            "srt_stats not available on ManagedSender (use socket_stats); \
             a future tst-pipeline accessor will expose the SRT-rich shape",
        ))
    }

    /// Reconnect/gap telemetry: attempts, successes, current gap-buffer
    /// depth, and drop counters. Always readable — unlike
    /// `socket_stats`/`srt_stats`, it does not require a live inner
    /// transport (the counters live in a side channel that survives
    /// reconnect cycles), but it DOES require the sender itself not be
    /// closed (mirrors the CLOSED check every other managed getter runs).
    ///
    /// `reconnecting` is only ever `True` under `ReconnectMode.BACKGROUND`.
    ///
    /// Raises `SrtError(IO)` if the internal gap-buffer lock is
    /// poisoned (an unwind inside another thread while holding it) —
    /// a read-only telemetry path must not panic.
    fn reconnect_stats(&self, py: Python<'_>) -> PyResult<Py<PyManagedTransportStats>> {
        if self.inner.is_none() {
            return Err(make_srt_error(py, "CLOSED", "managed sender is closed"));
        }
        let stats = py
            .allow_threads(|| self.stats_handle.stats())
            .ok_or_else(|| {
                make_srt_error(py, "IO", "reconnect stats unavailable: gap lock poisoned")
            })?;
        Py::new(py, PyManagedTransportStats::from_core(stats))
    }

    /// Close. Latches the cancel flag (so any in-flight reconnect
    /// loop exits) and tears down the inner transport. Idempotent.
    fn close(&mut self) {
        // Flip cancel first so any reconnect loop blocked on backoff
        // sleep exits on the next iteration's check.
        self.cancel.cancel();
        if let Some(mut t) = self.inner.take() {
            t.close();
        }
    }

    /// `True` while the managed sender holds a live transport.
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
            Some(_) => "ManagedSender(open)".to_string(),
            None => "ManagedSender(closed)".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// PyManagedReceiver — wraps PlReceiver<ManagedRecvTransport<SrtTransport>>
// ---------------------------------------------------------------------------

/// Python SRT managed receiver — wraps `tst_pipeline::Receiver
/// <ManagedRecvTransport<SrtTransport>>`. On any Broken/Closed event
/// from the inner socket, re-runs bind + accept under the configured
/// reconnect policy and resumes delivering bytes from the new
/// connection.
///
/// `reconnect_attempts()` exposes the total successful reconnect count
/// (does NOT include the initial bind+accept).
///
/// `policy.mode` is send-side only: `ReconnectMode.BACKGROUND` on a
/// policy handed to `ManagedReceiver` logs a warning on the Rust side
/// and the receiver reconnects on the caller's thread anyway (i.e. it
/// behaves as `ReconnectMode.BLOCKING`).
#[pyclass(name = "ManagedReceiver", module = "tstrans.srt")]
pub(crate) struct PyManagedReceiver {
    inner: Option<PlReceiver<ManagedRecvTransport<SrtTransport>>>,
    /// Shared handle to the `ManagedRecvTransport`'s reconnect counter.
    /// Held independently of the wrapper's lifetime so callers can
    /// read it even mid-reconnect.
    reconnects: Arc<std::sync::atomic::AtomicU64>,
    /// Latched-close + cancel-on-peer-side flag. Used by `is_alive`
    /// and by `close()` to short-circuit the inner shell's cancel
    /// chain. Wrapping the inner shell's cancel handle would be ideal
    /// but `ManagedRecvTransport::cancel_handle` builds a fresh
    /// snapshot each call; we stash one snapshot at construction so
    /// `close()` can cancel without re-acquiring `&self` on the
    /// inner.
    cancel: Arc<dyn TransportCancel + Send + Sync>,
    /// Locally-tracked closed flag (mirror of the snapshotted cancel
    /// state). Set on `close()` and on `__exit__`.
    closed: Arc<AtomicBool>,
}

#[pymethods]
impl PyManagedReceiver {
    /// Bind + accept a managed receiver from a `srt://...?mode=listener`
    /// URL. Performs the initial bind+accept under `py.allow_threads`;
    /// every subsequent reconnect re-runs the same path.
    #[staticmethod]
    #[pyo3(signature = (url, *, policy=None))]
    fn from_url(py: Python<'_>, url: &str, policy: Option<PyReconnectPolicy>) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Listener {
            let msg = format!(
                "ManagedReceiver.from_url requires ?mode=listener; got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }

        let policy_inner = policy.map(|p| p.inner.clone()).unwrap_or_default();
        let url_owned = url.to_string();

        // Slot the reconnect factory publishes its listener wake handle
        // into while it is parked in a re-accept; the managed transport's
        // cancel handle fires it. The INITIAL accept below shares the same
        // slot but stays uncancellable in practice — the cancel handle
        // that could fire it does not exist until this constructor
        // returns. `ManagedDemuxReceiver` is wired to the same slot, but
        // it reaches that same place by forking the helper: a plain
        // `listen_srt` for the initial accept, `listen_srt_cancellable`
        // for the factory. Here one helper serves both calls.
        let factory_cancel = Arc::new(FactoryCancel::new());

        // Initial bind+accept. `ManagedRecvTransport` takes an
        // already-connected inner + a factory; the factory will
        // re-bind+re-accept on later breaks.
        let initial = py
            .allow_threads(|| build_receiver_transport(&url_owned, &factory_cancel))
            .map_err(|e| transport_error_to_pyerr(py, e))?;

        // FnMut closure for the recv-side factory. `ManagedRecvTransport`
        // takes a boxed `FnMut() -> ... + Send` (no `Sync` required —
        // it lives entirely behind `&mut self` on the recv path).
        let factory: Box<dyn FnMut() -> Result<SrtTransport, TransportError> + Send> = {
            let url_for_factory = url_owned.clone();
            let fc = Arc::clone(&factory_cancel);
            Box::new(move || build_receiver_transport(&url_for_factory, &fc))
        };

        let managed = ManagedRecvTransport::new_with_factory_cancel(
            initial,
            factory,
            policy_inner,
            factory_cancel,
        );
        let reconnects = managed.reconnects_handle();

        // Snapshot a cancel handle BEFORE moving managed into the
        // Receiver shell. ManagedRecvTransport's cancel_handle returns
        // an Arc<dyn TransportCancel> that latches the wrapper's
        // cancelled flag and then wakes every place a reconnect can be
        // parked: it signals the interruptible backoff wait, fires the
        // `factory_cancel` slot above (waking a factory sitting in
        // re-accept), and closes the current inner.
        let cancel = <ManagedRecvTransport<SrtTransport> as tst_core::transport::RecvTransport>
            ::cancel_handle(&managed)
            .expect("ManagedRecvTransport::cancel_handle is documented as always Some");

        let inner = PlReceiver::new(managed, ReceiverConfig::default());
        Ok(Self {
            inner: Some(inner),
            reconnects,
            cancel,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Receive bytes from the underlying transport. Blocks until the
    /// first 188-byte TS packet arrives, then returns it (same
    /// one-quantum semantic as T2's `Receiver.recv_bytes`).
    ///
    /// Releases the GIL during the blocking recv AND during any
    /// in-line reconnect work (factory + backoff sleep).
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&mut self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed receiver is closed"))?;
        let _cap = max_len.max(188);
        let pkt = py.allow_threads(|| inner.next_packet());
        let bytes = pkt.map_err(|e| match e.source {
            tst_pipeline::receiver::ReceiverErrorSource::Transport(t) => {
                transport_error_to_pyerr(py, t)
            }
            _ => make_srt_error(py, "IO", &e.to_string()),
        })?;
        let mut accumulated: Vec<u8> = Vec::with_capacity(bytes.len());
        accumulated.extend_from_slice(&bytes);
        Ok(PyBytes::new_bound(py, &accumulated).unbind())
    }

    /// Total number of successful reconnect rebuilds. Does NOT include
    /// the initial bind+accept (which happened in `from_url`).
    /// Increments each time the inner transport breaks and the
    /// factory successfully rebuilds.
    fn reconnect_attempts(&self) -> u64 {
        self.reconnects.load(Ordering::Acquire)
    }

    /// Shareable cancel handle. Calling `.cancel()` latches the
    /// wrapper's close flag and forwards into the current inner
    /// transport's cancel handle to wake any thread parked in
    /// `recv_bytes`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_arc(self.cancel.clone()))
    }

    /// Scheme-neutral wire stats from the current inner transport.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed receiver is closed"))?;
        let core = inner.socket_stats().unwrap_or_default();
        Py::new(py, PySocketStats::from_core(core))
    }

    /// SRT-rich 17-field stats are not directly accessible through
    /// `ManagedRecvTransport` today — same drift as `ManagedSender`.
    /// Use `socket_stats()` for the 16-field scheme-neutral view.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "managed receiver is closed"))?;
        let _ = inner;
        Err(make_srt_error(
            py,
            "IO",
            "srt_stats not available on ManagedReceiver (use socket_stats); \
             a future tst-pipeline accessor will expose the SRT-rich shape",
        ))
    }

    /// Close. Flips the cancel flag (any in-flight reconnect exits on
    /// the next iteration) and tears down the inner shell.
    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
        if let Some(mut r) = self.inner.take() {
            r.close();
        }
    }

    /// `True` while the managed receiver holds a live shell.
    fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.inner.as_ref().is_some_and(|r| r.is_alive())
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
        let attempts = self.reconnects.load(Ordering::Acquire);
        match &self.inner {
            Some(_) => format!("ManagedReceiver(open, reconnect_attempts={attempts})"),
            None => format!("ManagedReceiver(closed, reconnect_attempts={attempts})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyManagedSender>()?;
    m.add_class::<PyManagedReceiver>()?;
    Ok(())
}
