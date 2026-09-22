//! `MuxSender` convenience wrapper for SRT.
//!
//! Wraps `tst_pipeline::MuxSender<tst_srt::SrtTransport>`: build a libsrt
//! caller-mode `MuxSender` from a URL and a `MuxerProgramConfig` in a
//! single call, then push elementary streams through the muxer with each
//! call ending in an `SrtTransport::send_bytes` flush.
//!
//! 95% port of `bindings/python/src/rtp/mux_sender.rs` — the only
//! differences are:
//!
//! - Inner transport: `SrtTransport` instead of `RtpTransport`.
//! - URL dispatch: `SrtUrl::parse` + `Socket::connect_with` instead of
//!   `RtpSocketBuilder::from_url`. There is no `SrtTransport::from_url`
//!   helper, so it opens the same way `PySender::from_url` does
//!   (`SrtUrl::parse` → `SrtUrl::connect_recv` → wrap; the composition
//!   itself lives in `tst_srt`, so this module formats no address).
//! - Error mapping: the one raise path (`crate::raise`).
//!   `MuxSenderErrorSource::Mux` keeps `mux_error_to_pyerr` (it carries
//!   `.pid` and the `write_file` breadcrumb); every other source becomes a
//!   `BindingError` on the SRT domain — see `mux_sender_err`.
//! - Construction-time failures (URL parse, socket connect, muxer config)
//!   raise `SrtError(CONFIG_INVALID / CONNECT_FAILED / TIMEOUT)` rather
//!   than `RtpError(BROKEN)`.
//!
//! Architectural notes (mirror `rtp/mux_sender.rs`):
//!
//! - `tst_pipeline::binding::Owned` + `&self` methods (Arc 2, carrying
//!   the cross-thread close shape of PR #209): `close()` latches the
//!   shared cancel before taking the slot, and the slot's emptiness is
//!   what keeps the PyClass instance addressable for idempotent closes.
//! - Bytes-like extraction: fast `bytes` downcast, fallback through
//!   Python's `bytes()` builtin coercion (abi3-py310 two-path pattern).
//! - GIL release: every push method + `from_url` runs the underlying I/O
//!   under `py.allow_threads`. The `Py<PyBytes>` ref pinning the slice
//!   lives on the caller's Python frame, so GC can't collect it while we
//!   hold the borrowed `&[u8]`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;

use tst_pipeline::binding::{BindingError, BindingErrorKind, Owned};
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};
use tst_srt::{SrtTransport, SrtUrl, url::Mode};

use crate::errors::mux_error_to_pyerr;
use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig, PyMuxerStats,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::raise::{SRT, pyok, raise};
use crate::srt::transport::{PyCancelHandle, PySocketStats, srt_cancel_source};
use crate::util::{CancelSource, alive_probe, close_owned};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Mux-sourced failures keep `mux_error_to_pyerr` (it carries `.pid` and
/// the `write_file` breadcrumb); everything else is a `BindingError`
/// raised on the SRT domain.
///
/// `pub(crate)` so `managed_convenience`'s `ManagedMuxSender` shares the
/// one split instead of carrying a copy.
pub(crate) fn mux_sender_err(py: Python<'_>, e: MuxSenderError) -> PyErr {
    match e.source {
        MuxSenderErrorSource::Mux(mux_err) => mux_error_to_pyerr(py, mux_err),
        _ => raise(py, &SRT, BindingError::from(e)),
    }
}

// ---------------------------------------------------------------------------
// PyMuxSender — wraps tst_pipeline::MuxSender<SrtTransport>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `Muxer` + `SrtTransport`.
/// Construct with a libsrt URL (`srt://host:port?mode=caller&...`) and a
/// built `MuxerProgramConfig`; push elementary streams; the wrapper
/// assembles MPEG-TS packets and sends them through the SRT socket.
///
/// All push methods accept any bytes-like input (`bytes`, `bytearray`,
/// `memoryview`, NumPy `uint8` arrays) and release the GIL while the
/// muxer + transport work proceeds.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.srt import MuxSender
/// from tstrans.mpegts import (
///     MuxerProgramConfigBuilder, VideoCodec, Pts90khz,
/// )
///
/// program = (
///     MuxerProgramConfigBuilder(1, 0x100)
///     .add_video(0x101, VideoCodec.H264)
///     .build()
/// )
/// with MuxSender.from_url("srt://127.0.0.1:7000?mode=caller", program) as s:
///     s.push_video(b"\x00\x00\x00\x01\x09\xf0", pts=Pts90khz.from_raw(0))
/// ```
#[pyclass(name = "MuxSender", module = "tstrans.srt")]
pub(crate) struct PyMuxSender {
    /// The binding layer's handle state machine (Arc 2): a push holds the
    /// slot only inside `with_mut`, always under `py.allow_threads`, so a
    /// push in flight on another thread never trips PyO3's borrow check
    /// and `close()` (cancel-first) ends it instead of waiting behind it.
    owned: Owned<RustMuxSender<SrtTransport>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

impl PyMuxSender {
    /// Wrap an already-connected transport with a muxer built from
    /// `program_config` (the `from_url` and `Socket.into_mux_sender()`
    /// paths meet here).
    pub(crate) fn from_pipeline_mux(
        py: Python<'_>,
        transport: SrtTransport,
        program_config: &PyMuxerProgramConfig,
    ) -> PyResult<Self> {
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))?;
        let cancel = srt_cancel_source(&transport);
        let sender =
            RustMuxSender::new(transport, muxer_cfg).map_err(|e| mux_error_to_pyerr(py, e))?;
        Ok(Self {
            owned: Owned::new(sender, cancel.as_dyn(), ()),
            cancel,
        })
    }
}

