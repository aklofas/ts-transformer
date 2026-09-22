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
//!   `UrlError` to `TransportError::Broken { msg, errno_code: None, cause: BrokenCause::Unspecified }`
//!   pragmatically so the reconnect loop treats them as a recoverable
//!   transport breakage and applies backoff.
//!
//! Concurrency: both wrappers hold their pipeline shell in a
//! `tst_pipeline::binding::Owned`, which takes the slot only inside
//! `with_mut` / `with_ref` (GIL released) and makes `close()` cancel-first
//! — the cross-thread close contract of PR #209, which a `&mut self`
//! send/recv could not meet (PyO3 raised `RuntimeError: Already borrowed`
//! on the closer).
//!
//! The open, the reconnect factory and the handle snapshots all live in
//! `tst_srt::shells` since Arc 2 (the binding used to carry its own copy
//! of each).

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_pipeline::binding::{BindingError, BindingErrorKind, HandleState, Owned};
use tst_pipeline::{
    ManagedRecvTransport, ManagedTransport, Receiver as PlReceiver, Sender as PlSender,
    SenderConfig,
};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::errors::make_srt_error;
use crate::raise::{SRT, pyok, pyres, raise};
use crate::srt::policy::{PyManagedTransportStats, PyReconnectPolicy};
use crate::srt::transport::{PyCancelHandle, PySocketStats, PySrtStats};
use crate::util::{CancelSource, alive_probe, close_owned};

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
    /// Shared slot — see the module doc; `close()` cancels before taking it.
    owned: Owned<PlSender<ManagedTransport<SrtTransport>>>,
    /// Trait-erased cancel handle pulled from the `ManagedTransport` at
    /// construction. `ManagedTransport::cancel_handle` always returns
    /// `Some(...)` (it wraps both the latched-close flag and the
    /// current inner transport's cancel handle).
    /// Shared cancel state (Arc 2 WP-B2): the same `Arc` every
    /// `CancelHandle` this shell hands out holds, so `close()` here and
    /// `cancel()` through any handle flip one observable flag.
    cancel: Arc<CancelSource>,
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
        // Validate the URL up-front so a malformed one raises
        // CONFIG_INVALID naming the Python method; `tst_srt::shells` would
        // refuse a listener URL too, but with its own message.
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Caller {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "ManagedSender.from_url requires ?mode=caller (default); got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        let policy_inner = policy.map(|p| p.inner.clone()).unwrap_or_default();
        // A3 owns the open + the reconnect factory + the handle snapshots
        // (the binding used to carry its own copy of each). The managed
        // family dials with `connect()` — the sender preset — matching the
        // C ABI.
        let (inner, handles, stats_handle) = py
            .allow_threads(|| {
                tst_srt::shells::managed_sender_from_url(
                    &parsed,
                    policy_inner,
                    SenderConfig::default(),
                )
            })
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        let cancel = CancelSource::new(handles.cancel);
        Ok(Self {
            owned: Owned::new(inner, cancel.as_dyn(), ()),
            cancel,
            stats_handle,
        })
    }

    /// Send one pre-muxed TS-bytes chunk. Accepts any bytes-like input.
    /// Releases the GIL during the underlying transport send AND during
    /// any reconnect/backoff wait that runs in-line on a Broken peer. A
    /// concurrent `close()` cancels first, so a send parked in the
    /// reconnect loop ends with `SrtError(CLOSED)`.
    fn send_bytes(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice: &[u8] = coerced.as_bytes();
        pyres(
            py,
            &SRT,
            py.allow_threads(|| self.owned.with_mut(|s| s.send_ts(slice))),
        )
    }

    /// Flush any partial TS bundle held in the framing buffer. Mirrors
    /// `Sender.flush` — releases the GIL during the underlying send.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        pyres(
            py,
            &SRT,
            py.allow_threads(|| self.owned.with_mut(|s| s.flush())),
        )
    }

    /// Shareable cancel handle. Calling `.cancel()` latches the
    /// managed wrapper's close flag (preventing further reconnects)
    /// and forwards into the current inner transport's cancel handle
    /// to wake any thread parked in `send_bytes`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Scheme-neutral wire stats from the current inner transport.
    /// Returns a fresh zeroed `SocketStats` if the inner is
    /// mid-reconnect (None), matching `ManagedTransport::socket_stats`
    /// semantics. Waits (GIL released) for a send in flight elsewhere.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|s| s.socket_stats().unwrap_or_default())
            }),
        )?;
        Py::new(py, PySocketStats::from_core(core))
    }

    /// SRT-rich 17-field stats are not reachable through `ManagedTransport`
    /// (no accessor on tst-pipeline — see the module doc's "API drift");
    /// raises `SrtError(IO)` on a live sender and `SrtError(CLOSED)` on a
    /// closed one, as before.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        if self.owned.is_closed() {
            return Err(raise(py, &SRT, BindingError::from(HandleState::Closed)));
        }
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
        if self.owned.is_closed() {
            return Err(raise(py, &SRT, BindingError::from(HandleState::Closed)));
        }
        let stats = py
            .allow_threads(|| self.stats_handle.stats())
            .ok_or_else(|| {
                make_srt_error(py, "IO", "reconnect stats unavailable: gap lock poisoned")
            })?;
        Py::new(py, PyManagedTransportStats::from_core(stats))
    }

    /// Close. Fires the managed cancel FIRST (latches the close flag so any
    /// in-flight reconnect loop — factory or backoff wait — exits with
    /// `TransportError::Closed`, and closes the current inner socket),
    /// then takes the slot and tears the shell down. A `send_bytes()`
    /// parked on another thread ends with `SrtError(CLOSED)`. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the managed sender holds a live transport (a send in
    /// flight on another thread counts as live).
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
            "ManagedSender(closed)".to_string()
        } else {
            "ManagedSender(open)".to_string()
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
    /// Shared slot — see the module doc; `close()` cancels before taking it.
    owned: Owned<PlReceiver<ManagedRecvTransport<SrtTransport>>>,
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
    /// Shared cancel state (Arc 2 WP-B2): the same `Arc` every
    /// `CancelHandle` this shell hands out holds, so `close()` here and
    /// `cancel()` through any handle flip one observable flag.
    cancel: Arc<CancelSource>,
}

