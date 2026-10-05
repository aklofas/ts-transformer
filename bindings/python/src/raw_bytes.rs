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

// PyO3 0.22 + Rust 2024 edition: the #[pymethods] macro generates calls to
// internal unsafe functions inside unsafe fn bodies. The `unsafe_op_in_unsafe_fn`
// lint (now a warning in edition 2024) fires on macro-generated code; suppress
// here exactly as in the sibling modules (codec.rs, mpegts.rs, mux.rs).
#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use pyo3::basic::CompareOp;
use pyo3::prelude::*;
use pyo3::sync::GILOnceCell;
use pyo3::types::PyBytes;

use tst_core::shared::SharedBytes;

/// Lazy, content-comparable byte holder backing `DemuxEvent.Video.raw` and
/// `DemuxEvent.Audio.raw`. The Python `bytes` value (via `.value`) is
/// materialized once on first access and cached.
#[pyclass(name = "RawBytes", module = "tstrans._native", frozen)]
pub struct RawBytes {
    shared: SharedBytes,
    cache: GILOnceCell<Py<PyBytes>>,
}

impl RawBytes {
    /// Rust path: wrap an existing `SharedBytes` with no copy. The caller does
    /// the (cheap) `Arc` clone at the call site.
    pub(crate) fn from_shared(shared: SharedBytes) -> Self {
        Self {
            shared,
            cache: GILOnceCell::new(),
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
            cache: GILOnceCell::new(),
        }
    }

    /// The payload as a Python `bytes`. Materialized once on first access and
    /// cached; subsequent reads return the identical object.
    #[getter]
    fn value(&self, py: Python<'_>) -> Py<PyBytes> {
        self.cache
            .get_or_init(py, || PyBytes::new_bound(py, &self.shared).unbind())
            .clone_ref(py)
    }

    fn __len__(&self) -> usize {
        self.shared.len()
    }

    /// Content equality vs another `RawBytes` or a Python `bytes`. Only `==` /
    /// `!=` are defined; ordering is `NotImplemented`. (A `bytearray` argument
    /// yields `NotImplemented` here; callers compare the materialized `.raw`
    /// `bytes`, where `bytes == bytearray` works via Python's own protocol.)
    fn __richcmp__(&self, py: Python<'_>, other: &Bound<'_, PyAny>, op: CompareOp) -> PyObject {
        let eq = if let Ok(other_holder) = other.downcast::<RawBytes>() {
            self.shared.as_slice() == other_holder.get().shared.as_slice()
        } else if let Ok(other_bytes) = other.extract::<&[u8]>() {
            self.shared.as_slice() == other_bytes
        } else {
            return py.NotImplemented();
        };
        match op {
            CompareOp::Eq => eq.into_py(py),
            CompareOp::Ne => (!eq).into_py(py),
            _ => py.NotImplemented(),
        }
    }

    /// Content hash derived from the raw bytes — a stable Rust hash, NOT equal
    /// to Python's `hash(bytes)`. Equal-content `RawBytes` hash equal, so the
    /// events embedding them satisfy the hash/eq invariant. PyO3 maps the
    /// returned `u64` to `Py_hash_t` with a wrapping `as isize` cast (the value
    /// may be negative; it never raises `OverflowError`).
    fn __hash__(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.shared.as_slice().hash(&mut hasher);
        hasher.finish()
    }

    /// Test-only: whether the Python `bytes` has been materialized yet. Used by
    /// the lazy-materialization regression tests; not part of the public
    /// surface.
    #[getter]
    fn _materialized(&self, py: Python<'_>) -> bool {
        self.cache.get(py).is_some()
    }
}
