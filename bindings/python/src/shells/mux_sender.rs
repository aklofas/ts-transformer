//! `MuxSender`: push elementary streams, any transport out.
//!
//! Wraps `tst_pipeline::MuxSender<Box<dyn Transport>>`, opened from a URL and
//! a `MuxerProgramConfig` by `MuxSender.open(url, program_config)`; each
//! push ends in a transport send.
//!
//! - `tst_pipeline::binding::Owned` + `&self` methods (the cross-thread
//!   close shape): `close()` latches the shared cancel before taking the
//!   slot, and the slot's emptiness is what keeps the PyClass instance
//!   addressable for idempotent closes.
//! - The transport's error domain rides in the snapshot (next to the
//!   stream handles), so it stays readable after `close()`.
//! - Mux-sourced failures keep `MuxError` (it carries `.pid` and the
//!   `write_file` breadcrumb); every other source is raised in the
//!   shell's transport domain.
//! - Bytes-like extraction: fast `bytes` downcast, fallback through
//!   Python's `bytes()` builtin coercion (the two-path pattern).
//! - GIL release: every push method and `open` run the underlying I/O
//!   under `py.detach`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyType;

use tst_core::transport::Transport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot, StreamHandles};
use tst_pipeline::binding::{BindingError, Domain, HandleState, Opened, Owned};
use tst_pipeline::{MuxSender as RustMuxSender, MuxSenderError, MuxSenderErrorSource};

use crate::errors::mux_error_to_pyerr;
use crate::generated::stats_mpegts::MuxerStats;
use crate::generated::stats_shells::SocketStats;
use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::raise::{pyok, raise};
use crate::shells::cancel::{PyCancelHandle, cancel_source};
use crate::util::{CancelSource, close_owned, drop_owned};

/// Mux-sourced failures keep `mux_error_to_pyerr`; everything else is a
/// `BindingError` raised in `domain`.
pub(crate) fn mux_sender_err(py: Python<'_>, domain: &Domain, e: MuxSenderError) -> PyErr {
    match e.source {
        MuxSenderErrorSource::Mux(mux_err) => mux_error_to_pyerr(py, mux_err),
        _ => raise(py, domain, BindingError::from(e)),
    }
}

/// Python mux sender over any transport: owns a `Muxer` and a transport.
/// Open it with a URL and a built `MuxerProgramConfig`, push elementary
/// streams, and the wrapper assembles MPEG-TS packets and sends them.
///
/// All push methods accept any bytes-like input (`bytes`, `bytearray`,
/// `memoryview`, NumPy `uint8` arrays) and release the GIL while the
/// muxer and transport work proceeds.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.shells import MuxSender
/// from tstrans.mpegts import (
///     MuxerProgramConfigBuilder, VideoCodec, Pts90khz,
/// )
///
/// program = (
///     MuxerProgramConfigBuilder(1, 0x100)
///     .add_video(0x101, VideoCodec.H264)
///     .build()
/// )
/// with MuxSender.open("srt://127.0.0.1:7000?mode=caller", program) as s:
///     s.send_video(b"\x00\x00\x00\x01\x09\xf0", pts=Pts90khz.from_raw(0))
/// ```
#[pyclass(name = "MuxSender", module = "tstrans.shells")]
pub(crate) struct PyMuxSender {
    /// The binding layer's handle state machine: a push holds the slot
    /// only inside `with_mut`, always under `py.detach`, so a push
    /// in flight on another thread never trips PyO3's borrow check and
    /// `close()` (cancel-first) ends it instead of waiting behind it.
    owned: Owned<RustMuxSender<Box<dyn Transport>>, Snapshot<StreamHandles>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
}

impl Drop for PyMuxSender {
    fn drop(&mut self) {
        drop_owned(&self.owned.snapshot().domain, &self.owned);
    }
}

impl PyMuxSender {
    /// Wrap an already-opened transport with a muxer built from
    /// `program_config` (the `open` path).
    pub(crate) fn from_opened(
        py: Python<'_>,
        opened: Opened<Box<dyn Transport>>,
        program_config: &PyMuxerProgramConfig,
    ) -> PyResult<Self> {
        let muxer_cfg = Self::muxer_config_for(py, program_config)?;
        Self::from_opened_with_config(py, opened, muxer_cfg)
    }

