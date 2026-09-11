//! `DemuxReceiver` convenience wrapper for RTP.
//!
//! Wraps `tst_pipeline::DemuxReceiver<tst_rtp::RtpRecvTransport>`:
//! bind a UDP RTP receiver to a URL, demux the resulting MPEG-TS
//! stream, and iterate over `DemuxEvent` instances.
//!
//! Architectural notes:
//!
//! - The PyClass wraps `DemuxReceiver<RtpRecvTransport>` directly,
//!   matching the Stage 1 tst-c lesson #1 (handles concrete
//!   per-transport).
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
//!   path via the existing `crate::mpegts::build_demuxer_config`
//!   helper.
//! - Concurrency: `inner` is held under `Arc<Mutex<Option<...>>>` and
//!   every PyMethod takes `&self`. The mutex serialises access; if a
//!   `__next__` call is parked under `py.allow_threads`, a concurrent
//!   `close()` / `__exit__()` from another Python thread fires the
//!   cancel handle (held outside the mutex), wakes the parked recv,
//!   then takes the inner once the recv path releases the lock. This
//!   avoids the PyO3 "Already borrowed" error that an
//!   `&mut self`-style design hits when close races a parked next.
//!
//! Error mapping:
//! - `DemuxReceiverErrorSource::Transport(...)` → `RtpError(TRANSPORT)`
//!   (or `CANCELLED` / `MALFORMED_PACKET` / `TIMEOUT` per `TransportError`
//!   variant — see `transport_error_to_rtp_pyerr` below)
//! - `DemuxReceiverErrorSource::Demux(...)`     → `DemuxError`
//! - Construction-time `ConnectError`            → `RtpError(TRANSPORT)`
//!
//! Both error pathways exercise existing literal `make_rtp_error` and
//! `make_demux_error` call sites, so no new ratchet anchors are needed.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::mpegts::demux::DemuxEvent;
use tst_core::transport::{BrokenCause, TransportError};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_rtp::{RtpRecvSocketBuilder, RtpRecvTransport, StreamEndReasonHandle};

use crate::errors::make_rtp_error;
use crate::mpegts::demux_error_to_pyerr;
use crate::mux::PyMuxerStats;
use crate::rtp::transport::PySocketStats;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Map a `TransportError` to `RtpError`. Mirror of the helper in
/// `transport.rs` + `mux_sender.rs`; kept inline so each file owns its
/// own error mapping without `pub(crate)` re-exports.
fn transport_error_to_rtp_pyerr(py: Python<'_>, e: TransportError) -> PyErr {
    match e {
        TransportError::ExplicitClose => {
            make_rtp_error(py, "CANCELLED", "transport cancelled by caller")
        }
        TransportError::TooLarge { len, max } => {
            let msg = format!("payload too large: {len} bytes exceeds {max}-byte cap");
            make_rtp_error(py, "MALFORMED_PACKET", &msg)
        }
        // A configured recv deadline (`?recv_timeout=` / `set_recv_timeout`)
        // expired — retryable, the transport/session is still alive.
        TransportError::Backpressure { msg, .. } => make_rtp_error(py, "TIMEOUT", &msg),
        other => make_rtp_error(py, "TRANSPORT", &other.to_string()),
    }
}

/// Map a `DemuxReceiverError` raised by `recv_event` onto the right
/// Python exception. Transport-side errors map to `RtpError`; demux-side
/// errors map to `DemuxError`.
fn demux_recv_error_to_pyerr(py: Python<'_>, e: DemuxReceiverError) -> PyErr {
    match e.source {
        DemuxReceiverErrorSource::Transport(t) => transport_error_to_rtp_pyerr(py, t),
        DemuxReceiverErrorSource::Demux(d) => demux_error_to_pyerr(py, &d),
        // `DemuxReceiverErrorSource` is `#[non_exhaustive]`; route any
        // future variant through `RtpError(TRANSPORT)` with the
        // ShellErrorKind discriminant preserved in the message.
        _ => make_rtp_error(py, "TRANSPORT", &format!("{:?}", e.kind)),
    }
}

