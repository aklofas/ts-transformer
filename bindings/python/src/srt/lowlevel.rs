//! `Builder`, `Socket`, `Listener` (the low-level SRT primitives).
//!
//! Layered atop T2:
//! - `Builder` is a hybrid fluent + kwargs SRT URL constructor (Q3).
//!   URL-provided values WIN over kwargs (Q4-A) — we accumulate kwargs
//!   into a `SocketConfig` / `ListenerConfig` FIRST, then call
//!   `UrlOverlay::apply_to_{socket,listener}` AFTER so the overlay's
//!   unconditional overwrites give the URL final say.
//! - `Socket` is a handle that promotes via `into_sender` /
//!   `into_receiver` into T2's PySender / PyReceiver, and via
//!   `into_mux_sender` / `into_demux_receiver` into T5's PyMuxSender /
//!   PyDemuxReceiver. Each consumes the socket handle.
//! - `Listener` exposes both blocking `accept(timeout_ms=...)` and a
//!   Python iterator (`for sock in listener: ...`). The iterator
//!   converts `AcceptError::ListenerClosed` to `StopIteration` so a
//!   `cancel()` call from another thread closes the loop cleanly.
//!
//! Error mapping for `UrlError`/`ConnectError`/`BindError`/`AcceptError`/
//! `IoError` reuses the canonical mappers in `srt/errors.rs` (the coverage
//! ratchet greps the whole `src/` directory, not per-file).

#![allow(
    unsafe_op_in_unsafe_fn,
    clippy::useless_conversion,
    // PyO3 `into_*` methods take `&self` (the slot is a `Mutex<Option<_>>`;
    // the consumption is enforced via `Option::take`) — clippy still flags
    // the `into_` prefix on a borrowing receiver.
    clippy::wrong_self_convention,
    // PyO3 #[new] constructors accept kwargs as positional Rust
    // parameters; this surface has more than 7 user-facing kwargs.
    clippy::too_many_arguments
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::Py;
use pyo3::exceptions::PyStopIteration;
use pyo3::prelude::*;

use tst_srt::error::AcceptError;
use tst_srt::options::{Congestion, MaxBandwidth, Passphrase, StreamId};
use tst_srt::{
    Listener as SrtListener, ListenerConfig, Socket as SrtSocket, SocketConfig, SrtTransport,
    SrtUrl, url::Mode,
};

use tst_pipeline::binding::{BindingError, HandleState, Owned};

use crate::errors::make_srt_error;
use crate::raise::{SRT, pyres, raise};
use crate::srt::errors::{
    bind_error_to_pyerr, connect_error_to_pyerr, io_error_to_pyerr, url_error_to_pyerr,
};
use crate::srt::transport::{PyCancelHandle, PyReceiver, PySender};
use crate::util::{alive_probe, close_owned};

// ---------------------------------------------------------------------------
// Builder mode tracking
// ---------------------------------------------------------------------------

/// Tri-state mode flag for the Builder.
///
/// `Rendezvous` doesn't have a backing `Mode` variant in `tst_srt::url`
/// — there's just `Caller` and `Listener`. We track it locally so
/// `.rendezvous()` is callable for forward-compat, then raise
/// `CONFIG_INVALID` at the finalize step. Forward-compat: when libsrt
/// rendezvous is wired through `tst_srt`, this enum gets the third
/// concrete arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuilderMode {
    /// Honor whatever mode the URL parsed to (default).
    UrlChoice,
    /// Caller mode explicitly chosen via `.caller()`. URL must agree.
    Caller,
    /// Listener mode explicitly chosen via `.listener()`. URL must agree.
    Listener,
    /// Rendezvous mode requested via `.rendezvous()`. Not yet supported
    /// — finalize raises `CONFIG_INVALID`.
    Rendezvous,
}

// ---------------------------------------------------------------------------
// PyBuilder — hybrid fluent + kwargs SRT URL constructor
// ---------------------------------------------------------------------------