    /// Build the single-program `MuxerConfig` for `program_config`. A
    /// consuming conversion calls this before it takes its source handle,
    /// so a refused config leaves the source usable.
    pub(crate) fn muxer_config_for(
        py: Python<'_>,
        program_config: &PyMuxerProgramConfig,
    ) -> PyResult<tst_core::mpegts::mux::MuxerConfig> {
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        cfg_builder.build().map_err(|e| mux_error_to_pyerr(py, e))
    }

    /// Wrap an already-opened transport with a muxer built from an
    /// already-validated `muxer_cfg` (the `Socket.into_mux_sender()` path).
    pub(crate) fn from_opened_with_config(
        py: Python<'_>,
        opened: Opened<Box<dyn Transport>>,
        muxer_cfg: tst_core::mpegts::mux::MuxerConfig,
    ) -> PyResult<Self> {
        let Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        let sender =
            RustMuxSender::new(transport, muxer_cfg).map_err(|e| mux_error_to_pyerr(py, e))?;
        // Read before the sender moves into the slot: the handle getters
        // answer from this, never from the slot a parked send holds.
        let handles = StreamHandles::of(&sender);
        Ok(Self {
            owned: Owned::new(sender, cancel.as_dyn(), Snapshot { domain, handles }),
            cancel,
        })
    }
}

