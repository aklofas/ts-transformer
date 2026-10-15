//! `tst_rtp::EndReason` → Python `tstrans.rtp.StreamEndReason`
//! conversion, shared by `transport::PyReceiver`, `demux_receiver::PyDemuxReceiver`,
//! and `h264_receiver::PyH264Receiver`.
//!
//! ★CROSS-SURFACE RULING (PR-B's final review): the C ABI reads a
//! `KeepaliveFailed`/`TransportFailed`/`ProtocolError` variant's `msg`
//! through the shared thread-local last-error channel
//! (`bindings/c/core/src/rtp/end_reason.rs`). That's a C-only pattern —
//! Python has no such channel, so `end_reason_detail` below reads the
//! `msg` field DIRECTLY off the Rust enum. Do not route this through
//! `tstrans.exceptions` or any error-raising path; a recorded end reason
//! is data, not a failure.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::intern;
use pyo3::prelude::*;

use tst_core::transport::EndReason;

/// Convert a recorded [`EndReason`] to the matching
/// `tstrans.rtp.StreamEndReason` IntEnum member.
///
/// Looks up the member by name via `py.import_bound("tstrans.rtp")` —
/// the same call-into-Python-defined-class pattern
/// `mpegts.rs::stream_kind_to_py` uses for `StreamKindTag` (kept in
/// pure Python so `isinstance`/pattern-matching works the same whether
/// a caller constructs a `EndReason` member directly or receives
/// one from this conversion).
///
/// `EndReason` is non-exhaustive on the tst-rtp side; a
/// future variant this binding doesn't know how to map yet returns
/// `Ok(None)` rather than erroring — matching the "ended through a path
/// this type doesn't instrument" contract documented on
/// `EndReasonHandle::get`.
pub(crate) fn end_reason_to_py(py: Python<'_>, r: &EndReason) -> PyResult<Option<PyObject>> {
    let name = match r {
        EndReason::CleanTeardown => "CLEAN_TEARDOWN",
        EndReason::SessionExpired => "SESSION_EXPIRED",
        EndReason::KeepaliveFailed { .. } => "KEEPALIVE_FAILED",
        EndReason::TransportFailed { .. } => "TRANSPORT_FAILED",
        EndReason::ProtocolError { .. } => "PROTOCOL_ERROR",
        EndReason::Cancelled => "CANCELLED",
        _ => return Ok(None),
    };
    let rtp = py.import_bound("tstrans.rtp")?;
    let enum_cls = rtp.getattr(intern!(py, "StreamEndReason"))?;
    let member = enum_cls.getattr(name)?;
    Ok(Some(member.into()))
}

/// The free-text `msg` carried by `KeepaliveFailed` / `TransportFailed` /
/// `ProtocolError`; `None` for the three detail-less variants
/// (`CleanTeardown`, `SessionExpired`, `Cancelled`) and any future
/// non-exhaustive variant.
pub(crate) fn end_reason_detail(r: &EndReason) -> Option<&str> {
    match r {
        EndReason::KeepaliveFailed { msg }
        | EndReason::TransportFailed { msg }
        | EndReason::ProtocolError { msg } => Some(msg.as_str()),
        _ => None,
    }
}
