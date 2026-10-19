//! PyO3 wrapper for `tst_pipeline::ext::pairing::PairingDemuxer`.
//!
//! Exposes the byte-feeding KLV↔video pairer as `tstrans.pipeline.Pairer`:
//! feed raw TS bytes, get back a list of `PairerOutput`s (one of
//! `Paired` / `UnpairedVideo` / `UnpairedKlv` / `PassThrough`). The
//! demuxer is owned internally, so `DemuxEvent`s never round-trip
//! across the binding boundary — only the projected sample shapes
//! (`VideoSample` / `KlvSample`) and pass-through events cross.
//!
//! Config + output value classes live Python-side in
//! `tstrans/pipeline.py`; this module constructs them by name
//! (`import("tstrans.pipeline")`, `getattr(...)`), and reuses the
//! `crate::mpegts` projection helpers (`build_stream_id`, `pts_to_py`,
//! `video_codec_to_py`, `metadata_kind_to_py`, `convert_event`). The
//! `VideoSample` projection is raw-first: `vs.raw` is wrapped in a
//! `_native.RawBytes` holder (Arc clone; no copy at conversion time).
//!
//! `#![allow(...)]` mirrors `mpegts.rs` — PyO3 0.22 + Rust 2024 macro
//! expansions trip these lints. Hand-written code here has no unsafe
//! blocks.
#![expect(
    clippy::disallowed_methods,
    reason = "in-memory parse/mux calls finish in bounded time and never park"
)]
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use tst_pipeline::ext::pairing::{
    KlvSample, PairerConfig, PairerMode, PairerOutput, PairingDemuxer, PairingDemuxerConfig,
    VideoSample,
};

use tst_core::mpegts::demux::DemuxerConfig;

use crate::mpegts::{
    build_stream_id, convert_event, demux_error_to_pyerr, metadata_kind_to_py, opt_pts_to_py,
    pts_to_py, video_codec_to_py,
};

// ---------------------------------------------------------------------------
// PyPairer — the byte-feeding pairer wrapper
// ---------------------------------------------------------------------------

/// Python `Pairer` — wraps `tst_pipeline::ext::pairing::PairingDemuxer`.
///
/// Surface: `feed(bytes)`, `flush()`, `stats()`, `demuxer_stats()`,
/// `reset_stats()`. Construction takes the video + KLV PIDs and an
/// optional `PairingDemuxerConfig` (Python-side dataclass) translated
/// to Rust at construction time.
#[pyclass(name = "Pairer", module = "tstrans.pipeline")]
pub struct PyPairer {
    inner: PairingDemuxer,
}

#[pymethods]
impl PyPairer {
    /// Construct a Pairer for the given video + KLV PIDs. `config` is a
    /// Python `PairingDemuxerConfig` dataclass or `None` (defaults:
    /// nearest-PTS, 300 ms tolerance).
    #[new]
    #[pyo3(signature = (video_pid, klv_pid, config = None))]
    fn new(
        py: Python<'_>,
        video_pid: u16,
        klv_pid: u16,
        config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let inner = match config {
            None => PairingDemuxer::new(video_pid, klv_pid),
            Some(cfg) => build_pairing_demuxer(py, video_pid, klv_pid, cfg)?,
        };
        Ok(Self { inner })
    }

