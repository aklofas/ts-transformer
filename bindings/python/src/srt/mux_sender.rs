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
//!   helper, so we replicate the T2 `PySender::from_url` construction
//!   pattern (parse → SocketConfig → Socket::connect_with → wrap).
//! - Error mapping: `crate::srt::errors::*` helpers instead of
//!   `crate::rtp::errors::*`. `MuxSenderErrorSource::Transport` collapses
//!   to `SrtError` (BROKEN / CLOSED / WOULD_BLOCK / CONFIG_INVALID / IO
//!   per `TransportError` variant — same mapping as `PySender::send_bytes`).
//! - Construction-time failures (URL parse, socket connect, muxer config)
//!   raise `SrtError(CONFIG_INVALID / CONNECT_FAILED / TIMEOUT)` rather
//!   than `RtpError(TRANSPORT)`.
//!
//! Architectural notes (mirror `rtp/mux_sender.rs`):
//!
//! - `Arc<Mutex<Option<_>>>` slot + `&self` methods (the cross-thread
//!   close shape of PR #209): `close()` cancels before taking the slot.
//! - `Option` so `close()` / `__exit__` can drop the inner sender while
//!   keeping the PyClass instance addressable for idempotent closes.
//! - Bytes-like extraction: fast `bytes` downcast, fallback through
//!   Python's `bytes()` builtin coercion (abi3-py310 two-path pattern).
//! - GIL release: every push method + `from_url` runs the underlying I/O
//!   under `py.allow_threads`. The `Py<PyBytes>` ref pinning the slice
//!   lives on the caller's Python frame, so GC can't collect it while we
//!   hold the borrowed `&[u8]`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;

use tst_core::transport::TransportCancel;
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};
use tst_srt::{Socket, SocketConfig, SrtTransport, SrtUrl, url::Mode};