#[pymethods]
impl PyMuxSender {
    /// Open a `MuxSender` on `url` for the single-program configuration
    /// `program_config`. Releases the GIL during the connect.
    ///
    /// Raises `ValueError` for an unsupported scheme; the transport's error
    /// class for a bad URL or a connect failure; `MuxError(CONFIG_INVALID)`
    /// if the muxer rejects the program config.
    #[classmethod]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<Self> {
        let opened = crate::shells::open::open_send(py, url)?;
        Self::from_opened(py, opened, &program_config)
    }

    /// Deprecated: use `open`.
    #[staticmethod]
    fn from_url(
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "`MuxSender.from_url` is deprecated; use `MuxSender.open(url, program_config)`",
        )?;
        let opened = crate::shells::open::open_send(py, url)?;
        Self::from_opened(py, opened, &program_config)
    }

    /// Deprecated: use `MuxSender.open(url, program_config)`. `pkt_size`
    /// becomes the `?pkt_size=` URL key when given.
    #[new]
    #[pyo3(signature = (url, program_config, *, pkt_size = None))]
    fn new(
        py: Python<'_>,
        url: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
        pkt_size: Option<usize>,
    ) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "constructing `MuxSender(url, ...)` is deprecated; use `MuxSender.open(url, ...)`, with `pkt_size` as the `?pkt_size=` URL key",
        )?;
        let url = match pkt_size {
            Some(n) => crate::util::with_query(url, "pkt_size", n),
            None => url.to_owned(),
        };
        let opened = crate::shells::open::open_send(py, &url)?;
        Self::from_opened(py, opened, &program_config)
    }
    // ── Push family — single-stream variants ──────────────────────────────
    //
    // Mirror `bindings/python/src/shells/mux_sender.rs` 1:1 for surface
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_video(slice, rust_pts, key_frame))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_klv(slice, rust_pts, metadata_service_id))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut(|s| s.send_audio(slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut(|s| s.send_subtitle(slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut(|s| s.send_data(slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_video_to(handle_inner, slice, rust_pts, key_frame))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_klv_to(handle_inner, slice, rust_pts, metadata_service_id))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_audio_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_subtitle_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
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
        let res = crate::util::allow_threads_parking(py, || {
            self.owned
                .with_mut(|s| s.send_data_to(handle_inner, slice, rust_pts))
        });
        match res {
            Ok(r) => r.map_err(|e| mux_sender_err(py, &self.owned.snapshot().domain, e)),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
        }
    }

    // ── Handle getters ────────────────────────────────────────────────────
    //
    // Single-program convenience — return the first configured handle
    // of each kind across all programs (which for our single-program
    // ctor is also the only program).
    //
    // Answered from the construction-time snapshot, never the slot: the
    // stream set cannot change after the sender is built, so these do not
    // wait behind a `send_*` parked on another thread. A closed sender
    // answers `None`.

    /// First configured video stream handle, or `None`.
    fn video_handle(&self) -> Option<PyVideoStreamHandle> {
        self.owned
            .open_snapshot()
            .and_then(|s| s.handles.video)
            .map(PyVideoStreamHandle)
    }

    /// First configured KLV stream handle, or `None`.
    fn klv_handle(&self) -> Option<PyKlvStreamHandle> {
        self.owned
            .open_snapshot()
            .and_then(|s| s.handles.klv)
            .map(PyKlvStreamHandle)
    }

    /// First configured audio stream handle, or `None`.
    fn audio_handle(&self) -> Option<PyAudioStreamHandle> {
        self.owned
            .open_snapshot()
            .and_then(|s| s.handles.audio)
            .map(PyAudioStreamHandle)
    }

    /// First configured subtitle stream handle, or `None`.
    fn subtitle_handle(&self) -> Option<PySubtitleStreamHandle> {
        self.owned
            .open_snapshot()
            .and_then(|s| s.handles.subtitle)
            .map(PySubtitleStreamHandle)
    }

    /// First configured data stream handle, or `None`.
    fn data_handle(&self) -> Option<PyDataStreamHandle> {
        self.owned
            .open_snapshot()
            .and_then(|s| s.handles.data)
            .map(PyDataStreamHandle)
    }

    // ── Stats ──────────────────────────────────────────────────────────────

    /// Tuple of `(SocketStats, MuxerStats)`. `SocketStats` reflects the
    /// underlying transport's wire-level counters; `MuxerStats`
    /// reflects the inner Rust `Muxer`'s programs / packets-emitted
    /// totals. Raises the transport's `CLOSED` kind if the sender has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<SocketStats>, Py<MuxerStats>)> {
        let (sock, pipe) = pyok(
            py,
            &self.owned.snapshot().domain,
            crate::util::allow_threads_parking(py, || {
                self.owned
                    // counters live inside the shell and have no value to report while a parked call holds it
                    .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |s| {
                        (s.socket_stats().unwrap_or_default(), s.stats())
                    })
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
        let sock_py = Py::new(py, SocketStats::from(&sock))?;
        let mux_py = Py::new(py, MuxerStats::from(&mux_stats))?;
        Ok((sock_py, mux_py))
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    /// Shareable cancel handle. Calling `.cancel()` on the returned
    /// handle from any thread wakes a push parked in the transport; that
    /// push raises the transport's `CLOSED` kind. The sender itself stays
    /// addressable (`close()` still frees it). Same as
    /// `Sender.cancel_handle()` and the C ABI's `tst_mux_sender_cancel`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Close the sender. Fires the cancel handle BEFORE taking the slot,
    /// so a push in flight on another thread ends promptly with
    /// the transport's `CLOSED` kind; then drops the underlying
    /// transport (the pipeline `MuxSender::close` is itself cancel-first —
    /// `finish()` is the lossless alternative on the Rust side). Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)
    }

    /// Drain every byte the muxer still holds to the transport, raise the
    /// first drain error, then close the transport — the lossless
    /// counterpart to `close()`. The sender is closed either way; a second
    /// `finish()` is a no-op, and so is a `finish()` after `close()`.
    ///
    /// Unlike `close()` this does NOT cancel first: a push parked on
    /// another thread keeps the slot and `finish()` waits behind it. Fire
    /// `cancel_handle().cancel()` (or `close()`) first if that is not
    /// wanted.
    fn finish(&self, py: Python<'_>) -> PyResult<()> {
        // lossless shutdown: drains the muxer through the transport, documented to wait behind a send in flight
        match crate::util::allow_threads_parking(py, || {
            self.owned
                .with_ref_blocking(BlockingReason::IsItselfBlocking, |s| s.finish())
        }) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(mux_sender_err(py, &self.owned.snapshot().domain, e)),
            // Already closed: the slot is empty, which is the same answer
            // a second `finish()` gets from the shell itself — stay quiet.
            Err(HandleState::Closed) => Ok(()),
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
        }
    }

    /// `True` while the sender owns a live transport (a push in flight on
    /// another thread counts as live).
    fn is_alive(&self) -> bool {
        self.owned.alive(|s| s.is_alive())
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