    /// Feed a buffer of TS bytes. Accepts any bytes-like input that
    /// Python's `bytes()` constructor accepts: `bytes`, `bytearray`,
    /// `memoryview` (over either), and NumPy `uint8` arrays.
    ///
    /// Fast path: a `bytes` argument is borrowed via PyO3's `&[u8]`
    /// extractor with no extra copy. Fallback: any other bytes-like
    /// is coerced through the Python `bytes()` builtin (a single C
    /// copy into a fresh immutable `bytes` object) and then borrowed
    /// the same way.
    ///
    /// Returns the list of `PairerOutput`s produced, in feed-time
    /// order. Raises `tstrans.exceptions.DemuxError` on
    /// non-conformance (strict mode). Raises `TypeError` if the
    /// argument is not bytes-like.
    fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        // Fast path: real `bytes` extracts to a borrowed &[u8].
        //
        // GIL-release rationale (mirrors mpegts::PyDemuxer::feed): the
        // `&[u8]` borrows from a `Py<PyBytes>` whose strong reference is
        // held by the calling Python frame for the duration of this call.
        // Python's GC cannot collect a referenced object, so the slice
        // remains valid without the GIL held. `feed` does pure-Rust
        // demux + pairing (no Python object construction inside), so it
        // is safe to wrap in `detach`.
        let outputs = if let Ok(slice) = data.extract::<&[u8]>() {
            py.detach(|| self.inner.feed(slice))
                .map_err(|e| demux_error_to_pyerr(py, &e))?
        } else {
            // Fallback: coerce via the Python `bytes()` builtin. Accepts
            // `bytearray`, `memoryview`, and any object exposing the
            // buffer protocol; raises `TypeError` if not bytes-like.
            let coerced: Bound<'_, PyBytes> = py
                .import("builtins")?
                .getattr(intern!(py, "bytes"))?
                .call1((data,))?
                .cast_into::<PyBytes>()?;
            // `coerced` is `!Ungil` (contains `Python<'_>`), but the
            // underlying `&[u8]` is — extract the slice first and let
            // `coerced` (the strong `Py<PyBytes>` reference) keep it
            // alive on the stack across the GIL drop.
            let slice: &[u8] = coerced.as_bytes();
            py.detach(|| self.inner.feed(slice))
                .map_err(|e| demux_error_to_pyerr(py, &e))?
        };
        convert_outputs(py, &outputs)
    }

    /// Drain remaining state at end-of-stream: flush the demuxer, pair
    /// any trailing events, then flush the pairer. Idempotent. Returns
    /// the list of trailing `PairerOutput`s.
    fn flush(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let outputs = py.detach(|| self.inner.flush());
        convert_outputs(py, &outputs)
    }

    /// Pairing counter snapshot as a dict (`PairerStats` fields).
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // A new Rust field must fail the build until this dict carries it.
        const _: () = assert!(
            tst_pipeline::ext::pairing::PairerStats::BINDING_SCHEMA
                .fields
                .len()
                == 4
        );
        let s = self.inner.stats();
        let d = PyDict::new(py);
        d.set_item("paired", s.paired)?;
        d.set_item("unpaired_video", s.unpaired_video)?;
        d.set_item("unpaired_klv", s.unpaired_klv)?;
        d.set_item("pass_through", s.pass_through)?;
        Ok(d.into())
    }

    /// Underlying demuxer counter snapshot as a dict. Matches the
    /// `tstrans.mpegts.Demuxer.stats()` field set.
    fn demuxer_stats(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // A new Rust field must fail the build until this dict carries it (the
        // count includes the two skipped fields).
        const _: () = assert!(
            tst_core::mpegts::demux::DemuxerStats::BINDING_SCHEMA
                .fields
                .len()
                == 8
        );
        let s = self.inner.demuxer_stats();
        let d = PyDict::new(py);
        d.set_item("program_maps_seen", s.program_maps_seen)?;
        d.set_item("pmt_versions_seen", s.pmt_versions_seen)?;
        d.set_item("discontinuities", s.discontinuities)?;
        d.set_item("nonconformant", s.nonconformant)?;
        d.set_item("programs_seen", s.programs_seen)?;
        d.set_item("subtitle_streams_seen", s.subtitle_streams_seen)?;
        Ok(d.into())
    }

    /// Reset the pairing counters to zero. Does not touch demuxer stats.
    fn reset_stats(&mut self) {
        self.inner.reset_stats();
    }
}