/// Fluent + kwargs SRT URL constructor.
///
/// Holds:
/// - The original URL string (parsed lazily at finalize).
/// - An accumulated SocketConfig + ListenerConfig built from kwargs
///   and chainable setters.
/// - A mode override (`UrlChoice` by default; `caller()` / `listener()`
///   / `rendezvous()` override it).
///
/// At finalize (`connect()` / `listen()`):
/// 1. Parse the URL.
/// 2. If mode override is set and disagrees with URL → `CONFIG_INVALID`.
/// 3. Overlay URL parameters on top of the accumulated config — URL wins
///    on conflict (per `UrlOverlay::apply_to_*` semantics: unconditional
///    overwrite).
/// 4. Call `Socket::connect_with` / `Listener::bind_with`.
///
/// All setters mutate-in-place and return `self` so chains work:
/// `Builder(url).caller().latency_ms(200).passphrase(p).connect()`.
#[pyclass(name = "Builder", module = "tstrans.srt")]
pub(crate) struct PyBuilder {
    url: String,
    mode_override: BuilderMode,
    /// Eagerly mutated by each setter. We carry both shapes so
    /// `.connect()` and `.listen()` can pick the right one without
    /// re-running the kwarg accumulation.
    socket_cfg: SocketConfig,
    listener_cfg: ListenerConfig,
    /// Plain-text passphrase used only by `__repr__` redaction (so
    /// callers can sanity-check the kwarg landed without leaking the
    /// secret). The real passphrase lives in `socket_cfg.passphrase` /
    /// `listener_cfg.passphrase` (already `SecretString`-wrapped).
    passphrase_set: bool,
}

impl PyBuilder {
    /// Apply a passphrase to BOTH configs. We translate at the kwarg
    /// boundary so callers never see `tst_srt::Passphrase` from Python.
    /// Validation errors collapse to `CONFIG_INVALID`.
    fn apply_passphrase(&mut self, py: Python<'_>, p: &str) -> PyResult<()> {
        let pp = Passphrase::new(p.to_string())
            .map_err(|e| make_srt_error(py, "CONFIG_INVALID", &e.to_string()))?;
        self.socket_cfg.passphrase = Some(pp.clone());
        self.listener_cfg.passphrase = Some(pp);
        self.passphrase_set = true;
        Ok(())
    }

    fn apply_stream_id(&mut self, py: Python<'_>, s: &str) -> PyResult<()> {
        let id = StreamId::new(s.to_string())
            .map_err(|e| make_srt_error(py, "CONFIG_INVALID", &e.to_string()))?;
        self.socket_cfg.stream_id = Some(id);
        // ListenerConfig doesn't carry stream_id (set on accepted sockets
        // via post-handshake observation). Caller-side only.
        Ok(())
    }

    fn apply_congestion(&mut self, py: Python<'_>, name: &str) -> PyResult<()> {
        let c = Congestion::from_str_strict(name)
            .map_err(|e| make_srt_error(py, "CONFIG_INVALID", &e.to_string()))?;
        self.socket_cfg.congestion = Some(c);
        self.listener_cfg.congestion = Some(c);
        Ok(())
    }
}

