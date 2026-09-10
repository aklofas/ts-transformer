//! `tst_pipeline::RecvEndReason` → Python `tstrans.srt.RecvEndReason`
//! conversion, used by `managed_convenience::PyManagedDemuxReceiver`.
//!
//! ★DEDICATED ENUM, NOT A REUSE OF `tstrans.rtp.StreamEndReason`: the C
//! ABI maps `RecvEndReason` onto its RTP-shaped `TstStreamEndReason` to
//! avoid minting a second ABI type (see
//! `bindings/c/core/src/receiver/demux_receiver/managed.rs`). Python has
//! no such constraint and the two are genuinely different types in Rust,
//! so the binding mirrors the Rust enum 1:1 — SOURCE WINS.
//!
//! ★PURE-PYTHON ENUM, same as `crate::rtp::end_reason`: the member class
//! lives in `bindings/python/python/tstrans/srt.py`, not in a
//! `#[pyclass(eq, eq_int)]` here. That follows the convention documented
//! on `tstrans.rtp.StreamEndReason` — a Rust-backed enum is for types
//! that cross the Python→Rust boundary as constructor arguments
//! (`ReconnectPolicy`, `BackoffStrategy`, `OverflowPolicy`,
//! `ReconnectMode`), while a type that only ever flows Rust→Python as a
//! return value stays pure Python so `isinstance` / `IntEnum` /
//! pattern-matching behave identically whether a caller names a member
//! directly or receives one from this conversion. Keeping both
//! end-reason enums the same kind is the point of the parity arc.

use pyo3::intern;
use pyo3::prelude::*;

use tst_pipeline::RecvEndReason;

/// Convert a recorded [`RecvEndReason`] to the matching
/// `tstrans.srt.RecvEndReason` IntEnum member.
///
/// Looks up the member by name via `py.import_bound("tstrans.srt")` —
/// the same call-into-Python-defined-class pattern
/// `crate::rtp::end_reason::end_reason_to_py` uses.
///
/// `RecvEndReason` is `#[non_exhaustive]` on the tst-pipeline side; a
/// future variant this binding doesn't know how to map yet returns
/// `Ok(None)` rather than erroring — matching the "ended through a path
/// this type doesn't instrument" contract documented on
/// `RecvEndReasonHandle::get`, and the same wildcard degradation the C
/// converter (`convert_recv_end_reason`) applies.
pub(crate) fn recv_end_reason_to_py(
    py: Python<'_>,
    r: &RecvEndReason,
) -> PyResult<Option<PyObject>> {
    let name = match r {
        RecvEndReason::EndOfStream => "END_OF_STREAM",
        RecvEndReason::ReconnectExhausted => "RECONNECT_EXHAUSTED",
        RecvEndReason::Cancelled => "CANCELLED",
        _ => return Ok(None),
    };
    let srt = py.import_bound("tstrans.srt")?;
    let enum_cls = srt.getattr(intern!(py, "RecvEndReason"))?;
    Ok(Some(enum_cls.getattr(name)?.unbind()))
}
