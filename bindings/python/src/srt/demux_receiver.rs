//! `DemuxReceiver` convenience wrapper for SRT.
//!
//! Wraps `tst_pipeline::DemuxReceiver<tst_srt::SrtTransport>`: bind a
//! libsrt listener-mode receiver on a URL, accept one peer, demux the
//! resulting MPEG-TS stream, and iterate over `DemuxEvent` instances.
//!
//! 95% port of `bindings/python/src/rtp/demux_receiver.rs`. Differences:
//!
//! - Inner transport: `SrtTransport` instead of `RtpRecvTransport`.
//!   `tst-srt` does NOT have a separate `SrtRecvTransport` —
//!   `SrtTransport` implements both `Transport` and `RecvTransport`.
//! - URL dispatch: `SrtUrl::parse` + `Listener::bind_with` + one-shot
//!   `accept` instead of `RtpRecvSocketBuilder::from_url`. Mirrors the
//!   T2 `PyReceiver::from_url` construction pattern.
//! - Error mapping: `crate::srt::errors::*` helpers.
//!   `DemuxReceiverErrorSource::Transport` collapses to `SrtError`.
//!
//! Architectural notes (mirror `rtp/demux_receiver.rs`):
//!
//! - `__iter__` returns self; `__next__` blocks (releases the GIL) on
//!   the next `recv_event()` until either an event arrives or the
//!   transport closes / errors / cancels.
//! - Events are converted to Python via the SAME conversion path used
//!   by `tstrans.mpegts.Demuxer.__next__`: `crate::mpegts::convert_event`.
//!   No new event types — Python sees the existing
//!   `tstrans.mpegts.DemuxEvent.*` subclass hierarchy.
//! - The constructor accepts an optional `DemuxerConfig` Python
//!   dataclass; if `None`, defaults are used. Configuration is lifted
//!   onto the Rust `tst_pipeline::DemuxReceiver::with_demux_options`
//!   path via the existing `crate::mpegts::build_demuxer_config` helper.
//! - Concurrency: `inner` is held under `Arc<Mutex<Option<...>>>` and
//!   every PyMethod takes `&self`. The mutex serialises access; a
//!   concurrent `close()` / `__exit__()` from another Python thread
//!   fires the cancel handle (held outside the mutex), wakes the parked
//!   recv, then takes the inner once the recv path releases the lock.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::mpegts::demux::DemuxEvent;
use tst_core::transport::{BrokenCause, TransportCancel, TransportError};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_srt::error::AcceptError;
use tst_srt::{Listener, ListenerConfig, Socket, SrtTransport, SrtUrl, url::Mode};

use crate::errors::make_srt_error;
use crate::mpegts::demux_error_to_pyerr;
use crate::mux::PyMuxerStats;
use crate::srt::errors::{accept_error_to_pyerr, bind_error_to_pyerr, url_error_to_pyerr};
use crate::srt::transport::PySocketStats;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a `DemuxReceiverError` raised by `recv_event` onto the right
/// Python exception. Transport-side errors map to `SrtError` (via the
/// `transport_error_to_pyerr` helper from `crate::srt::errors`);
/// demux-side errors map to `DemuxError`.
fn demux_recv_error_to_pyerr(py: Python<'_>, e: DemuxReceiverError) -> PyErr {
    match e.source {
        DemuxReceiverErrorSource::Transport(t) => {
            crate::srt::errors::transport_error_to_pyerr(py, t)
        }
        DemuxReceiverErrorSource::Demux(d) => demux_error_to_pyerr(py, &d),
        // `DemuxReceiverErrorSource` is `#[non_exhaustive]`; route any
        // future variant through `SrtError(IO)` with the
        // ShellErrorKind discriminant preserved in the message.
        _ => make_srt_error(py, "IO", &format!("{:?}", e.kind)),
    }
}