#[pymethods]
impl PyManagedReceiver {
    /// Bind + accept a managed receiver from a `srt://...?mode=listener`
    /// URL. Performs the initial bind+accept under `py.allow_threads`;
    /// every subsequent reconnect re-runs the same path.
    #[staticmethod]
    #[pyo3(signature = (url, *, policy=None))]
    fn from_url(py: Python<'_>, url: &str, policy: Option<PyReconnectPolicy>) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Listener {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "ManagedReceiver.from_url requires ?mode=listener; got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        let policy_inner = policy.map(|p| p.inner.clone()).unwrap_or_default();
        // A3 owns the bind+accept, the re-accept factory, the
        // `FactoryCancel` slot that wakes a parked re-accept, and the
        // handle snapshots. The INITIAL accept is still uncancellable in
        // practice: the handle that could fire it does not exist until
        // this constructor returns (DEBT-16, documented in python.md).
        let (inner, handles) = py
            .allow_threads(|| tst_srt::shells::managed_receiver_from_url(&parsed, policy_inner))
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        let reconnects = handles.reconnects.clone();
        let cancel = CancelSource::new(handles.cancel);
        Ok(Self {
            owned: Owned::new(inner, cancel.as_dyn(), ()),
            reconnects,
            cancel,
        })
    }

    /// Receive bytes from the underlying transport. Blocks until the
    /// first 188-byte TS packet arrives, then returns it (same
    /// one-quantum semantic as T2's `Receiver.recv_bytes`).
    ///
    /// Releases the GIL during the blocking recv AND during any
    /// in-line reconnect work (factory + backoff sleep). The slot is
    /// held for the whole park; `close()` cancels before taking it, so a
    /// parked call ends with `SrtError(CLOSED)`.
    #[pyo3(signature = (max_len = 1500))]
    fn recv_bytes(&self, py: Python<'_>, max_len: usize) -> PyResult<Py<PyBytes>> {
        let _cap = max_len.max(188);
        let bytes = pyres(
            py,
            &SRT,
            py.allow_threads(|| self.owned.with_mut(|r| r.next_packet())),
        )?;
        Ok(PyBytes::new_bound(py, &bytes).unbind())
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
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Scheme-neutral wire stats from the current inner transport. Waits
    /// (GIL released) for a recv parked on another thread.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        let core = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|r| r.socket_stats().unwrap_or_default())
            }),
        )?;
        Py::new(py, PySocketStats::from_core(core))
    }

    /// SRT-rich 17-field stats are not directly accessible through
    /// `ManagedRecvTransport` today — same drift as `ManagedSender`.
    /// Use `socket_stats()` for the 16-field scheme-neutral view.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySrtStats>> {
        if self.owned.is_closed() {
            return Err(raise(py, &SRT, BindingError::from(HandleState::Closed)));
        }
        Err(make_srt_error(
            py,
            "IO",
            "srt_stats not available on ManagedReceiver (use socket_stats); \
             a future tst-pipeline accessor will expose the SRT-rich shape",
        ))
    }

    /// Close. Flips the local closed flag, fires the managed cancel (any
    /// in-flight receive or reconnect exits with `TransportError::Closed`),
    /// then takes the slot and tears the shell down. A `recv_bytes()`
    /// parked on another thread ends with `SrtError(CLOSED)`. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the managed receiver holds a live shell (a recv parked
    /// on another thread counts as live).
    fn is_alive(&self) -> bool {
        !self.cancel.is_cancelled() && alive_probe(&self.owned, |r| r.is_alive())
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
        let attempts = self.reconnects.load(Ordering::Acquire);
        if self.owned.is_closed() {
            format!("ManagedReceiver(closed, reconnect_attempts={attempts})")
        } else {
            format!("ManagedReceiver(open, reconnect_attempts={attempts})")
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
