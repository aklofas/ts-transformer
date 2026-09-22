//! Wave B Task 23 — `MuxSender` convenience wrapper.
//!
//! Wraps `tst_pipeline::MuxSender<tst_rtp::RtpTransport>`: build a UDP
//! RTP sender from a URL and a `MuxerProgramConfig` in a single call,
//! then push elementary streams through the muxer with each call
//! ending in a `RtpTransport::send_bytes` flush.
//!
//! Architectural notes (Stage 1 tst-c lesson #1: handles concrete
//! per-transport):
//!
//! - This PyClass wraps `MuxSender<RtpTransport>` directly — NOT
//!   `MuxSender<Box<dyn Transport>>`. Future SRT support lands as a
//!   separate `tstrans.srt.MuxSender` PyClass with its own concrete
//!   `MuxSender<SrtTransport>`.
//! - The push methods mirror the
//!   `bindings/python/src/rtp/server.rs::PyMountHandle` shape (single +
//!   `_to` variants × video/klv/audio/subtitle; data added by the W3
//!   private-data arc — not present on `PyMountHandle`).
//! - Bytes-like extraction follows the audit-backlog #10 two-path
//!   pattern (fast `bytes` downcast, fallback through Python's
//!   `bytes()` builtin coercion) — same shape as `PyMountHandle`.
//! - Each push releases the GIL via `py.allow_threads(|| ...)`. The
//!   `Py<PyBytes>` strong ref pinning the slice lives on the caller's
//!   Python frame, so GC can't collect it while we hold the borrowed
//!   `&[u8]` without the GIL.
//! - Concurrency (Arc 2):
//!   Every wrapper holds a `tst_pipeline::binding::Owned`, which takes
//!   the slot only inside `with_mut` / `with_ref` (GIL released) and
//!   makes `close()` cancel-first.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::Py;
use pyo3::prelude::*;

use tst_core::transport::Transport;
use tst_pipeline::binding::{BindingError, Owned};
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};
use tst_rtp::{RtpSocketBuilder, RtpTransport};

