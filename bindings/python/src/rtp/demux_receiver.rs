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
//! - Concurrency (Arc 2):
//!   Every wrapper holds a `tst_pipeline::binding::Owned`, which takes
//!   the slot only inside `with_mut` / `with_ref` (GIL released) and
//!   makes `close()` cancel-first.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::RecvTransport;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_rtp::{RtpRecvSocketBuilder, RtpRecvTransport, StreamEndReasonHandle};

use crate::mpegts::demux_error_to_pyerr;
use crate::mux::PyMuxerStats;
use crate::raise::{RTP, pyok, raise};
use crate::rtp::transport::PySocketStats;
use crate::util::close_owned;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Demux-sourced failures keep `demux_error_to_pyerr` (a `DemuxError`,
/// not an `RtpError`); transport-sourced ones are `BindingError`s — and
/// A2's K6 rule maps a receiver shell's peer EOS to `EndOfStream`, which
/// an iterator reports as `StopIteration` (today's clean-EOF shape).
fn demux_recv_err(py: Python<'_>, e: DemuxReceiverError) -> PyErr {
    match e.source {
        DemuxReceiverErrorSource::Demux(d) => demux_error_to_pyerr(py, &d),
        _ => {
            let be = BindingError::from(e);
            if be.kind == BindingErrorKind::EndOfStream {
                pyo3::exceptions::PyStopIteration::new_err(())
            } else {
                raise(py, &RTP, be)
            }
        }
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
    /// The binding layer's handle state machine (Arc 2): a parked
    /// `__next__` holds the slot only inside `with_mut`, under
    /// `py.allow_threads`, so a concurrent `close()` from another thread
    /// ends it (cancel-first) instead of waiting behind it or tripping
    /// PyO3's "Already borrowed" check.
    owned: Owned<RustDemuxReceiver<RtpRecvTransport>>,
    /// Handle onto the underlying `RtpRecvTransport`'s
    /// [`StreamEndReasonHandle`], captured from the transport BEFORE it
    /// moves into `DemuxReceiver::new`/`with_demux_options` — the
    /// pipeline shell (generic over `RecvTransport`) has no
    /// `end_reason_handle()` delegate of its own, so this must be pulled
    /// pre-move. It is a `tst_rtp::StreamEndReasonHandle` — a different
    /// type and enum from the `RecvEndReasonHandle` that
    /// `Owned::with_end_reason` takes, which is why it stays a field.
    /// Independent of the slot's lifetime, so `end_reason()` /
    /// `end_detail()` keep answering after `close()`.
    end_reason: StreamEndReasonHandle,
    /// First exception raised by a registered byte sink (see
    /// `add_byte_sink`). The sink closure runs inside `recv_event`, where
    /// it cannot return a `PyResult` to the iterator, so on error it
    /// stashes the `PyErr` here (first error wins) and `__next__` drains
    /// the slot after `recv_event` returns. Kept OUTSIDE `owned` so the
    /// closure never touches the slot it runs underneath.
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
    /// Raises `RtpError(CLOSED)` on URL parse / socket bind failure.
    #[new]
    #[pyo3(signature = (url, *, demux_config = None))]
    fn new(py: Python<'_>, url: &str, demux_config: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let builder = RtpRecvSocketBuilder::from_url(url)
            .map_err(|e| raise(py, &RTP, BindingError::from(tst_rtp::ConnectError::from(e))))?;
        let transport = builder
            .build()
            .map_err(|e| raise(py, &RTP, BindingError::from(e)))?;
        let opts = match demux_config {
            None => None,
            Some(cfg) => Some(crate::mpegts::build_demuxer_config(py, cfg)?),
        };
        Self::from_recv_transport(py, transport, opts)
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
    /// Raises `RtpError(CLOSED)` if the receiver is already closed.
    fn add_byte_sink(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        // The closure runs inside `recv_event` (under `allow_threads`),
        // re-acquires the GIL per packet, and only ever touches
        // `callback` + `sink_error` — never `inner` (whose guard is held
        // by the parked `__next__`), so it cannot deadlock.
        let sink_error = self.sink_error.clone();
        // Take the slot with the GIL RELEASED (matching `close()` /
        // `stats()`). If we held the GIL here while a concurrent
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
        pyok(py, &RTP, res)
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
        let res = py.allow_threads(|| self.owned.with_mut(|rx| rx.recv_event()));
        // Fail-loud: surface any sink exception captured during this
        // `recv_event` (the slot guard has been dropped above, so touching
        // `sink_error` here can't nest under it).
        if let Ok(mut slot) = self.sink_error.lock() {
            if let Some(err) = slot.take() {
                return Err(err);
            }
        }
        match res {
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
            Ok(Ok(None)) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Ok(Ok(Some(ev))) => crate::mpegts::convert_event(py, &ev),
            Ok(Err(e)) => Err(demux_recv_err(py, e)),
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
        let raw = py.allow_threads(|| {
            self.owned.with_ref(|rx| {
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
            })
        });
        let (sock_stats, mux_stats) = pyok(py, &RTP, raw)?;
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
    /// Raises `RtpError(CLOSED)` if the receiver has been closed —
    /// same lock discipline as `stats()`.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        // Same GIL-released lock-then-extract shape as `stats()` above:
        // a registered byte sink fires `Python::with_gil` inside
        // `recv_event` while holding this same lock, so holding the GIL
        // here would deadlock (ABBA).
        let last_seen = pyok(
            py,
            &RTP,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|rx| rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen))
            }),
        )?;
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
        if self.owned.is_closed() {
            "DemuxReceiver(closed)".to_string()
        } else {
            "DemuxReceiver(open)".to_string()
        }
    }
}

impl PyDemuxReceiver {
    /// Crate-private constructor used by `#[new]` and by
    /// `crate::rtp::client::PyRtspSession::into_demux_receiver`: takes an
    /// already-built `RtpRecvTransport` and wraps it, with or without
    /// explicit demuxer options.
    ///
    /// A transport handed over by `RtspSession::into_recv_transport`
    /// already carries the owning `RtspClient`'s shared end-reason slot,
    /// so the handle pulled here observes reasons recorded by the RTSP
    /// keepalive / pump threads too, not just this receiver's own close.
    pub(crate) fn from_recv_transport(
        py: Python<'_>,
        transport: RtpRecvTransport,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        // Both pulled BEFORE the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let cancel =
            crate::rtp::transport::rtp_cancel_source(py, RecvTransport::cancel_handle(&transport))?;
        let receiver = match opts {
            None => RustDemuxReceiver::new(transport),
            Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
        };
        // The `CancelSource` lives on inside `Owned` (it is the shell's
        // `Arc<dyn TransportCancel>`), so `close()` still cancels first.
        // No field: the rtp `DemuxReceiver` exposes no `cancel_handle()`
        // — a documented parity gap vs the srt twin.
        Ok(Self {
            owned: Owned::new(receiver, cancel.as_dyn(), ()),
            end_reason,
            sink_error: Arc::new(Mutex::new(None)),
        })
    }
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDemuxReceiver>()?;
    Ok(())
}