use crate::errors::{make_srt_error, mux_error_to_pyerr};
use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig, PyMuxerStats,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::srt::errors::{connect_error_to_pyerr, transport_error_to_pyerr, url_error_to_pyerr};
use crate::srt::transport::{PyCancelHandle, PySocketStats};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a `MuxSenderError` raised by any of `send_*` to a Python
/// exception. `Mux(...)` variants surface as `MuxError`; `Transport(...)`
/// variants surface as `SrtError` (BROKEN / CLOSED / WOULD_BLOCK /
/// CONFIG_INVALID / IO per `TransportError` discriminant).
fn mux_sender_error_to_pyerr(py: Python<'_>, e: MuxSenderError) -> PyErr {
    match e.source {
        MuxSenderErrorSource::Mux(mux_err) => mux_error_to_pyerr(py, mux_err),
        MuxSenderErrorSource::Transport(t) => transport_error_to_pyerr(py, t),
        // `MuxSenderErrorSource` is `#[non_exhaustive]`; route any
        // future variant to a generic SrtError(IO) with the
        // free-text Display message preserved.
        _ => make_srt_error(py, "IO", &format!("{:?}", e.kind)),
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
    /// Shared slot: every push holds it (GIL released) for the duration of
    /// the muxer + transport work; `close()` fires `cancel` BEFORE taking
    /// it, so a push in flight on another thread ends with
    /// `SrtError(CLOSED | BROKEN)` instead of the close tripping PyO3's
    /// borrow check (`RuntimeError: Already borrowed`). `Option` so
    /// `close()` / `__exit__` can drop the inner sender while keeping the
    /// PyClass addressable for idempotent closes.
    inner: Arc<Mutex<Option<RustMuxSender<SrtTransport>>>>,
    /// `tst_pipeline::MuxSender::cancel_handle()` snapshot — always
    /// `Some` for an `SrtTransport` with a live socket. Exposed through
    /// `cancel_handle()` and fired first by `close()`.
    cancel: Arc<dyn TransportCancel + Send + Sync>,
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
        // 1. Wrap the single MuxerProgramConfig in a MuxerConfig.
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))?;

        // 2. Parse the URL + build the libsrt transport.
        let parsed = SrtUrl::parse(url).map_err(|e| url_error_to_pyerr(py, e))?;
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "MuxSender.from_url requires ?mode=caller (default); got mode={:?}",
                parsed.mode
            );
            return Err(make_srt_error(py, "CONFIG_INVALID", &msg));
        }
        let mut sock_cfg = SocketConfig::default();
        parsed.overlay.apply_to_socket(&mut sock_cfg);
        let addr = crate::util::join_host_port(&parsed.host, parsed.port);
        let socket = py
            .allow_threads(|| Socket::connect_with(&sock_cfg, addr.as_str()))
            .map_err(|e| connect_error_to_pyerr(py, e))?;
        let transport = SrtTransport::new(socket);

        // 3. Hand transport + config to the pipeline shell.
        let sender =
            RustMuxSender::new(transport, muxer_cfg).map_err(|e| mux_error_to_pyerr(py, e))?;
        let cancel = sender
            .cancel_handle()
            .expect("SrtTransport with a live socket always returns Some(cancel_handle)");
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(sender))),
            cancel,
        })
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_video(slice, rust_pts, key_frame)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_klv(slice, rust_pts, metadata_service_id)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| s.send_audio(slice, rust_pts))
            .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
            .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| s.send_subtitle(slice, rust_pts))
            .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
            .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| s.send_data(slice, rust_pts))
            .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
            .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_video_to(handle_inner, slice, rust_pts, key_frame)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_klv_to(handle_inner, slice, rust_pts, metadata_service_id)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_audio_to(handle_inner, slice, rust_pts)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_subtitle_to(handle_inner, slice, rust_pts)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
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
        crate::util::with_slot(py, &self.inner, |s| {
            s.send_data_to(handle_inner, slice, rust_pts)
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?
        .map_err(|e| mux_sender_error_to_pyerr(py, e))
    }

    // ── Handle getters ────────────────────────────────────────────────────
    //
    // Single-program convenience — return the first configured handle
    // of each kind across all programs (which for our single-program
    // ctor is also the only program).

    /// First configured video stream handle, or `None`.
    fn video_handle(&self, py: Python<'_>) -> Option<PyVideoStreamHandle> {
        crate::util::with_slot(py, &self.inner, |s| s.video_handles().into_iter().next())
            .flatten()
            .map(PyVideoStreamHandle)
    }

    /// First configured KLV stream handle, or `None`.
    fn klv_handle(&self, py: Python<'_>) -> Option<PyKlvStreamHandle> {
        crate::util::with_slot(py, &self.inner, |s| s.klv_handles().into_iter().next())
            .flatten()
            .map(PyKlvStreamHandle)
    }

    /// First configured audio stream handle, or `None`.
    fn audio_handle(&self, py: Python<'_>) -> Option<PyAudioStreamHandle> {
        crate::util::with_slot(py, &self.inner, |s| s.audio_handles().into_iter().next())
            .flatten()
            .map(PyAudioStreamHandle)
    }

    /// First configured subtitle stream handle, or `None`.
    fn subtitle_handle(&self, py: Python<'_>) -> Option<PySubtitleStreamHandle> {
        crate::util::with_slot(py, &self.inner, |s| s.subtitle_handles().into_iter().next())
            .flatten()
            .map(PySubtitleStreamHandle)
    }

    /// First configured data stream handle, or `None`.
    fn data_handle(&self, py: Python<'_>) -> Option<PyDataStreamHandle> {
        crate::util::with_slot(py, &self.inner, |s| s.data_handles().into_iter().next())
            .flatten()
            .map(PyDataStreamHandle)
    }

    // ── Stats ──────────────────────────────────────────────────────────────

    /// Tuple of `(SocketStats, MuxerStats)`. `SocketStats` reflects the
    /// underlying SRT transport's wire-level counters; `MuxerStats`
    /// reflects the inner Rust `Muxer`'s programs / packets-emitted
    /// totals. Raises `SrtError(CLOSED)` if the sender has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        let (sock, pipe) = crate::util::with_slot(py, &self.inner, |s| {
            (s.socket_stats().unwrap_or_default(), s.stats())
        })
        .ok_or_else(|| make_srt_error(py, "CLOSED", "MuxSender is closed"))?;
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
        Py::new(py, PyCancelHandle::from_arc(self.cancel.clone()))
    }

    /// Close the sender. Fires the cancel handle BEFORE taking the slot,
    /// so a push in flight on another thread ends promptly with
    /// `SrtError(CLOSED | BROKEN)`; then drops the underlying SRT
    /// transport (the pipeline `MuxSender::close` is itself cancel-first —
    /// `finish()` is the lossless alternative on the Rust side). Idempotent.
    fn close(&self, py: Python<'_>) {
        self.cancel.cancel();
        crate::util::close_slot(py, &self.inner, |s| s.close());
    }

    /// `True` while the sender owns a live transport (a push in flight on
    /// another thread counts as live).
    fn is_alive(&self) -> bool {
        crate::util::slot_alive(&self.inner, |s| s.is_alive())
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
        false // do not suppress exceptions
    }

    fn __repr__(&self) -> String {
        if crate::util::slot_alive(&self.inner, |_| true) {
            "MuxSender(open)".to_string()
        } else {
            "MuxSender(closed)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-task helpers used by T3 (`lowlevel.rs`) to promote a low-level
// connected `Socket` into a `PyMuxSender`.
// ---------------------------------------------------------------------------

impl PyMuxSender {
    /// Build a `PyMuxSender` from a connected libsrt `Socket` + a
    /// `MuxerProgramConfig`. Used by `Socket::into_mux_sender` (T3) so
    /// the Builder→Socket→MuxSender promotion path doesn't have to know
    /// about T5's internal field shape.
    pub(crate) fn from_pipeline_mux(
        py: Python<'_>,
        socket: Socket,
        program_config: &PyMuxerProgramConfig,
    ) -> PyResult<Self> {
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))?;
        let transport = SrtTransport::new(socket);
        let sender =
            RustMuxSender::new(transport, muxer_cfg).map_err(|e| mux_error_to_pyerr(py, e))?;
        let cancel = sender
            .cancel_handle()
            .expect("SrtTransport with a live socket always returns Some(cancel_handle)");
        Ok(Self {
            inner: Arc::new(Mutex::new(Some(sender))),
            cancel,
        })
    }
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMuxSender>()?;
    Ok(())
}