// ---------------------------------------------------------------------------
// Output conversion: Rust PairerOutput → Python tstrans.pipeline.* instance
// ---------------------------------------------------------------------------

/// Convert a slice of `PairerOutput` to a Python `list`.
fn convert_outputs(py: Python<'_>, outputs: &[PairerOutput]) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for out in outputs {
        list.append(convert_output(py, out)?)?;
    }
    Ok(list.into())
}

/// Convert a single `PairerOutput` to the matching Python subclass
/// instance under `tstrans.pipeline.PairerOutput.*`.
fn convert_output(py: Python<'_>, out: &PairerOutput) -> PyResult<Py<PyAny>> {
    let pipeline = py.import("tstrans.pipeline")?;
    let base = pipeline.getattr(intern!(py, "PairerOutput"))?;
    match out {
        PairerOutput::Paired { video, klv } => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("video", convert_video_sample(py, video)?)?;
            kwargs.set_item("klv", convert_klv_sample(py, klv)?)?;
            Ok(base
                .getattr(intern!(py, "Paired"))?
                .call((), Some(&kwargs))?
                .into())
        }
        PairerOutput::UnpairedVideo(v) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("video", convert_video_sample(py, v)?)?;
            Ok(base
                .getattr(intern!(py, "UnpairedVideo"))?
                .call((), Some(&kwargs))?
                .into())
        }
        PairerOutput::UnpairedKlv(k) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("klv", convert_klv_sample(py, k)?)?;
            Ok(base
                .getattr(intern!(py, "UnpairedKlv"))?
                .call((), Some(&kwargs))?
                .into())
        }
        PairerOutput::PassThrough(ev) => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("event", convert_event(py, ev)?)?;
            Ok(base
                .getattr(intern!(py, "PassThrough"))?
                .call((), Some(&kwargs))?
                .into())
        }
    }
}

/// Project a `VideoSample` to a `tstrans.pipeline.VideoSample` instance.
///
/// Raw-first: wraps `vs.raw` in a `_native.RawBytes` holder (Arc clone; no
/// payload copy at conversion time). The Python `.raw` property materializes
/// the bytes lazily on first access.
fn convert_video_sample(py: Python<'_>, vs: &VideoSample) -> PyResult<Py<PyAny>> {
    let mpegts = py.import("tstrans.mpegts")?;
    let pipeline = py.import("tstrans.pipeline")?;
    let raw_holder = crate::raw_bytes::RawBytes::from_shared(vs.raw.clone());
    let av1_carriage_py: Py<PyAny> = match vs.av1_carriage {
        None => py.None(),
        Some(mode) => crate::mux::av1_carriage_to_py(py, mode)?
            .into_any()
            .unbind(),
    };
    let kwargs = PyDict::new(py);
    kwargs.set_item("stream", build_stream_id(py, &mpegts, &vs.stream)?)?;
    kwargs.set_item("pts", pts_to_py(py, &mpegts, vs.pts)?)?;
    kwargs.set_item("dts", opt_pts_to_py(py, &mpegts, vs.dts)?)?;
    kwargs.set_item("codec", video_codec_to_py(py, &mpegts, &vs.codec)?)?;
    kwargs.set_item("raw", Py::new(py, raw_holder)?)?;
    kwargs.set_item("random_access_indicator", vs.random_access_indicator)?;
    kwargs.set_item("av1_carriage", av1_carriage_py)?;
    Ok(pipeline
        .getattr(intern!(py, "VideoSample"))?
        .call((), Some(&kwargs))?
        .into())
}