// ---------------------------------------------------------------------------
// PyDemuxReceiver — wraps tst_pipeline::DemuxReceiver<SrtTransport>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `Demuxer` + `SrtTransport`.
/// Construct with a libsrt listener URL (`srt://:7000?mode=listener`);
/// iterate over the emitted `DemuxEvent` instances.
///
/// Events are instances of the existing
/// `tstrans.mpegts.DemuxEvent.*` subclass hierarchy — same conversion
/// path as `tstrans.mpegts.Demuxer.__next__`.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.srt import DemuxReceiver
///
/// with DemuxReceiver.from_url("srt://:7000?mode=listener") as rx:
///     for event in rx:
///         match event:
///             case DemuxEvent.Sample(...): ...
///             case DemuxEvent.ProgramMap(...): ...
/// ```
#[pyclass(name = "DemuxReceiver", module = "tstrans.srt")]
pub(crate) struct PyDemuxReceiver {
    /// Live receiver, behind a mutex so concurrent `__next__` /
    /// `close()` calls from different Python threads don't trip the
    /// PyO3 "Already borrowed" check that an `&mut self`-style design
    /// would hit. `Option` so `close()` can take + drop the inner
    /// receiver while keeping the PyClass addressable.
    inner: Arc<Mutex<Option<RustDemuxReceiver<SrtTransport>>>>,
    /// Cancel handle pulled from the transport at construction. Held
    /// outside the mutex so `close()` can fire it BEFORE acquiring the
    /// lock — wakes any thread parked in a `__next__`'s `recv_event`,
    /// which then drops the mutex guard and the close path can take
    /// ownership of `inner` cleanly. Cloning is cheap (`Arc`); multiple
    /// `close()` calls are idempotent.
    cancel: Arc<dyn TransportCancel + Send + Sync>,
    /// First exception raised by a registered byte sink (see
    /// `add_byte_sink`). The sink closure runs inside `recv_event`
    /// (under `allow_threads`) where it can't return a `PyResult` to
    /// the iterator, so on error it stashes the `PyErr` here (first
    /// error wins). `__next__` drains this slot AFTER `recv_event`
    /// returns and re-raises fail-loud. Separate from `inner` so the
    /// closure never touches the `inner` lock it runs underneath.
    sink_error: Arc<Mutex<Option<PyErr>>>,
}

#[pymethods]
impl PyDemuxReceiver {
    /// Bind a receiver to `url` (e.g. `"srt://:7000?mode=listener"`).
    /// Releases the GIL during bind + accept. An empty host
    /// (`srt://:7000?mode=listener`) binds to `0.0.0.0`.
    ///
    /// `demux_config` is an optional `tstrans.mpegts.DemuxerConfig`
    /// dataclass; when `None`, defaults are used.
    ///
    /// Raises `SrtError(CONFIG_INVALID)` on URL parse / bad-mode
    /// failure; `SrtError(CONNECT_FAILED)` on bind failure;
    /// `SrtError(ACCEPT_FAILED)` / `SrtError(TIMEOUT)` on accept
    /// failure.
    #[staticmethod]
    #[pyo3(signature = (url, *, demux_config = None))]
    fn from_url(
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        // 1. Parse URL + check listener mode.
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Listener {
            let msg = format!(
                "DemuxReceiver.from_url requires ?mode=listener; got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }
        let mut cfg = ListenerConfig::default();
        parsed.overlay.apply_to_listener(&mut cfg);
        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else {
            crate::util::join_host_port(&parsed.host, parsed.port)
        };

        // 2. Optionally translate the DemuxerConfig dataclass (must
        // happen with the GIL held, before allow_threads).
        let demux_opts = match demux_config {
            None => None,
            Some(cfg_obj) => Some(crate::mpegts::build_demuxer_config(py, cfg_obj)?),
        };

        // 3. Bind + accept (releases GIL during the blocking accept).
        let socket = py
            .allow_threads(|| -> Result<Socket, AcceptOrBindError> {
                let mut listener =
                    Listener::bind_with(&cfg, addr.as_str()).map_err(AcceptOrBindError::Bind)?;
                let (sock, _peer) = listener.accept().map_err(AcceptOrBindError::Accept)?;
                Ok(sock)
            })
            .map_err(|e| match e {
                AcceptOrBindError::Bind(e) => bind_error_to_pyerr(py, e),
                AcceptOrBindError::Accept(e) => accept_error_to_pyerr(py, e),
            })?;

        let transport = SrtTransport::new(socket);

        // 4. Build the receiver (with or without demux options).
        let receiver = match demux_opts {
            None => RustDemuxReceiver::new(transport),
            Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
        };
        let cancel = receiver
            .cancel_handle()
            .expect("SrtTransport always returns Some(cancel_handle) for a live socket");
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            sink_error: Arc::new(Mutex::new(None)),
        })
    }

