//! `ManagedMuxSender` + `ManagedDemuxReceiver` for SRT.
//!
//! Auto-reconnect convenience wrappers paralleling
//! [`crate::srt::mux_sender::PyMuxSender`] +
//! [`crate::srt::demux_receiver::PyDemuxReceiver`] one-for-one, but with
//! a [`tst_pipeline::reconnect::ManagedTransport`] (sender) or a
//! [`tst_pipeline::ManagedRecvTransport`] (receiver) underneath. URL +
//! socket config are captured at construction and replayed by the
//! reconnect factory on each `Broken`/`Closed` event from the inner SRT
//! socket.
//!
//! ## Why a new file rather than extending T5
//!
//! The inner type of the wrapped pipeline shell changes
//! (`SrtTransport` → `ManagedTransport<SrtTransport>` /
//! `ManagedRecvTransport<SrtTransport>`), which cascades into the field
//! type, every accessor, and the cancel-handle wiring. Sharing T5's code
//! via generics would force the PyClass methods to be generic too —
//! pyo3 doesn't support generic `#[pymethods]`. Copy + adjust is the
//! ergonomic shape.
//!
//! ## Reconnect-attempt counter
//!
//! Both wrappers expose `reconnect_attempts() -> int` from
//! `ManagedHandles.attempts` — the core's factory-invocation counter,
//! owned by `ManagedTransport` / `ManagedRecvTransport` since Arc 2
//! (ARCH-08). The binding used to keep its own `Arc<AtomicU64>` bumped
//! from inside a factory closure; that closure, and the drift it allowed
//! against `ManagedTransportStats.reconnect_attempts`, are gone.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::Py;
use pyo3::prelude::*;

use tst_pipeline::binding::{BindingError, BindingErrorKind, HandleState, Owned};
use tst_pipeline::{
    ManagedDemuxReceiver as RustManagedDemuxReceiver, ManagedTransport, MuxSender as RustMuxSender,
};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::errors::mux_error_to_pyerr;
use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig, PyMuxerStats,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::raise::{SRT, pyok, raise};
use crate::srt::demux_receiver::demux_recv_err;
use crate::srt::mux_sender::mux_sender_err;
use crate::srt::policy::{PyManagedTransportStats, PyReconnectPolicy};
use crate::srt::transport::{PyCancelHandle, PySocketStats};
use crate::util::{CancelSource, alive_probe, close_owned};

// ---------------------------------------------------------------------------
// PyManagedMuxSender — wraps MuxSender<ManagedTransport<SrtTransport>>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `Muxer` plus a managed
/// SRT transport (auto-reconnect on `Broken`/`Closed`).
///
/// Construct via `ManagedMuxSender.from_url(url, program_config,
/// policy=ReconnectPolicy(...))`. URL must specify `?mode=caller` (the
/// default). When the underlying SRT socket drops, the wrapper rebuilds
/// it using the captured (URL, SocketConfig) — bytes accumulated during
/// the outage land in the gap buffer (sized by `ReconnectPolicy`).
///
/// All push methods accept any bytes-like input and release the GIL
/// while the muxer + transport work proceeds.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.srt import ManagedMuxSender, ReconnectPolicy
/// from tstrans.mpegts import MuxerProgramConfigBuilder, VideoCodec, Pts90khz
///
/// program = (
///     MuxerProgramConfigBuilder(1, 0x100)
///     .add_video(0x101, VideoCodec.H264)
///     .build()
/// )
/// with ManagedMuxSender.from_url(
///     "srt://127.0.0.1:7000?mode=caller", program, policy=ReconnectPolicy()
/// ) as s:
///     s.push_video(b"\x00\x00\x00\x01\x09\xf0", pts=Pts90khz.from_raw(0))
/// ```
#[pyclass(name = "ManagedMuxSender", module = "tstrans.srt")]
pub(crate) struct PyManagedMuxSender {
    /// Shared slot (PR #209 shape): every push holds it with the GIL
    /// released — including while the managed transport sits in its
    /// reconnect loop — and `close()` fires `cancel` BEFORE taking it, so
    /// a push parked in a backoff ends with `SrtError(CLOSED)` instead of
    /// the close raising `RuntimeError: Already borrowed`. `Option` so
    /// `close()` / `__exit__` can drop the inner shell while keeping the
    /// PyClass addressable for idempotent closes.
    owned: Owned<RustMuxSender<ManagedTransport<SrtTransport>>>,
    /// Shared cancel state (Arc 2 WP-B2): the same `Arc` every
    /// `CancelHandle` this shell hands out holds, so `close()` here and
    /// `cancel()` through any handle flip one observable flag.
    cancel: Arc<CancelSource>,
    /// `ManagedHandles.attempts` — the CORE's own reconnect-attempt
    /// counter (A3), read by `reconnect_attempts()`. Every
    /// `ManagedTransport::reconnect_and_drain` retry tick bumps it inside
    /// the core, so the binding no longer wraps the factory to count.
    attempts: Arc<AtomicU64>,
    /// Reconnect/gap telemetry observer, snapshotted from the
    /// `ManagedTransport` BEFORE it moves into `RustMuxSender::new`
    /// (same precedent as `cancel_handle()` on the basic-bytes shells).
    stats_handle: tst_pipeline::ManagedStatsHandle,
}

