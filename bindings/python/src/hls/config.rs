//! `HlsMode` enum + `HlsStats` frozen dataclass.
//!
//! `HlsMode` mirrors `tst_hls::HlsMode` (Live / Event / Vod). It is
//! an int-comparable PyEnum (`#[pyclass(eq, eq_int)]`) so Python callers
//! can do `mode == HlsMode.LIVE` and `IntEnum`-style ordering.
//!
//! `HlsStats` mirrors `tst_hls::HlsStats` — the richer per-impl
//! snapshot (3 u64 fields: `segments_written` / `bytes_pushed_total` /
//! `open_segment_bytes`). The
//! duration-style fields live on the universal `PublisherStats`
//! (`HlsPublisher.stats()`), not on `HlsStats` (`HlsPublisher.hls_stats()`).

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// HlsMode — PyEnum mirror of tst_hls::HlsMode
// ---------------------------------------------------------------------------

/// HLS playlist mode (`tstrans.hls.HlsMode`).
///
/// - `LIVE` — rolling-window playlist; old segments evict. No
///   `#EXT-X-ENDLIST` until `finish()`.
/// - `EVENT` — playlist monotone-grows; `#EXT-X-ENDLIST` on `finish()`.
/// - `VOD` — like EVENT but the playlist is written all-at-once at
///   `finish()`.
#[pyclass(
    from_py_object,
    name = "HlsMode",
    module = "tstrans.hls",
    eq,
    eq_int,
    frozen
)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
// SHOUTY_SNAKE variant names match the tst-py IntEnum-pyclass convention
// (cf. rtp's TransportPref); this allow covers the lint that would demand
// CamelCase `Live`/`Event`/`Vod`.
#[allow(clippy::upper_case_acronyms)]
pub(crate) enum PyHlsMode {
    // SCREAMING_SNAKE variant names so Python sees `HlsMode.LIVE` etc.
    // (matches the tst-py pyclass-enum convention — cf. rtp's TransportPref
    // `AUTO`/`UDP`/`TCP`). The upstream `tst_hls::HlsMode` uses
    // `Live`/`Event`/`Vod`; the `From` impls bridge the two namings.
    LIVE,
    EVENT,
    VOD,
}

impl From<PyHlsMode> for tst_hls::HlsMode {
    fn from(m: PyHlsMode) -> Self {
        match m {
            PyHlsMode::LIVE => Self::Live,
            PyHlsMode::EVENT => Self::Event,
            PyHlsMode::VOD => Self::Vod,
        }
    }
}

impl From<tst_hls::HlsMode> for PyHlsMode {
    fn from(m: tst_hls::HlsMode) -> Self {
        match m {
            tst_hls::HlsMode::Live => Self::LIVE,
            tst_hls::HlsMode::Event => Self::EVENT,
            tst_hls::HlsMode::Vod => Self::VOD,
        }
    }
}

#[pymethods]
impl PyHlsMode {
    fn __repr__(&self) -> &'static str {
        match self {
            PyHlsMode::LIVE => "HlsMode.LIVE",
            PyHlsMode::EVENT => "HlsMode.EVENT",
            PyHlsMode::VOD => "HlsMode.VOD",
        }
    }
}