#[pymethods]
impl PyBuilder {
    /// Construct a new Builder. Common knobs accepted as kwargs.
    #[new]
    #[pyo3(signature = (
        url,
        *,
        latency_ms = None,
        passphrase = None,
        stream_id = None,
        congestion = None,
        connect_timeout_ms = None,
        recv_timeout_ms = None,
        send_timeout_ms = None,
    ))]
    fn new(
        py: Python<'_>,
        url: &str,
        latency_ms: Option<u32>,
        passphrase: Option<&str>,
        stream_id: Option<&str>,
        congestion: Option<&str>,
        connect_timeout_ms: Option<u32>,
        recv_timeout_ms: Option<u32>,
        send_timeout_ms: Option<u32>,
    ) -> PyResult<Self> {
        let mut b = Self {
            url: url.to_string(),
            mode_override: BuilderMode::UrlChoice,
            socket_cfg: SocketConfig::default(),
            listener_cfg: ListenerConfig::default(),
            passphrase_set: false,
        };
        if let Some(ms) = latency_ms {
            let d = Duration::from_millis(ms.into());
            b.socket_cfg.latency = Some(d);
            b.listener_cfg.latency = Some(d);
        }
        if let Some(p) = passphrase {
            b.apply_passphrase(py, p)?;
        }
        if let Some(s) = stream_id {
            b.apply_stream_id(py, s)?;
        }
        if let Some(c) = congestion {
            b.apply_congestion(py, c)?;
        }
        if let Some(ms) = connect_timeout_ms {
            b.socket_cfg.connect_timeout = Some(Duration::from_millis(ms.into()));
        }
        if let Some(ms) = recv_timeout_ms {
            let d = Duration::from_millis(ms.into());
            b.socket_cfg.recv_timeout = Some(d);
            b.listener_cfg.recv_timeout = Some(d);
        }
        if let Some(ms) = send_timeout_ms {
            let d = Duration::from_millis(ms.into());
            b.socket_cfg.send_timeout = Some(d);
            b.listener_cfg.send_timeout = Some(d);
        }
        Ok(b)
    }

    // --- Mode setters ---

    /// Mark this builder as caller-mode. Chainable.
    fn caller(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.mode_override = BuilderMode::Caller;
        slf
    }

    /// Mark this builder as listener-mode. Chainable.
    fn listener(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.mode_override = BuilderMode::Listener;
        slf
    }

    /// Mark this builder as rendezvous-mode.
    ///
    /// **Not yet supported by tst-srt** — `connect()` will raise
    /// `SrtError(CONFIG_INVALID)`. The setter is provided for forward
    /// compatibility so chain code doesn't need to change when libsrt
    /// rendezvous lands in the lower crates.
    fn rendezvous(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.mode_override = BuilderMode::Rendezvous;
        slf
    }

    // --- Knob setters (chainable) ---

    fn latency_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        let d = Duration::from_millis(ms.into());
        slf.socket_cfg.latency = Some(d);
        slf.listener_cfg.latency = Some(d);
        slf
    }

    fn passphrase<'py>(
        mut slf: PyRefMut<'py, Self>,
        py: Python<'py>,
        p: &str,
    ) -> PyResult<PyRefMut<'py, Self>> {
        slf.apply_passphrase(py, p)?;
        Ok(slf)
    }

    fn stream_id<'py>(
        mut slf: PyRefMut<'py, Self>,
        py: Python<'py>,
        s: &str,
    ) -> PyResult<PyRefMut<'py, Self>> {
        slf.apply_stream_id(py, s)?;
        Ok(slf)
    }

    fn congestion<'py>(
        mut slf: PyRefMut<'py, Self>,
        py: Python<'py>,
        name: &str,
    ) -> PyResult<PyRefMut<'py, Self>> {
        slf.apply_congestion(py, name)?;
        Ok(slf)
    }

    fn connect_timeout_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        slf.socket_cfg.connect_timeout = Some(Duration::from_millis(ms.into()));
        slf
    }

    fn recv_timeout_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        let d = Duration::from_millis(ms.into());
        slf.socket_cfg.recv_timeout = Some(d);
        slf.listener_cfg.recv_timeout = Some(d);
        slf
    }

    fn send_timeout_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        let d = Duration::from_millis(ms.into());
        slf.socket_cfg.send_timeout = Some(d);
        slf.listener_cfg.send_timeout = Some(d);
        slf
    }

    fn peer_latency_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        slf.socket_cfg.peer_latency = Some(Duration::from_millis(ms.into()));
        // ListenerConfig has no peer_latency (libsrt sets it on caller side).
        slf
    }

    fn recv_latency_ms(mut slf: PyRefMut<'_, Self>, ms: u32) -> PyRefMut<'_, Self> {
        let d = Duration::from_millis(ms.into());
        slf.socket_cfg.recv_latency = Some(d);
        slf.listener_cfg.recv_latency = Some(d);
        slf
    }

    fn max_bandwidth_bps(mut slf: PyRefMut<'_, Self>, bps: u64) -> PyRefMut<'_, Self> {
        let mb = MaxBandwidth::Limited(bps);
        slf.socket_cfg.max_bandwidth = Some(mb);
        slf.listener_cfg.max_bandwidth = Some(mb);
        slf
    }

    fn mss(mut slf: PyRefMut<'_, Self>, value: u16) -> PyRefMut<'_, Self> {
        slf.socket_cfg.mss = Some(value);
        slf.listener_cfg.mss = Some(value);
        slf
    }

    fn payload_size(mut slf: PyRefMut<'_, Self>, value: u16) -> PyRefMut<'_, Self> {
        slf.socket_cfg.payload_size = Some(value);
        slf.listener_cfg.payload_size = Some(value);
        slf
    }

    // --- Finalizers ---

    /// Resolve the builder to a connected `Socket`. Mode must be
    /// `caller` (either via `.caller()` or via URL default).
    /// Releases the GIL during the SRT handshake.
    fn connect(&self, py: Python<'_>) -> PyResult<PySocket> {
        if matches!(self.mode_override, BuilderMode::Rendezvous) {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                "rendezvous mode is not yet supported by tst-srt",
            ));
        }
        if matches!(self.mode_override, BuilderMode::Listener) {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                "Builder.connect() requires caller mode (mode_override is Listener)",
            ));
        }
        let parsed = SrtUrl::parse(&self.url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Caller {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                &format!(
                    "Builder.connect() requires URL mode=caller (default); got mode={:?}",
                    parsed.mode
                ),
            ));
        }
        // Apply kwargs FIRST then URL overlay AFTER — overlay does
        // unconditional overwrite, so URL wins on conflict (Q4-A).
        let mut cfg = self.socket_cfg.clone();
        parsed.overlay.apply_to_socket(&mut cfg);
        let addr = crate::util::join_host_port(&parsed.host, parsed.port);
        let socket = py
            .allow_threads(|| SrtSocket::connect_with(&cfg, addr.as_str()))
            .map_err(|e| connect_error_to_pyerr(py, e))?;
        Ok(PySocket::wrap(socket))
    }

    /// Resolve the builder to a bound `Listener`. Mode must be
    /// `listener` (either via `.listener()` or via URL `?mode=listener`).
    /// Releases the GIL during `srt_bind` + `srt_listen`.
    fn listen(&self, py: Python<'_>) -> PyResult<PyListener> {
        if matches!(self.mode_override, BuilderMode::Rendezvous) {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                "rendezvous mode is not yet supported by tst-srt",
            ));
        }
        if matches!(self.mode_override, BuilderMode::Caller) {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                "Builder.listen() requires listener mode (mode_override is Caller)",
            ));
        }
        let parsed = SrtUrl::parse(&self.url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Listener {
            return Err(make_srt_error(
                py,
                "CONFIG_INVALID",
                &format!(
                    "Builder.listen() requires URL ?mode=listener; got mode={:?}",
                    parsed.mode
                ),
            ));
        }
        let mut cfg = self.listener_cfg.clone();
        parsed.overlay.apply_to_listener(&mut cfg);
        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else {
            crate::util::join_host_port(&parsed.host, parsed.port)
        };
        let listener = py
            .allow_threads(|| SrtListener::bind_with(&cfg, addr.as_str()))
            .map_err(|e| bind_error_to_pyerr(py, e))?;
        Ok(PyListener::wrap(listener))
    }

    fn __repr__(&self) -> String {
        let pp = if self.passphrase_set {
            "<redacted>"
        } else {
            "None"
        };
        let mode = match self.mode_override {
            BuilderMode::UrlChoice => "url",
            BuilderMode::Caller => "caller",
            BuilderMode::Listener => "listener",
            BuilderMode::Rendezvous => "rendezvous",
        };
        format!(
            "Builder(url={:?}, mode={}, passphrase={})",
            self.url, mode, pp
        )
    }
}

