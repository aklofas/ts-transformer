//! `RawBytes` — a lazy, content-comparable holder for a demuxed payload.
//!
//! The demuxer does not copy each video/audio/subtitle/unknown payload into
//! a Python `bytes` at demux time. Instead it hands the underlying
//! [`SharedBytes`] (a cheap `Arc` bump — no payload copy) to a `RawBytes`
//! holder. The Python `bytes` is materialized on first `.raw` / `.payload`
//! access and cached thereafter, so a caller that filters the stream or reads
//! only PMT/KLV never pays for the media-payload copy.
//!
//! This holder backs:
//! - `DemuxEvent.Video.raw` and `DemuxEvent.Audio.raw`
//! - `DemuxEvent.Subtitle.payload` and `DemuxEvent.UnknownSample.payload`
//!
//! Retention tradeoff: holding the event keeps the underlying demux buffer
//! (the `Arc`) alive until the event is dropped, even if `.raw` / `.payload`
//! is never materialized. The win is pay-per-access (lazy) materialization,
//! not zero-copy at the boundary — abi3 copies the bytes into a fresh
//! `PyBytes` regardless.
//!
//! This holder is shared by `src/pipeline.rs` and the demux event types,
//! hence the standalone module.

// PyO3 under the Rust 2024 edition: the #[pymethods] macro generates calls to
// internal unsafe functions inside unsafe fn bodies. The `unsafe_op_in_unsafe_fn`
// lint (now a warning in edition 2024) fires on macro-generated code; suppress
// here exactly as in the sibling modules (codec.rs, mpegts.rs, mux.rs).
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::basic::CompareOp;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBool, PyBytes};

use tst_core::shared::SharedBytes;

/// Lazy, content-comparable byte holder backing `DemuxEvent.Video.raw` and
/// `DemuxEvent.Audio.raw`. The Python `bytes` value (via `.value`) is
/// materialized once on first access and cached.
#[pyclass(name = "RawBytes", module = "tstrans._native", frozen)]
pub struct RawBytes {
    shared: SharedBytes,
    cache: PyOnceLock<Py<PyBytes>>,
}

impl RawBytes {
    /// Rust path: wrap an existing `SharedBytes` with no copy. The caller does
    /// the (cheap) `Arc` clone at the call site.
    pub(crate) fn from_shared(shared: SharedBytes) -> Self {
        Self {
            shared,
            cache: PyOnceLock::new(),
        }
    }
}

#[pymethods]
impl RawBytes {
    /// Python / direct-construction path: copy the supplied bytes into a fresh
    /// shared allocation. Used when a caller builds a `DemuxEvent.Video` /
    /// `.Audio` by hand with `raw=b"..."`.
    #[new]
    fn new(data: &[u8]) -> Self {
        Self {
            shared: SharedBytes::from_vec(data.to_vec()),
            cache: PyOnceLock::new(),
        }
    }

    /// The payload as a Python `bytes`. Materialized once on first access and
    /// cached; subsequent reads return the identical object.
    #[getter]
    fn value(&self, py: Python<'_>) -> Py<PyBytes> {
        self.cache
            .get_or_init(py, || PyBytes::new(py, &self.shared).unbind())
            .clone_ref(py)
    }

    fn __len__(&self) -> usize {
        self.shared.len()
    }

    /// Content equality vs another `RawBytes` or a Python `bytes`. Only `==` /
    /// `!=` are defined; ordering is `NotImplemented`. (A `bytearray` argument
    /// yields `NotImplemented` here; callers compare the materialized `.raw`
    /// `bytes`, where `bytes == bytearray` works via Python's own protocol.)
    fn __richcmp__(&self, py: Python<'_>, other: &Bound<'_, PyAny>, op: CompareOp) -> Py<PyAny> {
        let eq = if let Ok(other_holder) = other.cast::<RawBytes>() {
            self.shared.as_slice() == other_holder.get().shared.as_slice()
        } else if let Ok(other_bytes) = other.extract::<&[u8]>() {
            self.shared.as_slice() == other_bytes
        } else {
            return py.NotImplemented();
        };
        match op {
            CompareOp::Eq => PyBool::new(py, eq).to_owned().into_any().unbind(),
            CompareOp::Ne => PyBool::new(py, !eq).to_owned().into_any().unbind(),
            _ => py.NotImplemented(),
        }
    }

    /// Hash of the materialised `bytes`, so a `RawBytes` and the equal `bytes`
    /// are interchangeable as dict/set keys; hashing materialises the cached
    /// value.
    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.value(py).bind(py).hash()
    }

    /// Test-only: whether the Python `bytes` has been materialized yet. Used by
    /// the lazy-materialization regression tests; not part of the public
    /// surface.
    #[getter]
    fn _materialized(&self, py: Python<'_>) -> bool {
        self.cache.get(py).is_some()
    }
}