// ---------------------------------------------------------------------------
// PyDemuxReceiver — wraps tst_pipeline::DemuxReceiver<RtpRecvTransport>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `Demuxer` + `RtpRecvTransport`.
/// Construct with a URL (`rtp://host:port`); iterate over the emitted
/// `DemuxEvent` instances.
///
/// Events are instances of the existing
/// `tstrans.mpegts.DemuxEvent.*` subclass hierarchy — same conversion
/// path as `tstrans.mpegts.Demuxer.__next__`.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.rtp import DemuxReceiver
///
/// with DemuxReceiver("rtp://0.0.0.0:5004") as rx:
///     for event in rx:
///         match event:
///             case DemuxEvent.Sample(...): ...
///             case DemuxEvent.ProgramMap(...): ...
/// ```
#[pyclass(name = "DemuxReceiver", module = "tstrans.rtp")]
pub struct PyDemuxReceiver {
    /// Live receiver, behind a mutex so concurrent `__next__` /
    /// `close()` calls from different Python threads don't trip the
    /// PyO3 "Already borrowed" check that an `&mut self`-style design
    /// would hit. `Option` so `close()` can take + drop the inner
    /// receiver while keeping the PyClass addressable.
    inner: Arc<Mutex<Option<RustDemuxReceiver<RtpRecvTransport>>>>,
    /// Cancel handle pulled from the transport at construction. Held
    /// outside the mutex so `close()` can fire it BEFORE acquiring the
    /// lock — wakes any thread parked in a `__next__`'s `recv_event`,
    /// which then drops the mutex guard and the close path can take
    /// ownership of `inner` cleanly. Cloning is cheap (`Arc`); multiple
    /// `close()` calls are idempotent.
    cancel: Arc<dyn tst_core::transport::TransportCancel + Send + Sync>,
    /// Handle onto the underlying `RtpRecvTransport`'s
    /// [`StreamEndReasonHandle`], captured from the transport BEFORE it
    /// moves into `DemuxReceiver::new`/`with_demux_options` — the
    /// pipeline shell (generic over `RecvTransport`) has no
    /// `end_reason_handle()` delegate of its own, so this must be pulled
    /// pre-move, same as `cancel` above. Independent of `inner`'s
    /// lifetime, so `end_reason()` / `end_detail()` keep working after
    /// `close()`.
    end_reason: StreamEndReasonHandle,
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
    /// Bind a receiver to `url` (e.g. `"rtp://0.0.0.0:5004"` for
    /// unicast or `"rtp://239.0.0.1:5004"` for multicast).
    ///
    /// `demux_config` is an optional `tstrans.mpegts.DemuxerConfig`
    /// dataclass; when `None`, defaults are used.
    ///
    /// Raises `RtpError(TRANSPORT)` on URL parse / socket bind failure.
    #[new]
    #[pyo3(signature = (url, *, demux_config = None))]
    fn new(py: Python<'_>, url: &str, demux_config: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        // Build the RTP recv transport.
        let builder = RtpRecvSocketBuilder::from_url(url)
            .map_err(|e| make_rtp_error(py, "TRANSPORT", &e.to_string()))?;
        let transport = builder
            .build()
            .map_err(|e| make_rtp_error(py, "TRANSPORT", &e.to_string()))?;
        // Pulled BEFORE `transport` moves into the pipeline shell below —
        // see the `end_reason` field doc for why.
        let end_reason = transport.end_reason_handle();

        // Build the receiver (with or without demux options).
        let receiver = match demux_config {
            None => RustDemuxReceiver::new(transport),
            Some(cfg) => {
                let opts = crate::mpegts::build_demuxer_config(py, cfg)?;
                RustDemuxReceiver::with_demux_options(transport, opts)
            }
        };
        // Pull the cancel handle once at construction — the pipeline
        // shell delegates to the underlying transport's
        // `cancel_handle()`, which always returns Some() for
        // RtpRecvTransport.
        let cancel = receiver
            .cancel_handle()
            .expect("RtpRecvTransport always returns Some(cancel_handle)");
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            end_reason,
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
    /// Raises `RtpError(TRANSPORT)` if the receiver is already closed.
    fn add_byte_sink(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        // The closure runs inside `recv_event` (under `allow_threads`),
        // re-acquires the GIL per packet, and only ever touches
        // `callback` + `sink_error` — never `inner` (whose guard is held
        // by the parked `__next__`), so it cannot deadlock.
        let sink_error = self.sink_error.clone();
        let inner = self.inner.clone();
        // Acquire `inner` with the GIL RELEASED (matching `close()` /
        // `stats()`). If we held the GIL here while a concurrent
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
            "poisoned" => make_rtp_error(py, "TRANSPORT", "DemuxReceiver lock poisoned"),
            _ => make_rtp_error(py, "TRANSPORT", "DemuxReceiver is closed"),
        })
    }

    /// Block until the next `DemuxEvent` is available. Returns a
    /// `tstrans.mpegts.DemuxEvent.*` subclass instance.
    ///
    /// Raises `StopIteration` on clean EOF (transport closed cleanly,
    /// demuxer drained); `RtpError` on transport-side failure;
    /// `DemuxError` on demuxer-side failure (strict-mode rejection,
    /// malformed PMT/PES); or any exception raised by a registered
    /// byte sink (see `add_byte_sink`), re-raised fail-loud.
    fn __next__(&self, py: Python<'_>) -> PyResult<PyObject> {
        let inner = self.inner.clone();
        // Release the GIL while parked on `recv_event`. The pipeline's
        // `recv_event` is pure-Rust: parks on the underlying transport's
        // `recv_bytes`, feeds the resulting TS packet to the demuxer,
        // and returns the next event. No Python objects are constructed
        // inside, so `allow_threads` is safe.
        //
        // The mutex guard is acquired inside `allow_threads` so a
        // concurrent `close()` (which doesn't take the mutex; just
        // fires `cancel`) can wake the parked recv. Once recv returns
        // (event, EOF, or cancelled error), we drop the guard and the
        // close path can take ownership of `inner` cleanly.
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

    /// Tuple of `(SocketStats, MuxerStats)`. `SocketStats` reflects the
    /// underlying RTP transport's wire-level counters; the second
    /// element is a `MuxerStats`-shaped projection of the demuxer's
    /// own event/PMT counters surfaced through the pipeline shell's
    /// `DemuxReceiverStats` (a Demuxer-side analog — fields are reused
    /// to keep the tuple shape symmetric with `MuxSender.stats()`).
    ///
    /// Returns zeroed defaults if the receiver is closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        // Release the GIL before taking the inner lock. A registered byte
        // sink fires `Python::with_gil` inside `recv_event` while holding
        // this same lock; if we held the GIL here we would deadlock
        // (ABBA: iterator holds lock + blocks on GIL; this call holds GIL
        // + blocks on lock). Matches the `close()` / `add_byte_sink()`
        // pattern in this file. Extract plain Rust values under the lock,
        // build Python objects after the guard is dropped and the GIL is
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
                // The underlying RTP transport's full SocketStats live behind a
                // separate accessor that the pipeline shell doesn't expose
                // directly; we synthesise a SocketStats with the
                // bytes_received / packets_received fields populated from the
                // pipeline projection. RTCP-derived fields stay zero until
                // Stage 3 closes the deferred TCP RTCP wiring.
                // `SocketStats` is `#[non_exhaustive]`; populate via mut spread.
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
            .map_err(|_| make_rtp_error(py, "TRANSPORT", "DemuxReceiver lock poisoned"))?
            .ok_or_else(|| make_rtp_error(py, "TRANSPORT", "DemuxReceiver is closed"))?;
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
    /// Raises `RtpError(TRANSPORT)` if the receiver has been closed —
    /// same lock discipline as `stats()`.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        // Same GIL-released lock-then-extract shape as `stats()` above:
        // a registered byte sink fires `Python::with_gil` inside
        // `recv_event` while holding this same lock, so holding the GIL
        // here would deadlock (ABBA).
        let inner = self.inner.clone();
        let raw: Result<Option<Option<std::time::SystemTime>>, &'static str> =
            py.allow_threads(|| {
                let guard = inner.lock().map_err(|_| "poisoned")?;
                Ok(guard
                    .as_ref()
                    .map(|rx| rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen)))
            });
        let last_seen = raw
            .map_err(|_| make_rtp_error(py, "TRANSPORT", "DemuxReceiver lock poisoned"))?
            .ok_or_else(|| make_rtp_error(py, "TRANSPORT", "DemuxReceiver is closed"))?;
        Ok(last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64))
    }

    /// Why the receive session ended, or `None` if it hasn't ended yet
    /// (or ended through a path this arc doesn't instrument). Still
    /// readable after `close()` — `end_reason` is a
    /// [`tst_rtp::StreamEndReasonHandle`] captured from the underlying
    /// transport at construction, independent of `inner`'s lifetime.
    ///
    /// `StreamEndReasonHandle::get` is a lock-free `Arc<OnceLock<_>>`
    /// read — no blocking, so no `py.allow_threads` is needed and no
    /// `inner` mutex is touched.
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

    /// Close the receiver. Idempotent. Fires the cancel handle BEFORE
    /// acquiring the mutex so a concurrent `__next__` parked in
    /// `recv_event` unparks promptly — without this cancel-first step
    /// the close would deadlock waiting for the lock the parked recv
    /// holds.
    fn close(&self, py: Python<'_>) {
        // Step 1: cancel any in-flight recv. Releases the parked
        // `__next__` immediately (within ~100ms per RTP transport
        // cancel-poll tick). py.allow_threads so we don't hold the GIL
        // during the brief cancel-wake window.
        self.cancel.cancel();
        let inner = self.inner.clone();
        py.allow_threads(move || {
            // Step 2: take the inner under the mutex. If the parked
            // recv has already returned, this is immediate; otherwise
            // we wait briefly for it to release.
            if let Ok(mut guard) = inner.lock() {
                if let Some(mut r) = guard.take() {
                    r.close();
                }
            }
            // Lock-poisoned: silently no-op (close is best-effort —
            // the cancel above already woke any parked recv).
        });
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
        // Try-lock so a __repr__ call from another thread during a
        // parked recv doesn't deadlock; report "<busy>" if locked.
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
    /// Crate-private constructor used by
    /// `crate::rtp::client::PyRtspSession::into_demux_receiver`: takes
    /// an already-built `RtpRecvTransport` (handed off from the RTSP
    /// session via `RtspSession::into_recv_transport`) and wraps it
    /// with default demux options.
    pub(crate) fn from_recv_transport(transport: RtpRecvTransport) -> Self {
        // Pulled BEFORE `transport` moves into `DemuxReceiver::new` below.
        // `transport` here already carries the owning RtspClient's
        // shared end-reason slot (swapped in by
        // `RtspSession::into_recv_transport` before this constructor is
        // ever called), so this handle observes reasons recorded by the
        // RTSP keepalive/pump threads too, not just this receiver's own
        // close.
        let end_reason = transport.end_reason_handle();
        let receiver = RustDemuxReceiver::new(transport);
        let cancel = receiver
            .cancel_handle()
            .expect("RtpRecvTransport always returns Some(cancel_handle)");
        Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            end_reason,
            sink_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Like [`Self::from_recv_transport`] but with explicit demuxer
    /// options. Used when the Python caller passes a `DemuxerConfig`
    /// dataclass to `RtspSession.into_demux_receiver(demux_config=...)`.
    pub(crate) fn from_recv_transport_with_config(
        transport: RtpRecvTransport,
        opts: tst_core::mpegts::demux::DemuxerConfig,
    ) -> Self {
        // See `from_recv_transport`'s comment — same pre-move capture,
        // same RTSP-shared-slot carry-through.
        let end_reason = transport.end_reason_handle();
        let receiver = RustDemuxReceiver::with_demux_options(transport, opts);
        let cancel = receiver
            .cancel_handle()
            .expect("RtpRecvTransport always returns Some(cancel_handle)");
        Self {
            inner: Arc::new(Mutex::new(Some(receiver))),
            cancel,
            end_reason,
            sink_error: Arc::new(Mutex::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDemuxReceiver>()?;
    Ok(())
}