// ---------------------------------------------------------------------------
// PySocket — handle that promotes via into_sender / into_receiver
// ---------------------------------------------------------------------------

/// Low-level SRT socket handle. Returned by `Builder.connect()` and
/// `Listener.accept()`.
///
/// Held by reference until consumed via `into_sender()` /
/// `into_receiver()` (each consumes the handle; the underlying
/// `tst_srt::Socket` moves into the new wrapper). `close()` is a manual
/// teardown; the destructor closes too.
#[pyclass(name = "Socket", module = "tstrans.srt")]
pub(crate) struct PySocket {
    /// `&self` everywhere (cross-thread `close()` / getters never trip
    /// PyO3's borrow check); a plain `Mutex` suffices because no `Socket`
    /// method releases the GIL — nothing ever holds this lock across a
    /// park. `Option` so the `into_*` promotions and `close()` can take
    /// the handle.
    inner: Mutex<Option<SrtSocket>>,
}

impl PySocket {
    pub(crate) fn wrap(socket: SrtSocket) -> Self {
        Self {
            inner: Mutex::new(Some(socket)),
        }
    }

    /// Take the libsrt handle for a consuming promotion; `CLOSED` once
    /// it has been consumed or closed.
    fn take_socket(&self, py: Python<'_>) -> PyResult<SrtSocket> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "socket is closed"))
    }
}