    /// Iterator protocol: `iter(rx)` returns `self`.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Register a fan-out callback that receives every 188-byte TS
    /// packet — as a fresh `bytes` — BEFORE the demuxer parses it.
    /// `callback` is `Callable[[bytes], None]`. Sinks fire in
    /// registration order; registration is append-only for the
    /// receiver's lifetime (there is no removal). Useful for tee'ing
    /// the raw transport stream (record-to-disk, parallel parser, etc.)
    /// without consuming the demuxed event iterator.
    ///
    /// Fail-loud: if `callback` raises, the exception is captured and
    /// re-raised from the *next* `__next__` / event pull, and iteration
    /// stops. Only the first sink error is surfaced; later per-packet
    /// errors are dropped.
    ///
    /// Thread-safe to call concurrently with iteration: registration
    /// acquires the `inner` lock with the GIL released (like `close()`),
    /// so it simply blocks until the in-flight `recv_event` yields,
    /// rather than deadlocking against a sink firing under the GIL.
    ///
    /// Cost: each registered sink re-acquires the GIL once per packet
    /// inside the recv loop, so on high-bitrate streams a slow sink (or
    /// many sinks) throttles the receiver. Keep sink bodies cheap.
    ///
    /// Raises `SrtError(CLOSED)` if the receiver is already closed.
    fn add_byte_sink(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        // The closure runs inside `recv_event` (under `allow_threads`),
        // re-acquires the GIL per packet, and only ever touches
        // `callback` + `sink_error` — never `inner` (whose guard is held
        // by the parked `__next__`), so it cannot deadlock.
        let sink_error = self.sink_error.clone();
        let inner = self.inner.clone();
        // Acquire `inner` with the GIL RELEASED (matching `close()` /
        // `socket_stats()`). If we held the GIL here while a concurrent
        // `__next__` held `inner` inside `recv_event`, a sink firing on
        // the recv thread would block re-acquiring the GIL while we
        // block on `inner.lock()` — a deadlock. Registering the sink (a
        // Vec push) needs no GIL and never re-enters Python, so it is
        // safe to do inside the released-GIL block.
        let outcome: Result<(), &'static str> = py.allow_threads(move || {
            let mut guard = inner.lock().map_err(|_| "poisoned")?;
            let rx = guard.as_mut().ok_or("closed")?;
            rx.add_byte_sink(Box::new(move |pkt: &[u8]| {
                Python::with_gil(|py| {
                    let b = PyBytes::new_bound(py, pkt);
                    if let Err(e) = callback.call1(py, (b,)) {
                        // First error wins; later packet errors are dropped.
                        if let Ok(mut slot) = sink_error.lock() {
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                        }
                    }
                });
            }));
            Ok(())
        });
        outcome.map_err(|kind| match kind {
            "poisoned" => make_srt_error(py, "IO", "DemuxReceiver lock poisoned"),
            _ => make_srt_error(py, "CLOSED", "DemuxReceiver is closed"),
        })
    }

    /// Block until the next `DemuxEvent` is available. Returns a
    /// `tstrans.mpegts.DemuxEvent.*` subclass instance.
    ///
    /// Raises `StopIteration` on clean EOF (transport closed cleanly,
    /// demuxer drained); `SrtError` on transport-side failure;
    /// `DemuxError` on demuxer-side failure (strict-mode rejection,
    /// malformed PMT/PES); or any exception raised by a registered
    /// byte sink (see `add_byte_sink`), re-raised fail-loud.
    fn __next__(&self, py: Python<'_>) -> PyResult<PyObject> {
        let inner = self.inner.clone();
        let res: Result<Option<DemuxEvent>, DemuxReceiverError> = py.allow_threads(|| {
            let mut guard = match inner.lock() {
                Ok(g) => g,
                Err(_) => {
                    return Err(DemuxReceiverError::from(TransportError::Broken {
                        msg: "DemuxReceiver inner lock poisoned".into(),
                        errno_code: None,
                        cause: BrokenCause::Unspecified,
                    }));
                }
            };
            match guard.as_mut() {
                Some(rx) => rx.recv_event(),
                None => Err(DemuxReceiverError::from(TransportError::Closed)),
            }
        });
        // Fail-loud: surface any sink exception captured during this
        // `recv_event` (the `inner` guard has been dropped above, so
        // touching `sink_error` here can't nest under it). Take it so a
        // resumed iteration after a caught error isn't permanently
        // poisoned.
        if let Ok(mut slot) = self.sink_error.lock() {
            if let Some(err) = slot.take() {
                return Err(err);
            }
        }
        match res {
            Ok(None) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Ok(Some(ev)) => crate::mpegts::convert_event(py, &ev),
            Err(e) => Err(demux_recv_error_to_pyerr(py, e)),
        }
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `__next__`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<crate::srt::transport::PyCancelHandle>> {
        Py::new(
            py,
            crate::srt::transport::PyCancelHandle::from_arc(self.cancel.clone()),
        )
    }

    /// Snapshot of the scheme-neutral 16-field wire stats (matches
    /// `tstrans.srt.SocketStats`).
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        // Release the GIL before taking the inner lock. A registered byte
        // sink fires `Python::with_gil` inside `recv_event` while holding
        // this same lock; if we held the GIL here we would deadlock
        // (ABBA: iterator holds lock + blocks on GIL; this call holds GIL
        // + blocks on lock). Matches the `close()` / `add_byte_sink()`
        // pattern in this file.
        let inner = self.inner.clone();
        let core: Result<Option<tst_core::transport::SocketStats>, &'static str> = py
            .allow_threads(|| {
                let guard = inner.lock().map_err(|_| "poisoned")?;
                Ok(guard
                    .as_ref()
                    .map(|rx| rx.socket_stats().unwrap_or_default()))
            });
        let core = core
            .map_err(|_| make_srt_error(py, "IO", "DemuxReceiver lock poisoned"))?
            .ok_or_else(|| make_srt_error(py, "CLOSED", "DemuxReceiver is closed"))?;
        Py::new(py, PySocketStats::from_core(core))
    }

    /// Tuple of `(SocketStats, MuxerStats)`. Mirrors the rtp
    /// `DemuxReceiver.stats()` shape so callers can read the same
    /// `(SocketStats, MuxerStats)` tuple on both MuxSender and
    /// DemuxReceiver.
    ///
    /// Returns `SrtError(CLOSED)` if the receiver has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        // Release the GIL before taking the inner lock — same rationale
        // as `socket_stats` above (GIL↔mutex ABBA deadlock with byte
        // sinks). Extract plain Rust values under the lock, then build
        // Python objects after the guard is dropped and the GIL is
        // reacquired.
        let inner = self.inner.clone();
        type RawStats = (
            tst_core::transport::SocketStats,
            tst_core::mpegts::mux::MuxerStats,
        );
        let raw: Result<Option<RawStats>, &'static str> = py.allow_threads(|| {
            let guard = inner.lock().map_err(|_| "poisoned")?;
            Ok(guard.as_ref().map(|rx| {
                let combined = rx.stats();
                // SocketStats from the wire counters tracked at the pipeline
                // layer (full SocketStats via the transport accessor isn't
                // surfaced through the pipeline shell).
                let mut sock_stats = tst_core::transport::SocketStats::default();
                sock_stats.bytes_received = combined.bytes_received;
                sock_stats.packets_received = combined.packets_received;
                // Re-shape the demux side as a MuxerStats projection so callers
                // get the same `(SocketStats, MuxerStats)` tuple shape on both
                // MuxSender + DemuxReceiver.
                let mux_stats = tst_core::mpegts::mux::MuxerStats {
                    ts_packets_emitted: combined.packets_received,
                    ts_bytes_emitted: combined.bytes_received,
                    programs_configured: combined.program_maps_seen as u32,
                    subtitle_streams_configured: 0,
                    per_stream: combined.per_stream,
                };
                (sock_stats, mux_stats)
            }))
        });
        let (sock_stats, mux_stats) = raw
            .map_err(|_| make_srt_error(py, "IO", "DemuxReceiver lock poisoned"))?
            .ok_or_else(|| make_srt_error(py, "CLOSED", "DemuxReceiver is closed"))?;
        let sock_py = Py::new(py, PySocketStats::from_core(sock_stats))?;
        let mux_py = Py::new(py, PyMuxerStats::from_inner(mux_stats))?;
        Ok((sock_py, mux_py))
    }

    /// Wall-clock time the stream identified by `pid` last carried an
    /// item through this receiver (last emitted event), as a Unix-epoch
    /// microsecond count. `None` if `pid` was never seen — including an
    /// unrecognized PID (no range check beyond the native `u16`, mirror
    /// of `Muxer.stream_codec_stats`'s pid handling: unknown → `None`,
    /// no dedicated "bad pid" error).
    ///
    /// This deliberately differs from the C ABI's `0`-sentinel
    /// convention (the C getters have no `Option`) — Python's `None` is
    /// the honest "never" value.
    ///
    /// Raises `SrtError(CLOSED)` if the receiver has been closed — same
    /// lock discipline as `stats()`.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        // Same GIL-released lock-then-extract shape as `stats()` above.
        let inner = self.inner.clone();
        let raw: Result<Option<Option<std::time::SystemTime>>, &'static str> =
            py.allow_threads(|| {
                let guard = inner.lock().map_err(|_| "poisoned")?;
                Ok(guard
                    .as_ref()
                    .map(|rx| rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen)))
            });
        let last_seen = raw
            .map_err(|_| make_srt_error(py, "IO", "DemuxReceiver lock poisoned"))?
            .ok_or_else(|| make_srt_error(py, "CLOSED", "DemuxReceiver is closed"))?;
        Ok(last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64))
    }

    /// Close the receiver. Idempotent. Fires the cancel handle BEFORE
    /// acquiring the mutex so a concurrent `__next__` parked in
    /// `recv_event` unparks promptly.
    fn close(&self, py: Python<'_>) {
        self.cancel.cancel();
        let inner = self.inner.clone();
        py.allow_threads(move || {
            if let Ok(mut guard) = inner.lock() {
                if let Some(mut r) = guard.take() {
                    r.close();
                }
            }
        });
    }

    /// `True` while the receiver owns a live transport.
    fn is_alive(&self) -> bool {
        match self.inner.try_lock() {
            Ok(g) => g.as_ref().is_some_and(|r| r.is_alive()),
            // Lock currently held by a parked __next__ — the receiver
            // is still alive (the parked recv hasn't released the
            // inner). Optimistic but matches the rtp shape.
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
        match self.inner.try_lock() {
            Ok(g) => match g.as_ref() {
                Some(_) => "DemuxReceiver(open)".to_string(),
                None => "DemuxReceiver(closed)".to_string(),
            },
            Err(_) => "DemuxReceiver(<busy>)".to_string(),
        }
    }
}

