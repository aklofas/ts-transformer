//! Plan A5b Wave C T11 — `MuxPublisher` shell (concrete `HlsPublisher`).
//!
//! `tstrans.hls.MuxPublisher` wraps
//! `tst_pipeline::MuxPublisher<tst_hls::HlsPublisher>`: it owns a
//! `Muxer` + an `HlsPublisher`, accepts elementary streams
//! (video / klv / audio / subtitle), muxes them into MPEG-TS, and pushes
//! the resulting bytes into the HLS sink (which segments + serves them).
//!
//! Design note (deviation from plan T11): the plan sketched a generic
//! `PyBridgePublisher` adapting *any* Python `Publisher` subclass back to
//! the Rust `Publisher` trait via per-call GIL acquisition. The shipped
//! design monomorphizes over the concrete `HlsPublisher` (the only
//! publisher impl that exists). This matches the Stage-1 tst-c lesson
//! (handles are concrete per-transport, never `Box<dyn ...>`) and the
//! `rtp/mux_sender.rs::PyMuxSender` template (concrete
//! `MuxSender<RtpTransport>`). A generic Python-bridge publisher can be
//! added later if a use case lands; it is not needed for HLS.
//!
//! `with_config_hls(publisher, program_config)` *consumes* the
//! `HlsPublisher` (moves its inner out of the `Option`); the source
//! handle becomes closed. `finish_into_publisher()` consumes the
//! `MuxPublisher` and returns a fresh `HlsPublisher` wrapping the inner
//! publisher so the caller can still `finish()` / `render_playlist()` /
//! `local_addr()` it.
//!
//! GIL: each `send_*` releases the GIL via `py.allow_threads` (mirrors
//! `rtp/mux_sender.rs`) — the inner muxer + HLS disk writes are pure
//! Rust and never re-enter Python.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Mutex;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use tst_hls::HlsPublisher;
use tst_pipeline::{MuxPublisher as RustMuxPublisher, MuxPublisherError};

use crate::hls::map_mux_publisher_error;
use crate::hls::publisher::PyHlsPublisher;
use crate::hls::publisher_abc::PyPublisherStats;
use crate::mux::{PyMuxerProgramConfig, py_pts90khz};
use crate::raise::{HLS, raise};
use tst_pipeline::binding::{BindingError, BindingErrorKind};

// ---------------------------------------------------------------------------
// MuxPublisherStats — frozen mirror of tst_pipeline::MuxPublisherStats
// ---------------------------------------------------------------------------

/// Cumulative `MuxPublisher` shell stats (`tstrans.hls.MuxPublisherStats`).
///
/// Mirrors `tst_pipeline::MuxPublisherStats`.
#[pyclass(name = "MuxPublisherStats", module = "tstrans.hls", frozen, get_all)]
#[derive(Clone)]
pub(crate) struct PyMuxPublisherStats {
    /// Total TS bytes drained from the muxer and handed to the publisher.
    pub bytes_pushed: u64,
    /// Total muxer drain calls that produced at least one chunk.
    pub drain_calls: u64,
    /// Total explicit `cut_segment()` calls (plus auto-cuts on keyframes).
    pub cut_calls: u64,
}

impl From<tst_pipeline::MuxPublisherStats> for PyMuxPublisherStats {
    fn from(s: tst_pipeline::MuxPublisherStats) -> Self {
        Self {
            bytes_pushed: s.bytes_pushed,
            drain_calls: s.drain_calls,
            cut_calls: s.cut_calls,
        }
    }
}

#[pymethods]
impl PyMuxPublisherStats {
    fn __repr__(&self) -> String {
        format!(
            "MuxPublisherStats(bytes_pushed={}, drain_calls={}, cut_calls={})",
            self.bytes_pushed, self.drain_calls, self.cut_calls,
        )
    }
}

// ---------------------------------------------------------------------------
// PyMuxPublisher — wraps MuxPublisher<HlsPublisher>
// ---------------------------------------------------------------------------

/// Owns a `Muxer` + an `HlsPublisher`; push elementary streams, the
/// shell muxes to MPEG-TS and feeds the HLS sink.
///
/// Construct via `MuxPublisher.with_config_hls(publisher, program_config)`
/// — which *consumes* the `HlsPublisher`. Recover the publisher (e.g. to
/// `finish()` it cleanly) via `finish_into_publisher()`.
///
/// Example:
/// ```python
/// from tstrans.hls import HlsPublisher, MuxPublisher, HlsMode
/// from tstrans.mpegts import MuxerProgramConfigBuilder, VideoCodec, Pts90khz
///
/// pub = HlsPublisher.builder().bind("127.0.0.1:0").output_dir("/tmp/hls").build()
/// program = MuxerProgramConfigBuilder(1, 0x100).add_video(0x101, VideoCodec.H264).build()
/// mp = MuxPublisher.with_config_hls(pub, program)
/// mp.send_video(b"\x00\x00\x00\x01\x65...", pts=Pts90khz.from_raw(0), key_frame=True)
/// pub = mp.finish_into_publisher()
/// pub.finish()
/// ```
#[pyclass(name = "MuxPublisher", module = "tstrans.hls")]
pub(crate) struct PyMuxPublisher {
    /// `Option` so `finish_into_publisher()` can move the shell out while
    /// keeping the PyClass addressable for repeated no-op closes.
    /// `Mutex` because the push methods all take `&self` (the inner Rust
    /// `MuxPublisher` already holds its own `Mutex<Inner>` — the outer
    /// `Mutex<Option<...>>` only guards the take-on-finish).
    inner: Mutex<Option<RustMuxPublisher<HlsPublisher>>>,
}