#[pymethods]
impl PyManagedMuxSender {
    /// Build a `ManagedMuxSender` targeting `url` for the single-program
    /// configuration `program_config`. URL must specify `?mode=caller`.
    ///
    /// `policy` defaults to `ReconnectPolicy()` (matches Rust default —
    /// 10 attempts, exponential backoff 100ms..=10_000ms, gap buffer
    /// of 256 messages, drop-oldest overflow).
    ///
    /// Releases the GIL during the libsrt handshake. Raises
    /// `SrtError(CONFIG_INVALID)` on URL parse / bad-mode failure;
    /// `SrtError(CONNECT_FAILED)` / `SrtError(TIMEOUT)` on handshake
    /// failure; `MuxError(CONFIG_INVALID)` if the muxer construction
    /// rejects the program config.
    #[staticmethod]
    #[pyo3(signature = (url, program_config, *, policy = None))]
    fn from_url(
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
        policy: Option<PyReconnectPolicy>,
    ) -> PyResult<Self> {
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))?;

        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Caller {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "ManagedMuxSender.from_url requires ?mode=caller (default); got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        let policy_inner = policy.map(|p| p.inner).unwrap_or_default();
        // A3 owns the open, the reconnect factory (including the attempt
        // counter, which now lives on `ManagedTransport` itself) and the
        // handle snapshots. The managed family dials with `connect()` —
        // the sender preset — matching the C ABI.
        let (sender, handles, stats_handle) = py
            .allow_threads(|| {
                tst_srt::shells::managed_mux_sender_from_url(&parsed, policy_inner, muxer_cfg)
            })
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        let cancel = CancelSource::new(handles.cancel);
        Ok(Self {
            owned: Owned::new(sender, cancel.as_dyn(), ()),
            cancel,
            attempts: handles.attempts,
            stats_handle,
        })
    }

    // ── Send family — single-stream variants ──────────────────────────────

    /// Send one video access unit to the lone configured video stream.
    /// Annex-B framing for H.264/H.265/H.266; raw OBU stream for AV1.
    #[pyo3(signature = (nal, *, pts, key_frame = false))]
    fn send_video(
        &self,
        py: Python<'_>,
        nal: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        key_frame: bool,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, nal)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_video(slice, rust_pts, key_frame))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    /// Send one KLV blob to the lone configured KLV stream.
    #[pyo3(signature = (klv, *, pts, metadata_service_id = 0))]
    fn send_klv(
        &self,
        py: Python<'_>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        metadata_service_id: u8,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, klv)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_klv(slice, rust_pts, metadata_service_id))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    /// Send one encoded audio frame to the lone configured audio stream.
    #[pyo3(signature = (adts, *, pts))]
    fn send_audio(
        &self,
        py: Python<'_>,
        adts: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, adts)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.owned.with_mut(|s| s.send_audio(slice, rust_pts)));
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    /// Send one subtitle payload to the lone configured subtitle stream.
    #[pyo3(signature = (payload, *, pts))]
    fn send_subtitle(
        &self,
        py: Python<'_>,
        payload: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.owned.with_mut(|s| s.send_subtitle(slice, rust_pts)));
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    /// Send one data payload to the lone configured data stream.
    /// Pass-through: lands verbatim as one PES packet on stream_id 0xBD.
    #[pyo3(signature = (data, *, pts))]
    fn send_data(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.owned.with_mut(|s| s.send_data(slice, rust_pts)));
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    // ── Send family — handle-targeted variants ────────────────────────────

    #[pyo3(signature = (handle, nal, *, pts, key_frame = false))]
    fn send_video_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyVideoStreamHandle>,
        nal: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        key_frame: bool,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, nal)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_video_to(handle_inner, slice, rust_pts, key_frame))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    #[pyo3(signature = (handle, klv, *, pts, metadata_service_id = 0))]
    fn send_klv_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyKlvStreamHandle>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        metadata_service_id: u8,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, klv)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_klv_to(handle_inner, slice, rust_pts, metadata_service_id))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    #[pyo3(signature = (handle, adts, *, pts))]
    fn send_audio_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyAudioStreamHandle>,
        adts: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, adts)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_audio_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    #[pyo3(signature = (handle, payload, *, pts))]
    fn send_subtitle_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PySubtitleStreamHandle>,
        payload: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_subtitle_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    #[pyo3(signature = (handle, data, *, pts))]
    fn send_data_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyDataStreamHandle>,
        data: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_data_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
        }
    }

    // ── Handle getters ────────────────────────────────────────────────────

    fn video_handle(&self, py: Python<'_>) -> Option<PyVideoStreamHandle> {
        py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.video_handles().into_iter().next())
        })
        // `with_ref` RECOVERS a poisoned mutex and documents "never
        // Poisoned", so `.ok()` here only turns a CLOSED slot into
        // `None` — the same answer the pre-Arc-2 `with_slot` gave for an
        // empty slot.
        .ok()
        .flatten()
        .map(PyVideoStreamHandle)
    }

    fn klv_handle(&self, py: Python<'_>) -> Option<PyKlvStreamHandle> {
        py.allow_threads(|| self.owned.with_ref(|s| s.klv_handles().into_iter().next()))
            .ok()
            .flatten()
            .map(PyKlvStreamHandle)
    }

    fn audio_handle(&self, py: Python<'_>) -> Option<PyAudioStreamHandle> {
        py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.audio_handles().into_iter().next())
        })
        .ok()
        .flatten()
        .map(PyAudioStreamHandle)
    }

    fn subtitle_handle(&self, py: Python<'_>) -> Option<PySubtitleStreamHandle> {
        py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.subtitle_handles().into_iter().next())
        })
        .ok()
        .flatten()
        .map(PySubtitleStreamHandle)
    }

    fn data_handle(&self, py: Python<'_>) -> Option<PyDataStreamHandle> {
        py.allow_threads(|| self.owned.with_ref(|s| s.data_handles().into_iter().next()))
            .ok()
            .flatten()
            .map(PyDataStreamHandle)
    }

    // ── Stats ──────────────────────────────────────────────────────────────

    /// `(SocketStats, MuxerStats)` snapshot. Same shape as T5's
    /// `MuxSender.stats()`. `SocketStats` may report zeros while the
    /// transport is mid-reconnect (the inner socket is `None`).
    ///
    /// Waits (GIL released) for a push in flight on another thread: the
    /// slot lock is taken inside `allow_threads`, so holding the GIL while
    /// waiting for it can never freeze the interpreter.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        let (sock, pipe) = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|s| (s.socket_stats().unwrap_or_default(), s.stats()))
            }),
        )?;
        let mux_stats = tst_core::mpegts::mux::MuxerStats {
            ts_packets_emitted: pipe.packets_sent,
            ts_bytes_emitted: pipe.bytes_sent,
            programs_configured: pipe.programs_configured,
            subtitle_streams_configured: 0,
            per_stream: pipe.per_stream,
        };
        let sock_py = Py::new(py, PySocketStats::from_core(sock))?;
        let mux_py = Py::new(py, PyMuxerStats::from_inner(mux_stats))?;
        Ok((sock_py, mux_py))
    }

    /// Total number of times the reconnect factory has been invoked
    /// since construction. 0 means the initial connect is still live;
    /// rising values mean the inner SRT socket has been rebuilt (or a
    /// rebuild attempt failed and was retried).
    fn reconnect_attempts(&self) -> u64 {
        self.attempts.load(Ordering::Acquire)
    }

    /// Reconnect/gap telemetry: attempts, successes, current gap-buffer
    /// depth, and drop counters. Mirror of `ManagedSender.reconnect_stats`.
    ///
    /// Requires the sender not be closed (mirrors the CLOSED check
    /// every other managed getter runs); the counters themselves are
    /// readable independent of the inner transport's connect state.
    ///
    /// Raises `SrtError(IO)` if the internal gap-buffer lock is
    /// poisoned.
    fn reconnect_stats(&self, py: Python<'_>) -> PyResult<Py<PyManagedTransportStats>> {
        if self.owned.is_closed() {
            return Err(raise(py, &SRT, BindingError::from(HandleState::Closed)));
        }
        let stats = py
            .allow_threads(|| self.stats_handle.stats())
            .ok_or_else(|| {
                raise(
                    py,
                    &SRT,
                    BindingError::new(
                        BindingErrorKind::SrtIo,
                        "reconnect stats unavailable: gap lock poisoned",
                    ),
                )
            })?;
        Py::new(py, PyManagedTransportStats::from_core(stats))
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    /// Shareable cancel handle. `.cancel()` from any thread latches the
    /// managed transport's close flag and wakes a push parked anywhere in
    /// the reconnect loop (backoff wait, factory connect) or in the live
    /// send; that push raises `SrtError(CLOSED)`. Mirrors
    /// `ManagedSender.cancel_handle()`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Close the sender. Fires the managed cancel BEFORE taking the slot,
    /// so a push parked on another thread ends with `SrtError(CLOSED)`;
    /// then drops the managed transport (which closes the inner SRT
    /// socket). Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the sender holds a live transport (a push in flight on
    /// another thread counts as live).
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
        if !self.owned.is_closed() {
            format!(
                "ManagedMuxSender(open, reconnect_attempts={})",
                self.reconnect_attempts()
            )
        } else {
            "ManagedMuxSender(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyManagedDemuxReceiver — wraps ManagedDemuxReceiver<SrtTransport>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `ManagedDemuxReceiver` +
/// SRT transport with auto-reconnect.
///
/// Construct via `ManagedDemuxReceiver.from_url(url,
/// demux_config=DemuxerConfig(...), policy=ReconnectPolicy(...))`. URL
/// may specify `?mode=listener` (default for the receiver side) or
/// `?mode=caller`; in caller mode the wrapper will dial the configured
/// peer on each reconnect, in listener mode it re-binds and re-accepts.
///
/// On reconnect, the inner [`tst_pipeline::ManagedDemuxReceiver`] emits
/// a [`tstrans.mpegts.DemuxEvent.ReconnectDiscontinuity`] event before
/// any post-reconnect events. Consumers should drop per-stream caches on
/// receipt and rebuild from the next `ProgramMap` event.
///
/// `policy.mode` is send-side only: `ReconnectMode.BACKGROUND` on a
/// policy handed to `ManagedDemuxReceiver` logs a warning on the Rust
/// side and the receiver reconnects on the caller's thread anyway
/// (i.e. it behaves as `ReconnectMode.BLOCKING`).
///
/// `end_reason()` reports why the receive session ended
/// (`tstrans.srt.RecvEndReason`), or `None` while it is still live.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.srt import ManagedDemuxReceiver, ReconnectPolicy
///
/// with ManagedDemuxReceiver.from_url(
///     "srt://:7000?mode=listener", policy=ReconnectPolicy()
/// ) as rx:
///     for event in rx:
///         match event:
///             case DemuxEvent.ReconnectDiscontinuity():
///                 cache = {}  # rebuild on next ProgramMap
///             case DemuxEvent.Sample(...): ...
/// ```
#[pyclass(name = "ManagedDemuxReceiver", module = "tstrans.srt")]
pub(crate) struct PyManagedDemuxReceiver {
    /// Live receiver behind a mutex so a concurrent `__next__` /
    /// `close()` from different Python threads serialise cleanly.
    /// `Option` so `close()` can take + drop the inner shell.
    owned: Owned<RustManagedDemuxReceiver<SrtTransport>>,
    /// Cancel handle pulled from the receiver at construction. Held
    /// outside the mutex so `close()` can fire it BEFORE acquiring the
    /// lock — wakes any thread parked in `__next__`'s `recv_event`,
    /// which then drops the mutex guard and the close path can take
    /// ownership of `inner` cleanly.
    /// Shared cancel state (Arc 2 WP-B2): the same `Arc` every
    /// `CancelHandle` this shell hands out holds, so `close()` here and
    /// `cancel()` through any handle flip one observable flag.
    cancel: Arc<CancelSource>,
    /// `ManagedHandles.attempts` — the core's factory-invocation counter
    /// (Arc 2 ARCH-08). Symmetric with `PyManagedMuxSender`.
    attempts: Arc<AtomicU64>,
}

#[pymethods]
impl PyManagedDemuxReceiver {
    /// Bind (or connect) a managed receiver to `url`.
    ///
    /// `demux_config` is an optional `tstrans.mpegts.DemuxerConfig`
    /// dataclass; defaults are used when `None`. `policy` defaults to
    /// `ReconnectPolicy()` (matches Rust default).
    ///
    /// Raises `SrtError(CONFIG_INVALID)` on URL parse failure;
    /// `SrtError(CONNECT_FAILED)` on bind / connect failure;
    /// `SrtError(ACCEPT_FAILED)` / `SrtError(TIMEOUT)` on accept failure.
    #[staticmethod]
    #[pyo3(signature = (url, *, demux_config = None, policy = None))]
    fn from_url(
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
        policy: Option<PyReconnectPolicy>,
    ) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        // Translate the DemuxerConfig dataclass with the GIL held.
        let demux_opts = match demux_config {
            None => None,
            Some(cfg_obj) => Some(crate::mpegts::build_demuxer_config(py, cfg_obj)?),
        };
        let policy_inner = policy.map(|p| p.inner).unwrap_or_default();
        // A3 dispatches on `url.mode` (both modes are legal here), owns the
        // re-accept `FactoryCancel` slot the cancel handle fires, and
        // snapshots the end-reason handle before the shell move. The FIRST
        // accept stays uncancellable (DEBT-16, documented in python.md).
        let (receiver, handles) = py
            .allow_threads(|| {
                tst_srt::shells::managed_demux_receiver_from_url(
                    &parsed,
                    policy_inner,
                    demux_opts.unwrap_or_default(),
                )
            })
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        let cancel = CancelSource::new(handles.cancel);
        Ok(Self {
            owned: Owned::new(receiver, cancel.as_dyn(), ()).with_end_reason(handles.end_reason),
            cancel,
            attempts: handles.attempts,
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Block until the next `DemuxEvent` is available. Emits
    /// `DemuxEvent.ReconnectDiscontinuity` once after each transport
    /// reconnect; consumers should drop per-stream caches on receipt
    /// and rebuild from the next `ProgramMap`.
    ///
    /// Raises `StopIteration` on a clean end of stream — which includes an
    /// exhausted reconnect budget: the decorator's give-up reaches the shell
    /// as `EndOfStream`, so iteration ends cleanly and `end_reason()` reports
    /// `RecvEndReason.RECONNECT_EXHAUSTED`. Raises `SrtError` on a
    /// transport-side failure the decorator did not absorb (a cancel arrives
    /// as `SrtError(CLOSED)`); `DemuxError` on demuxer failure.
    fn __next__(&self, py: Python<'_>) -> PyResult<PyObject> {
        let res = py.allow_threads(|| self.owned.with_mut(|rx| rx.recv_event()));
        match res {
            Err(state) => Err(raise(py, &SRT, BindingError::from(state))),
            Ok(Ok(None)) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Ok(Ok(Some(ev))) => crate::mpegts::convert_event(py, &ev),
            Ok(Err(e)) => Err(demux_recv_err(py, e)),
        }
    }

    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Wire-level transport stats (RTT, bytes received, etc.) sourced
    /// from the underlying `ManagedRecvTransport::socket_stats`. Returns
    /// `SrtError(CLOSED)` if the receiver has been closed, or all-zero
    /// stats if the wrapper is mid-reconnect.
    ///
    /// Releases the GIL while acquiring the outer `Arc<Mutex<Option<...>>>`
    /// so a concurrent `__next__` parked in `recv_event` (which holds that
    /// same mutex inside `allow_threads`) cannot freeze the interpreter.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        // Two-step: acquire the outer mutex inside allow_threads so the GIL
        // is free while waiting. Without this, a parked __next__ holding the
        // mutex inside allow_threads would freeze all Python threads.
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

    /// SRT-specific stats. Same access pattern as `socket_stats` — peers
    /// the inner managed transport for its `SocketStats`, projects out
    /// the SRT-only fields. Today this returns the same `SocketStats`
    /// view as `socket_stats` because `ManagedRecvTransport` doesn't
    /// expose a separate SRT stats accessor — they're already in the
    /// `SocketStats` shape.
    fn srt_stats(&self, py: Python<'_>) -> PyResult<Py<PySocketStats>> {
        // Same as socket_stats today; reserved for future projection.
        self.socket_stats(py)
    }

    /// Total number of times the reconnect factory has been invoked
    /// since construction. Mirror of `ManagedMuxSender.reconnect_attempts`.
    fn reconnect_attempts(&self) -> u64 {
        self.attempts.load(Ordering::Acquire)
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
    /// Same access pattern as `socket_stats`: releases the GIL before
    /// acquiring the outer `Arc<Mutex<Option<...>>>` so a concurrent
    /// `__next__` parked in `recv_event` (holding that same mutex inside
    /// `allow_threads`) can't freeze the interpreter. Raises
    /// `SrtError(CLOSED)` if the receiver has been closed.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        let last_seen = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|rx| rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen))
            }),
        )?;
        Ok(last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64))
    }

    /// Why this receive session ended — a `tstrans.srt.RecvEndReason`
    /// member, or `None` while the stream is still live (or if it ended
    /// through a path this arc doesn't instrument).
    ///
    /// Recorded first-writer-wins by the underlying
    /// `tst_pipeline::ManagedDemuxReceiver`, so it survives `close()`:
    /// the handle was captured at construction, independent of `inner`.
    /// Reads a lock-free `OnceLock` cell — it never touches the `inner`
    /// mutex, so it is safe to call from a watchdog thread while another
    /// thread is parked in `__next__` (no `allow_threads` needed, and no
    /// GIL↔Mutex ordering to respect).
    ///
    /// Only two of the three variants are reachable on the managed-SRT
    /// path today: `RECONNECT_EXHAUSTED` (the reconnect budget ran out —
    /// a peer FIN arrives as a retryable break, so this is also what a
    /// peer close under a zero-retry policy reports) and `CANCELLED`
    /// (caller fired `cancel_handle()` or `close()`). `END_OF_STREAM` is
    /// reserved for a future transport that can signal a clean EOS
    /// distinct from budget exhaustion.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.owned.end_reason() {
            Some(r) => crate::srt::end_reason::recv_end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Close the receiver. Fires the cancel handle BEFORE acquiring the
    /// mutex so a concurrent `__next__` parked in `recv_event` unparks
    /// promptly. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the receiver holds a live shell (a `__next__` parked on
    /// another thread counts as live; the probe never waits).
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
            "ManagedDemuxReceiver(closed)".to_string()
        } else {
            format!(
                "ManagedDemuxReceiver(open, reconnect_attempts={})",
                self.reconnect_attempts()
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyManagedMuxSender>()?;
    m.add_class::<PyManagedDemuxReceiver>()?;
    Ok(())
}
