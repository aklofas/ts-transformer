//! `DemuxReceiver` convenience wrapper for SRT.
//!
//! Wraps `tst_pipeline::DemuxReceiver<tst_srt::SrtTransport>`: bind a
//! libsrt listener-mode receiver on a URL, accept one peer, demux the
//! resulting MPEG-TS stream, and iterate over `DemuxEvent` instances.
//!
//! 95% port of `bindings/python/src/rtp/demux_receiver.rs`. Differences:
//!
//! - Inner transport: `SrtTransport` instead of `RtpRecvTransport`.
//!   `tst-srt` does NOT have a separate receive-only transport type —
//!   `SrtTransport` implements both `Transport` and `RecvTransport`.
//! - URL dispatch: `SrtUrl::parse` + `Listener::bind_with` + one-shot
//!   `accept` instead of `RtpRecvSocketBuilder::from_url`. Mirrors the
//!   T2 `PyReceiver::from_url` construction pattern.
//! - Error mapping: the one raise path (`crate::raise`), with
//!   demux-sourced failures kept on `DemuxError` (`demux_recv_err`).
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

use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::mpegts::demux_error_to_pyerr;
use crate::mux::PyMuxerStats;
use crate::raise::{SRT, pyok, raise};
use crate::srt::transport::{PySocketStats, srt_cancel_source};
use crate::util::{CancelSource, alive_probe, close_owned};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Demux-sourced failures keep `demux_error_to_pyerr` (a `DemuxError`,
/// not an `SrtError`); transport-sourced ones are `BindingError`s — and
/// A2's K6 rule maps a receiver shell's `Transport(TransportError::Closed)`
/// (peer EOS) to `EndOfStream`, which an iterator reports as
/// `StopIteration` (today's clean-EOF shape), never as an error.
///
/// `pub(crate)` so `managed_convenience`'s `ManagedDemuxReceiver` shares
/// the one split instead of carrying a copy.
pub(crate) fn demux_recv_err(py: Python<'_>, e: DemuxReceiverError) -> PyErr {
    match e.source {
        DemuxReceiverErrorSource::Demux(d) => demux_error_to_pyerr(py, &d),
        _ => {
            let be = BindingError::from(e);
            if be.kind == BindingErrorKind::EndOfStream {
                pyo3::exceptions::PyStopIteration::new_err(())
            } else {
                raise(py, &SRT, be)
            }
        }
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
    /// The binding layer's handle state machine (Arc 2): a `__next__`
    /// parked in `recv_event` holds the slot only inside `with_mut`,
    /// under `py.allow_threads`; `close()` (cancel-first) ends it
    /// instead of waiting behind it.
    owned: Owned<RustDemuxReceiver<SrtTransport>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// First exception raised by a registered byte sink (see
    /// `add_byte_sink`). The sink closure runs inside `recv_event`
    /// (under `allow_threads`) where it can't return a `PyResult` to
    /// the iterator, so on error it stashes the `PyErr` here (first
    /// error wins). `__next__` drains this slot AFTER `recv_event`
    /// returns and re-raises fail-loud. Separate from `inner` so the
    /// closure never touches the `inner` lock it runs underneath.
    sink_error: Arc<Mutex<Option<PyErr>>>,
}

impl PyDemuxReceiver {
    /// Wrap an already-accepted transport (the `from_url` and
    /// `Socket.into_demux_receiver()` paths meet here).
    pub(crate) fn from_transport(
        transport: SrtTransport,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> Self {
        let cancel = srt_cancel_source(&transport);
        let receiver = match opts {
            None => RustDemuxReceiver::new(transport),
            Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
        };
        Self {
            owned: Owned::new(receiver, cancel.as_dyn(), ()),
            cancel,
            sink_error: Arc::new(Mutex::new(None)),
        }
    }
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
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Listener {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "DemuxReceiver.from_url requires ?mode=listener; got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        // Translate the DemuxerConfig dataclass with the GIL held, before
        // allow_threads.
        let demux_opts = match demux_config {
            None => None,
            Some(cfg_obj) => Some(crate::mpegts::build_demuxer_config(py, cfg_obj)?),
        };
        // A3 owns the bind-host rule (empty host => 0.0.0.0), the IPv6
        // bracketing and the single-accept listener.
        let slot = tst_core::cancel::CancelSlot::new();
        let transport = py
            .allow_threads(|| parsed.accept_one(&slot))
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        Ok(Self::from_transport(transport, demux_opts))
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
        // Take the slot with the GIL RELEASED (matching `close()` /
        // `socket_stats()`). If we held the GIL here while a concurrent
        // `__next__` held the slot inside `recv_event`, a sink firing on
        // the recv thread would block re-acquiring the GIL while we
        // block on the slot — a deadlock. Registering the sink (a
        // Vec push) needs no GIL and never re-enters Python, so it is
        // safe to do inside the released-GIL block.
        let res = py.allow_threads(move || {
            self.owned.with_mut(|rx| {
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
            })
        });
        pyok(py, &SRT, res)
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
        let res = py.allow_threads(|| self.owned.with_mut(|rx| rx.recv_event()));
        // Fail-loud: surface any sink exception captured during this
        // `recv_event` (the slot guard has been dropped above, so touching
        // `sink_error` here can't nest under it). Take it so a resumed
        // iteration after a caught error isn't permanently poisoned.
        if let Ok(mut slot) = self.sink_error.lock() {
            if let Some(err) = slot.take() {
                return Err(err);
            }
        }
        match res {
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
            Ok(Ok(None)) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Ok(Ok(Some(ev))) => crate::mpegts::convert_event(py, &ev),
            Ok(Err(e)) => Err(demux_recv_err(py, e)),
        }
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `__next__`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<crate::srt::transport::PyCancelHandle>> {
        Py::new(
            py,
            crate::srt::transport::PyCancelHandle::from_source(&self.cancel),
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
        let core = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|rx| rx.socket_stats().unwrap_or_default())
            }),
        )?;
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
        let raw = py.allow_threads(|| {
            self.owned.with_ref(|rx| {
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
            })
        });
        let (sock_stats, mux_stats) = pyok(py, &SRT, raw)?;
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
        let raw = py.allow_threads(|| {
            self.owned
                .with_ref(|rx| rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen))
        });
        let last_seen = pyok(py, &SRT, raw)?;
        Ok(last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64))
    }

    /// Close the receiver. Idempotent. Fires the cancel handle BEFORE
    /// acquiring the mutex so a concurrent `__next__` parked in
    /// `recv_event` unparks promptly.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the receiver owns a live transport (a `__next__`
    /// parked on another thread counts as live; the probe never waits).
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
            "DemuxReceiver(closed)".to_string()
        } else {
            "DemuxReceiver(open)".to_string()
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