#[pymethods]
impl PyMuxSender {
    /// Build a libsrt `MuxSender` targeting `url` for the single-program
    /// configuration `program_config`. The URL must specify
    /// `?mode=caller` (the SrtUrl default).
    ///
    /// Releases the GIL during the libsrt handshake (`srt_connect`) so
    /// other Python threads can run while this thread blocks on the
    /// network.
    ///
    /// Raises `SrtError(CONFIG_INVALID)` on URL parse / bad-mode failure;
    /// `SrtError(CONNECT_FAILED)` / `SrtError(TIMEOUT)` on handshake
    /// failure; `MuxError(CONFIG_INVALID)` if the muxer construction
    /// rejects the program config.
    #[staticmethod]
    fn from_url(
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<Self> {
        let parsed = SrtUrl::parse(url).map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        if parsed.mode != Mode::Caller {
            return Err(raise(
                py,
                &SRT,
                BindingError {
                    kind: BindingErrorKind::ConfigInvalid,
                    detail: format!(
                        "MuxSender.from_url requires ?mode=caller (default); got mode={:?}",
                        parsed.mode
                    ),
                },
            ));
        }
        // `connect_recv`, not `connect`: this open composed
        // `SocketConfig::default()` + the overlay with no
        // `merge_sender_defaults`, and keeping the preset off is the
        // behaviour-neutral re-point (see `srt::transport::PySender`).
        let transport = py
            .allow_threads(|| parsed.connect_recv())
            .map_err(|e| raise(py, &SRT, BindingError::from(e)))?;
        Self::from_pipeline_mux(py, transport, &program_config)
    }

    // ── Push family — single-stream variants ──────────────────────────────
    //
    // Mirror `bindings/python/src/rtp/mux_sender.rs` 1:1 for surface
    // consistency. Each method:
    //   - takes the payload bytes-like as the first positional arg,
    //   - takes `pts` keyword-only,
    //   - releases the GIL during the underlying push.

    /// Send one video access unit onto the lone configured video
    /// stream. Annex-B framing for H.264/H.265/H.266; raw OBU stream
    /// for AV1.
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

    /// Send one KLV blob onto the lone configured KLV stream.
    /// `metadata_service_id` defaults to 0 (single-service case).
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

    /// Send one encoded audio frame onto the lone configured audio
    /// stream. `frames` is one or more pre-framed audio frames
    /// concatenated by the caller (ADTS for AAC, MPEG-2 audio frames
    /// for MP2).
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

    /// Send one subtitle payload onto the lone configured subtitle
    /// stream.
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

    /// Send one data payload onto the lone configured data stream.
    ///
    /// Pass-through contract: no AU-cell wrap, no framing, no payload
    /// inspection — `data` lands verbatim as one PES packet on PES
    /// `stream_id` 0xBD (private_stream_1). `pts` is written into the
    /// PES header only when the stream was configured with
    /// `carries_pts=True`; it is always used for PSI/PCR pacing
    /// decisions regardless.
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

    /// Send to a specific video stream handle.
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

    /// Send to a specific KLV stream handle.
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

    /// Send to a specific audio stream handle.
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

    /// Send to a specific subtitle stream handle.
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

    /// Send to a specific data stream handle. Same pass-through
    /// contract as `send_data`.
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
    //
    // Single-program convenience — return the first configured handle
    // of each kind across all programs (which for our single-program
    // ctor is also the only program).

    /// First configured video stream handle, or `None`.
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

    /// First configured KLV stream handle, or `None`.
    fn klv_handle(&self, py: Python<'_>) -> Option<PyKlvStreamHandle> {
        py.allow_threads(|| self.owned.with_ref(|s| s.klv_handles().into_iter().next()))
            .ok()
            .flatten()
            .map(PyKlvStreamHandle)
    }

    /// First configured audio stream handle, or `None`.
    fn audio_handle(&self, py: Python<'_>) -> Option<PyAudioStreamHandle> {
        py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.audio_handles().into_iter().next())
        })
        .ok()
        .flatten()
        .map(PyAudioStreamHandle)
    }

    /// First configured subtitle stream handle, or `None`.
    fn subtitle_handle(&self, py: Python<'_>) -> Option<PySubtitleStreamHandle> {
        py.allow_threads(|| {
            self.owned
                .with_ref(|s| s.subtitle_handles().into_iter().next())
        })
        .ok()
        .flatten()
        .map(PySubtitleStreamHandle)
    }

    /// First configured data stream handle, or `None`.
    fn data_handle(&self, py: Python<'_>) -> Option<PyDataStreamHandle> {
        py.allow_threads(|| self.owned.with_ref(|s| s.data_handles().into_iter().next()))
            .ok()
            .flatten()
            .map(PyDataStreamHandle)
    }

    // ── Stats ──────────────────────────────────────────────────────────────

    /// Tuple of `(SocketStats, MuxerStats)`. `SocketStats` reflects the
    /// underlying SRT transport's wire-level counters; `MuxerStats`
    /// reflects the inner Rust `Muxer`'s programs / packets-emitted
    /// totals. Raises `SrtError(CLOSED)` if the sender has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        let (sock, pipe) = pyok(
            py,
            &SRT,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|s| (s.socket_stats().unwrap_or_default(), s.stats()))
            }),
        )?;
        // Project tst-pipeline's MuxSenderStats back onto the
        // tst-core::mpegts::stats::MuxerStats shape Python already
        // surfaces via `Muxer.stats()`. `subtitle_streams_configured`
        // isn't tracked by the pipeline shell (only by the inner
        // `Muxer`), so we default it to 0 — same approach as
        // `crate::rtp::mux_sender::PyMuxSender::stats`.
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

    // ── Lifecycle ──────────────────────────────────────────────────────────

    /// Shareable cancel handle. Calling `.cancel()` on the returned
    /// handle from any thread wakes a push parked in the transport; that
    /// push raises `SrtError(BROKEN | CLOSED)`. The sender itself stays
    /// addressable (`close()` still frees it). Mirrors
    /// `Sender.cancel_handle()` and the C ABI's `tst_mux_sender_cancel`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Close the sender. Fires the cancel handle BEFORE taking the slot,
    /// so a push in flight on another thread ends promptly with
    /// `SrtError(CLOSED | BROKEN)`; then drops the underlying SRT
    /// transport (the pipeline `MuxSender::close` is itself cancel-first —
    /// `finish()` is the lossless alternative on the Rust side). Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &SRT, &self.owned)
    }

    /// `True` while the sender owns a live transport (a push in flight on
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
        Ok(false) // do not suppress exceptions
    }

    fn __repr__(&self) -> String {
        if self.owned.is_closed() {
            "MuxSender(closed)".to_string()
        } else {
            "MuxSender(open)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMuxSender>()?;
    Ok(())
}
