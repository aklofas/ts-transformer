//! `PublisherStats`.
//!
//! The public `Publisher` ABC lives in the Python layer
//! (`tstrans/hls.py`) as a pure `abc.ABC`: a real ABC mixes cleanly with
//! `abc.ABCMeta` (so `Publisher.register(HlsPublisher)` works for the
//! virtual-subclass contract) whereas a native PyO3 pyclass has the plain
//! `type` metaclass and no `.register()` classmethod. The ABC mirrors the
//! Rust `tst_core::publisher::Publisher` trait (`push_ts` / `cut_segment`
//! / `finish` / `stats`).
//!
//! This module owns only the native `PublisherStats` projection of
//! `tst_core::publisher::PublisherStats`. The two `Option<Duration>`
//! fields are surfaced as `Optional[int]` microseconds — integer µs
//! avoids float-precision surprises and is trivially convertible to
//! seconds on the Python side (`age_us / 1_000_000`). The pyi documents
//! this choice.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// PublisherStats — frozen mirror of tst_core::publisher::PublisherStats
// ---------------------------------------------------------------------------

/// Universal cross-publisher stats snapshot (`tstrans.hls.PublisherStats`).
///
/// Mirrors `tst_core::publisher::PublisherStats`. The two duration fields
/// are exposed as `Optional[int]` microseconds (`None` when no segment is
/// open / no segment has completed yet).
#[pyclass(name = "PublisherStats", module = "tstrans.hls", frozen, get_all)]
#[derive(Clone)]
pub(crate) struct PyPublisherStats {
    /// Total completed segments written.
    pub segments_written: u64,
    /// Total bytes pushed into the sink.
    pub bytes_written: u64,
    /// Wall-clock age of the currently-open segment, in microseconds;
    /// `None` when no segment is open.
    pub current_segment_age_us: Option<u64>,
    /// Wall-clock duration of the most recently completed segment, in
    /// microseconds; `None` before the first segment cut.
    pub last_segment_duration_us: Option<u64>,
}

impl PyPublisherStats {
    /// Project a `tst_core::publisher::PublisherStats` into the Python
    /// frozen dataclass (durations flattened to µs).
    pub(crate) fn from_core(s: tst_core::publisher::PublisherStats) -> Self {
        Self {
            segments_written: s.segments_written,
            bytes_written: s.bytes_written,
            current_segment_age_us: s.current_segment_age.map(|d| d.as_micros() as u64),
            last_segment_duration_us: s.last_segment_duration.map(|d| d.as_micros() as u64),
        }
    }
}

#[pymethods]
impl PyPublisherStats {
    /// Construct directly — useful when implementing a custom Python
    /// `Publisher` subclass whose `stats()` must return a `PublisherStats`.
    #[new]
    #[pyo3(signature = (
        segments_written,
        bytes_written,
        current_segment_age_us = None,
        last_segment_duration_us = None,
    ))]
    fn new(
        segments_written: u64,
        bytes_written: u64,
        current_segment_age_us: Option<u64>,
        last_segment_duration_us: Option<u64>,
    ) -> Self {
        Self {
            segments_written,
            bytes_written,
            current_segment_age_us,
            last_segment_duration_us,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "PublisherStats(segments_written={}, bytes_written={}, \
             current_segment_age_us={:?}, last_segment_duration_us={:?})",
            self.segments_written,
            self.bytes_written,
            self.current_segment_age_us,
            self.last_segment_duration_us,
        )
    }
}