#[pymethods]
impl PySocket {
    /// Consume this socket and produce a `Sender`. The socket handle
    /// moves into the new Sender — subsequent calls on `self` raise
    /// `SrtError(kind=CLOSED)`.
    fn into_sender(&self, py: Python<'_>) -> PyResult<PySender> {
        Ok(PySender::from_transport(SrtTransport::new(
            self.take_socket(py)?,
        )))
    }

    /// Consume this socket and produce a `Receiver`. Same consumption
    /// semantics as `into_sender`.
    fn into_receiver(&self, py: Python<'_>) -> PyResult<PyReceiver> {
        Ok(PyReceiver::from_transport(SrtTransport::new(
            self.take_socket(py)?,
        )))
    }

    /// Consume this socket and produce a `MuxSender` for the given
    /// single-program `MuxerProgramConfig`. The socket handle moves into
    /// the new MuxSender; subsequent calls on `self` raise
    /// `SrtError(kind=CLOSED)`.
    ///
    /// Raises `MuxError(CONFIG_INVALID)` if the program config fails the
    /// muxer's validation.
    fn into_mux_sender(
        &self,
        py: Python<'_>,
        program_config: PyRef<'_, crate::mux::PyMuxerProgramConfig>,
    ) -> PyResult<crate::srt::mux_sender::PyMuxSender> {
        let socket = self.take_socket(py)?;
        crate::srt::mux_sender::PyMuxSender::from_pipeline_mux(py, socket, &program_config)
    }

    /// Consume this socket and produce a `DemuxReceiver`. Optional
    /// `demux_config` is a `tstrans.mpegts.DemuxerConfig` dataclass; if
    /// `None`, defaults are used. Same consumption semantics as
    /// `into_mux_sender`.
    #[pyo3(signature = (*, demux_config = None))]
    fn into_demux_receiver(
        &self,
        py: Python<'_>,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<crate::srt::demux_receiver::PyDemuxReceiver> {
        let socket = self.take_socket(py)?;
        match demux_config {
            None => Ok(crate::srt::demux_receiver::PyDemuxReceiver::from_pipeline_demux(socket)),
            Some(cfg) => {
                let opts = crate::mpegts::build_demuxer_config(py, cfg)?;
                Ok(
                    crate::srt::demux_receiver::PyDemuxReceiver::from_pipeline_demux_with_config(
                        socket, opts,
                    ),
                )
            }
        }
    }