/// Project a `KlvSample` to a `tstrans.pipeline.KlvSample` instance.
fn convert_klv_sample(py: Python<'_>, ks: &KlvSample) -> PyResult<Py<PyAny>> {
    let mpegts = py.import("tstrans.mpegts")?;
    let pipeline = py.import("tstrans.pipeline")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("stream", build_stream_id(py, &mpegts, &ks.stream)?)?;
    kwargs.set_item("pts", pts_to_py(py, &mpegts, ks.pts)?)?;
    kwargs.set_item("kind", metadata_kind_to_py(py, &mpegts, &ks.kind)?)?;
    kwargs.set_item("payload", PyBytes::new(py, &ks.payload))?;
    Ok(pipeline
        .getattr(intern!(py, "KlvSample"))?
        .call((), Some(&kwargs))?
        .into())
}

// ---------------------------------------------------------------------------
// Config translation — Python dataclasses → Rust PairingDemuxerConfig
// ---------------------------------------------------------------------------

/// Build a `PairingDemuxer` from a Python `PairingDemuxerConfig`
/// dataclass instance.
///
/// `PairingDemuxerConfig` and `PairerConfig` are both marked
/// non-exhaustive, so this cannot use struct-literal syntax from
/// the external `bindings/python` crate — it constructs via
/// `Default::default()` then assigns the public fields.
fn build_pairing_demuxer(
    py: Python<'_>,
    video_pid: u16,
    klv_pid: u16,
    cfg: &Bound<'_, PyAny>,
) -> PyResult<PairingDemuxer> {
    let pairer_obj = cfg.getattr(intern!(py, "pairer"))?;
    let pairer = build_pairer_config(py, &pairer_obj)?;

    let demuxer_obj = cfg.getattr(intern!(py, "demuxer"))?;
    let demuxer = if demuxer_obj.is_none() {
        DemuxerConfig::default()
    } else {
        crate::mpegts::build_demuxer_config(py, &demuxer_obj)?
    };

    let mut c = PairingDemuxerConfig::default();
    c.pairer = pairer;
    c.demuxer = demuxer;
    Ok(PairingDemuxer::with_config(video_pid, klv_pid, c))
}

/// Translate a Python `PairerConfig` dataclass instance to a Rust
/// `PairerConfig`. Constructs via `Default::default()` + field
/// assignment (the type is non-exhaustive).
fn build_pairer_config(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<PairerConfig> {
    // mode: `PairerMode.Realtime` (singleton) or `PairerMode.Buffered(...)`.
    // The Python concrete classes are `_RealtimeMode` / `_BufferedMode`;
    // detect Buffered by the class name.
    let mode_obj = obj.getattr(intern!(py, "mode"))?;
    let mode_name: String = mode_obj.get_type().name()?.to_string();
    let mode = if mode_name == "_BufferedMode" {
        let max_lag_obj = mode_obj.getattr(intern!(py, "max_lag"))?;
        PairerMode::Buffered {
            max_lag: timedelta_to_duration(py, &max_lag_obj)?,
        }
    } else {
        PairerMode::Realtime
    };

    let tolerance_obj = obj.getattr(intern!(py, "tolerance"))?;
    let tolerance = timedelta_to_duration(py, &tolerance_obj)?;
    let max_buffered_klv: u64 = obj.getattr(intern!(py, "max_buffered_klv"))?.extract()?;
    let max_buffered_video: u64 = obj.getattr(intern!(py, "max_buffered_video"))?.extract()?;

    let mut pc = PairerConfig::default();
    pc.mode = mode;
    pc.tolerance = tolerance;
    pc.max_buffered_klv = max_buffered_klv;
    pc.max_buffered_video = max_buffered_video;
    Ok(pc)
}

/// Convert a Python `datetime.timedelta` to a Rust `Duration` via
/// `total_seconds()`. Negative durations are rejected.
fn timedelta_to_duration(_py: Python<'_>, td: &Bound<'_, PyAny>) -> PyResult<Duration> {
    let secs: f64 = td.call_method0("total_seconds")?.extract()?;
    if secs < 0.0 {
        return Err(PyValueError::new_err("duration must be non-negative"));
    }
    Ok(Duration::from_secs_f64(secs))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPairer>()?;
    Ok(())
}