impl PyMuxPublisher {
    fn with_inner<F, R>(&self, py: Python<'_>, f: F) -> PyResult<R>
    where
        F: FnOnce(
            &RustMuxPublisher<HlsPublisher>,
        ) -> Result<R, MuxPublisherError<tst_hls::HlsError>>,
    {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("MuxPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::Closed, "MuxPublisher already finished"),
            )
        })?;
        f(inner).map_err(|e| map_mux_publisher_error(py, e))
    }
}

#[pymethods]
impl PyMuxPublisher {
    /// Build a `MuxPublisher` from a single-program config + an
    /// `HlsPublisher`. **Consumes** `publisher` — the passed
    /// `HlsPublisher` handle becomes closed.
    ///
    /// Raises `HlsError(INVALID_CONFIG)` if the muxer rejects the program
    /// config, or `HlsError(FINISHED)` if `publisher` was already
    /// consumed / finished.
    #[staticmethod]
    fn with_config_hls(
        py: Python<'_>,
        publisher: &Bound<'_, PyHlsPublisher>,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<Self> {
        // 1. Take ownership of the inner HlsPublisher (consumes the handle).
        let hls = {
            let mut pub_ref = publisher.borrow_mut();
            pub_ref.take_inner().ok_or_else(|| {
                raise(
                    py,
                    &HLS,
                    BindingError::new(
                        BindingErrorKind::HlsFinished,
                        "HlsPublisher already consumed or finished",
                    ),
                )
            })?
        };

        // 2. Wrap the single MuxerProgramConfig in a MuxerConfig (mirror
        //    rtp/mux_sender.rs::PyMuxSender::new).
        let mut cfg_builder = tst_core::mpegts::mux::MuxerConfig::builder();
        cfg_builder.add_program(program_config.inner.clone());
        let muxer_cfg = cfg_builder.build().map_err(|e| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsInvalidConfig, e.to_string()),
            )
        })?;

        // 3. Hand publisher + config to the pipeline shell.
        let mp = RustMuxPublisher::with_config(hls, muxer_cfg)
            .map_err(|e| map_mux_publisher_error(py, e))?;
        Ok(Self {
            inner: Mutex::new(Some(mp)),
        })
    }

    // ── Push family ─────────────────────────────────────────────────────────

    /// Push one video access unit (Annex-B framing). When `key_frame` is
    /// true the shell auto-cuts a segment after the push.
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
        self.with_inner(py, |mp| {
            py.allow_threads(|| mp.send_video(slice, rust_pts, key_frame))
        })
    }

    /// Push one KLV blob. `stream_index` selects the KLV stream when
    /// multiple are configured (default 0 for single-stream).
    #[pyo3(signature = (klv, *, pts, stream_index = 0))]
    fn send_klv(
        &self,
        py: Python<'_>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        stream_index: u8,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, klv)?;
        let slice = coerced.as_bytes();
        self.with_inner(py, |mp| {
            py.allow_threads(|| mp.send_klv(slice, rust_pts, stream_index))
        })
    }

    /// Push one or more pre-framed audio frames (ADTS for AAC,
    /// MPEG-2 audio frames for MP2).
    #[pyo3(signature = (frames, *, pts))]
    fn send_audio(
        &self,
        py: Python<'_>,
        frames: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, frames)?;
        let slice = coerced.as_bytes();
        self.with_inner(py, |mp| py.allow_threads(|| mp.send_audio(slice, rust_pts)))
    }

    /// Push one subtitle payload.
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
        self.with_inner(py, |mp| {
            py.allow_threads(|| mp.send_subtitle(slice, rust_pts))
        })
    }

    /// Explicit segment-cut hint (IDR boundary). Cuts the current HLS
    /// segment so the next push starts a fresh decodable segment.
    fn cut_segment(&self, py: Python<'_>) -> PyResult<()> {
        self.with_inner(py, |mp| py.allow_threads(|| mp.cut_segment()))
    }

    // ── Stats ────────────────────────────────────────────────────────────────

    /// Shell-level stats (`MuxPublisherStats`).
    fn stats(&self, py: Python<'_>) -> PyResult<PyMuxPublisherStats> {
        let stats = self.with_inner(py, |mp| Ok(mp.stats()))?;
        Ok(PyMuxPublisherStats::from(stats))
    }

    /// Publisher-side universal stats (`PublisherStats`).
    fn publisher_stats(&self, py: Python<'_>) -> PyResult<PyPublisherStats> {
        let stats = self.with_inner(py, |mp| Ok(mp.publisher_stats()))?;
        Ok(PyPublisherStats::from_core(stats))
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    /// Consume the shell and return the owned `HlsPublisher`. The caller
    /// should then `finish()` it (writes the final playlist + tears down
    /// the HTTP server). Raises `HlsError(FINISHED)` if already consumed.
    fn finish_into_publisher(&self, py: Python<'_>) -> PyResult<PyHlsPublisher> {
        let mp = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("MuxPublisher mutex poisoned"))?;
            guard.take().ok_or_else(|| {
                raise(
                    py,
                    &HLS,
                    BindingError::new(BindingErrorKind::Closed, "MuxPublisher already finished"),
                )
            })?
        };
        let hls = mp.finish().map_err(|e| map_mux_publisher_error(py, e))?;
        Ok(PyHlsPublisher::from_inner(hls))
    }

    fn __repr__(&self) -> String {
        let open = self.inner.lock().map(|g| g.is_some()).unwrap_or(false);
        if open {
            "MuxPublisher(open)".to_string()
        } else {
            "MuxPublisher(finished)".to_string()
        }
    }
}