    /// Local bound address as `(host, port)`. Useful when the URL
    /// requested port 0 (kernel-pick).
    fn local_addr(&self, py: Python<'_>) -> PyResult<(String, u16)> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let socket = guard
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "socket is closed"))?;
        let addr = socket.local_addr().map_err(|e| io_error_to_pyerr(py, e))?;
        Ok((addr.ip().to_string(), addr.port()))
    }

    /// Peer address as `(host, port)`. Errors if the socket isn't
    /// connected (e.g., a fresh bind without accept).
    fn peer_addr(&self, py: Python<'_>) -> PyResult<(String, u16)> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let socket = guard
            .as_ref()
            .ok_or_else(|| make_srt_error(py, "CLOSED", "socket is closed"))?;
        let addr = socket.peer_addr().map_err(|e| io_error_to_pyerr(py, e))?;
        Ok((addr.ip().to_string(), addr.port()))
    }

    /// Stream ID negotiated at handshake, if any.
    fn stream_id(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.stream_id().map(String::from))
    }

    /// Close the socket. Subsequent `into_sender` / `into_receiver`
    /// calls raise `SrtError(kind=CLOSED)`. Idempotent; safe from any
    /// thread.
    fn close(&self) {
        let taken = self.inner.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(socket) = taken {
            // SrtSocket::close consumes self and is documented as always-Ok.
            let _ = socket.close();
        }
    }

    /// `True` while this socket still owns the libsrt handle.
    fn is_alive(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        if self.is_alive() {
            "Socket(open)".to_string()
        } else {
            "Socket(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyListener — bound listening socket, accept loop + iterator
// ---------------------------------------------------------------------------

/// Bound SRT listener. Iterate to consume accepted Sockets, or call
/// `accept(timeout_ms=...)` for explicit per-accept control.
///
/// The iterator stops cleanly when `cancel_handle().cancel()` is called
/// from another thread — `AcceptError::ListenerClosed` maps to
/// `StopIteration` in `__next__`. Other accept errors propagate as
/// `SrtError`.
/// `tst_srt::Listener` behind the binding layer's `Close`: `close(self)`
/// consumes the listener, so the slot holds an `Option` it can take.
pub(crate) struct ListenerHeld(Option<SrtListener>);

impl ListenerHeld {
    fn get(&mut self) -> Result<&mut SrtListener, AcceptError> {
        self.0.as_mut().ok_or(AcceptError::ListenerClosed)
    }
}

impl tst_pipeline::binding::Close for ListenerHeld {
    type Error = tst_srt::error::IoError;

    fn close(&mut self) -> Result<(), Self::Error> {
        match self.0.take() {
            Some(l) => l.close(),
            None => Ok(()),
        }
    }
}

#[pyclass(name = "Listener", module = "tstrans.srt")]
pub(crate) struct PyListener {
    /// Snapshot = the bound address read at construction (never waits
    /// behind a parked `accept()` — the thread asking for the port is
    /// usually the one that has to connect to end that park); `None` only
    /// if `getsockname` failed.
    owned: Owned<ListenerHeld, Option<SocketAddr>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<crate::util::CancelSource>,
}

impl PyListener {
    pub(crate) fn wrap(listener: SrtListener) -> Self {
        let cancel = crate::util::CancelSource::new(Arc::new(listener.cancel_handle()));
        let local_addr = listener.local_addr().ok();
        Self {
            owned: Owned::new(ListenerHeld(Some(listener)), cancel.as_dyn(), local_addr),
            cancel,
        }
    }
}

#[pymethods]
impl PyListener {
    /// Block until an incoming peer completes the SRT handshake, then
    /// return the accepted `Socket`. With `timeout_ms=None` blocks
    /// indefinitely; with `timeout_ms=N` raises `SrtError(TIMEOUT)`
    /// after `N` ms. Releases the GIL while parked; a `close()` (or
    /// `cancel_handle().cancel()`) from another thread ends the park
    /// with `SrtError(CLOSED)`.
    #[pyo3(signature = (timeout_ms = None))]
    fn accept(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<PySocket> {
        let res = py.allow_threads(|| {
            self.owned.with_mut(|l| match timeout_ms {
                None => l.get()?.accept(),
                Some(ms) => l.get()?.accept_timeout(Duration::from_millis(ms)),
            })
        });
        let (socket, _peer) = pyres(py, &SRT, res)?;
        Ok(PySocket::wrap(socket))
    }

    /// Returns a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `accept()`
    /// — the parked call returns `SrtError(kind=CLOSED)`. Iterator
    /// code converts that to `StopIteration` for clean for-loops.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Local bound address as `(host, port)`. Useful when the URL
    /// requested port 0 (kernel-pick) — the bound port reads back via
    /// libsrt's `getsockname`. Answered from the construction-time
    /// snapshot, so it does not wait behind an `accept()` parked on another
    /// thread — except when that construction-time read failed (`None`),
    /// in which case it falls back to the slot and waits for the parked
    /// call like the other getters. Raises `SrtError(CLOSED)` once the
    /// listener is closed.
    fn local_addr(&self, py: Python<'_>) -> PyResult<(String, u16)> {
        let addr = match self.owned.snapshot() {
            Some(addr) if !self.owned.is_closed() => *addr,
            Some(_) => {
                return Err(raise(py, &SRT, BindingError::from(HandleState::Closed)));
            }
            None => {
                let res = py.allow_threads(|| {
                    self.owned.with_mut(|l| {
                        l.get()
                            .map_err(BindingError::from)
                            .and_then(|l| l.local_addr().map_err(BindingError::from))
                    })
                });
                pyres(py, &SRT, res)?
            }
        };
        Ok((addr.ip().to_string(), addr.port()))
    }

    /// Close the listener. Fires the cancel handle BEFORE taking the
    /// slot (a parked `accept()` on another thread ends with
    /// `SrtError(CLOSED)`; an iterating `for sock in listener` loop ends
    /// with `StopIteration`), then frees the libsrt listener — the
    /// ordering `tst_srt::Listener::close` documents. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while this listener still owns the libsrt handle (an accept
    /// parked on another thread counts as alive; the probe never waits).
    fn is_alive(&self) -> bool {
        alive_probe(&self.owned, |_| true)
    }

    /// `__iter__` returns self — the listener IS its own iterator. Each
    /// `next()` calls `accept()` without timeout. `ListenerClosed`
    /// maps to `StopIteration`; other errors propagate.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<PySocket> {
        let res = py.allow_threads(|| self.owned.with_mut(|l| l.get()?.accept()));
        match res {
            Err(HandleState::Closed) => Err(PyStopIteration::new_err(())),
            Ok(Err(AcceptError::ListenerClosed)) => Err(PyStopIteration::new_err(())),
            other => {
                let (socket, _peer) = pyres(py, &SRT, other)?;
                Ok(PySocket::wrap(socket))
            }
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
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        if self.owned.is_closed() {
            "Listener(closed)".to_string()
        } else {
            "Listener(open)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------
//
// Touch sites the T4 grep ratchet looks for (already present above via
// real call sites): "CONFIG_INVALID", "TIMEOUT", "CLOSED",
// "CONNECT_FAILED", "ACCEPT_FAILED", "IO". Variants not used in this
// module (WOULD_BLOCK, BROKEN) are covered by transport.rs.

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyBuilder>()?;
    m.add_class::<PySocket>()?;
    m.add_class::<PyListener>()?;
    Ok(())
}