use crate::errors::mux_error_to_pyerr;
use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig, PyMuxerStats,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::raise::{RTP, pyok, raise};
use crate::rtp::transport::PySocketStats;
use crate::rtp::transport::rtp_cancel_source;
use crate::util::close_owned;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Mux-sourced failures keep `mux_error_to_pyerr` (it carries `.pid` and
/// the `write_file` breadcrumb); everything else is a `BindingError`
/// raised on the RTP domain.
fn mux_sender_err(py: Python<'_>, e: MuxSenderError) -> PyErr {
    match e.source {
        MuxSenderErrorSource::Mux(mux_err) => mux_error_to_pyerr(py, mux_err),
        _ => raise(py, &RTP, BindingError::from(e)),
    }
}

// ---------------------------------------------------------------------------
// PyMuxSender — wraps tst_pipeline::MuxSender<RtpTransport>.
// ---------------------------------------------------------------------------

/// Single-call convenience wrapper that owns a `Muxer` + `RtpTransport`.
/// Construct with a URL (`rtp://host:port`) and a built
/// `MuxerProgramConfig`; push elementary streams; the wrapper assembles
/// MPEG-TS packets and sends them over RTP/UDP.
///
/// All push methods accept any bytes-like input (`bytes`, `bytearray`,
/// `memoryview`, NumPy `uint8` arrays) and release the GIL while the
/// muxer + transport work proceeds.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.rtp import MuxSender
/// from tstrans.mpegts import (
///     MuxerProgramConfigBuilder, VideoCodec, Pts90khz,
/// )
///
/// program = (
///     MuxerProgramConfigBuilder(1, 0x100)
///     .add_video(0x101, VideoCodec.H264)
///     .build()
/// )
/// with MuxSender("rtp://127.0.0.1:5004", program) as s:
///     s.push_video(b"\x00\x00\x00\x01\x09\xf0", pts=Pts90khz.from_raw(0))
/// ```
#[pyclass(name = "MuxSender", module = "tstrans.rtp")]
pub struct PyMuxSender {
    /// Shared slot (PR #209 shape): every push holds it with the GIL
    /// released; `close()` fires `cancel` BEFORE taking it, so a push in
    /// flight on another thread ends with `RtpError(CLOSED)` instead
    /// of the close raising `RuntimeError: Already borrowed`. `Option` so
    /// `close()` / `__exit__` can drop the inner sender while keeping the
    /// PyClass addressable for repeated no-op closes.
    owned: Owned<RustMuxSender<RtpTransport>>,
}

#[pymethods]
impl PyMuxSender {
    /// Build an RTP `MuxSender` targeting `url` for the single-program
    /// configuration `program_config`. `pkt_size` overrides the UDP
    /// datagram payload size (default 1316 = 7 × 188 TS packets, sized
    /// to stay under the typical Ethernet MTU minus IP+UDP+RTP header).
    ///
    /// Raises `RtpError(CLOSED)` on URL parse / socket bind failure;
    /// `MuxError(CONFIG_INVALID)` if the muxer construction rejects the
    /// program config.
    #[new]
    #[pyo3(signature = (url, program_config, *, pkt_size = 1316))]
    fn new(
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
        pkt_size: usize,
    ) -> PyResult<Self> {
        // 1. Wrap the single MuxerProgramConfig in a MuxerConfig.
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))?;

        // 2. Build the RTP transport from the URL + pkt_size.
        let mut sock_builder = RtpSocketBuilder::from_url(url)
            .map_err(|e| raise(py, &RTP, BindingError::from(tst_rtp::ConnectError::from(e))))?;
        sock_builder.pkt_size(pkt_size);
        let transport = sock_builder
            .build()
            .map_err(|e| raise(py, &RTP, BindingError::from(e)))?;
        // Pulled BEFORE the transport moves into the shell.
        let cancel = rtp_cancel_source(py, Transport::cancel_handle(&transport))?;

        // 3. Hand transport + config to the pipeline shell.
        let sender =
            RustMuxSender::new(transport, muxer_cfg).map_err(|e| mux_error_to_pyerr(py, e))?;
        // The `CancelSource` lives on inside `Owned` (it is the shell's
        // `Arc<dyn TransportCancel>`), so `close()` still cancels first.
        // No field: the rtp `MuxSender` exposes no `cancel_handle()` — a
        // documented parity gap vs the srt twin, so nothing reads it back.
        Ok(Self {
            owned: Owned::new(sender, cancel.as_dyn(), ()),
        })
    }

    // ── Push family — single-stream variants ──────────────────────────────
    //
    // Mirror `bindings/python/src/rtp/server.rs::PyMountHandle` 1:1 for
    // surface consistency. Each method:
    //   - takes the payload bytes-like as the first positional arg,
    //   - takes `pts` keyword-only (audit #9 normalization),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
        // Note: `tst_pipeline::MuxSender::send_klv` takes
        // (klv, pts, metadata_service_id). The single-stream convenience
        // routes through the muxer's auto-target dispatcher.
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|s| s.send_klv(slice, rust_pts, metadata_service_id))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, e)),
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
            Err(state) => Err(raise(py, &RTP, BindingError::from(state))),
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
    /// underlying RTP transport's wire-level counters; `MuxerStats`
    /// reflects the inner Rust `Muxer`'s programs / packets-emitted
    /// totals. Raises `RtpError(CLOSED)` if the sender is closed.
    /// Waits (GIL released) for a push in flight on another thread.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<PySocketStats>, Py<PyMuxerStats>)> {
        let (sock, pipe) = pyok(
            py,
            &RTP,
            py.allow_threads(|| {
                self.owned
                    .with_ref(|s| (s.socket_stats().unwrap_or_default(), s.stats()))
            }),
        )?;
        // Project tst-pipeline's MuxSenderStats back onto the
        // tst-core::mpegts::stats::MuxerStats shape Python already
        // surfaces via `Muxer.stats()`. `subtitle_streams_configured`
        // isn't tracked by the pipeline shell (only by the inner
        // `Muxer`), so we default it to 0.
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

    /// Close the sender. Fires the transport's cancel BEFORE taking the
    /// slot (a push in flight on another thread ends with
    /// `RtpError(CLOSED)`), then drops the underlying RTP transport
    /// (the pipeline `MuxSender::close` is itself cancel-first). Idempotent.
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
        Ok(false) // do not suppress exceptions
    }

    fn __repr__(&self) -> String {
        if !self.owned.is_closed() {
            "MuxSender(open)".to_string()
        } else {
            "MuxSender(closed)".to_string()
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