impl PyDemuxReceiver {
    /// Crate-private constructor used by `Socket::into_demux_receiver`
    /// (T3): takes an already-connected `Socket` (the caller has
    /// already done the accept/connect handshake) and wraps it with
    /// default demux options.
    pub(crate) fn from_pipeline_demux(socket: Socket) -> Self {
        let transport = SrtTransport::new(socket);
        let receiver = RustDemuxReceiver::new(transport);
        let cancel = receiver
            .cancel_handle()
            .expect("SrtTransport always returns Some(cancel_handle) for a live socket");
        Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            sink_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Like [`Self::from_pipeline_demux`] but with explicit demuxer
    /// options. Used when the Python caller passes a `DemuxerConfig`
    /// dataclass to `Socket.into_demux_receiver(demux_config=...)`.
    pub(crate) fn from_pipeline_demux_with_config(
        socket: Socket,
        opts: tst_core::mpegts::demux::DemuxerConfig,
    ) -> Self {
        let transport = SrtTransport::new(socket);
        let receiver = RustDemuxReceiver::with_demux_options(transport, opts);
        let cancel = receiver
            .cancel_handle()
            .expect("SrtTransport always returns Some(cancel_handle) for a live socket");
        Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            sink_error: Arc::new(Mutex::new(None)),
        }
    }
}

/// Internal helper for `DemuxReceiver::from_url` — combines bind +
/// accept failure paths inside one `allow_threads` block. Each variant
/// maps to a distinct user-visible `SrtErrorKind` in the outer match.
enum AcceptOrBindError {
    Bind(tst_srt::error::BindError),
    Accept(AcceptError),
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDemuxReceiver>()?;
    Ok(())
}
