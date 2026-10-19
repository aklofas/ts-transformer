//! `tst_core::transport::EndReason` → `tstrans.shells.EndReason`.
//!
//! The member class is pure Python (`shells.py`), not a `#[pyclass]`: the
//! type only ever flows Rust → Python as a return value, so an `IntEnum`
//! behaves the same whether a caller names a member or receives one from
//! here. A recorded end reason is data, not a failure, so nothing here
//! raises through `tstrans.exceptions`.

use pyo3::intern;
use pyo3::prelude::*;

use tst_core::transport::EndReason;

/// Convert a recorded [`EndReason`] to the matching `EndReason` member.
///
/// `EndReason` is `#[non_exhaustive]`; a future variant this binding does
/// not know yet returns `Ok(None)` rather than erroring, the "ended through
/// a path this type does not instrument" contract of `EndReasonHandle::get`.
pub(crate) fn end_reason_to_py(py: Python<'_>, r: &EndReason) -> PyResult<Option<Py<PyAny>>> {
    let name = match r {
        EndReason::CleanTeardown => "CLEAN_TEARDOWN",
        EndReason::SessionExpired => "SESSION_EXPIRED",
        EndReason::KeepaliveFailed { .. } => "KEEPALIVE_FAILED",
        EndReason::TransportFailed { .. } => "TRANSPORT_FAILED",
        EndReason::ProtocolError { .. } => "PROTOCOL_ERROR",
        EndReason::Cancelled => "CANCELLED",
        EndReason::EndOfStream => "END_OF_STREAM",
        EndReason::ReconnectExhausted => "RECONNECT_EXHAUSTED",
        _ => return Ok(None),
    };
    let shells = py.import(intern!(py, "tstrans.shells"))?;
    let enum_cls = shells.getattr(intern!(py, "EndReason"))?;
    Ok(Some(enum_cls.getattr(name)?.unbind()))
}

/// The free-text `msg` carried by `KeepaliveFailed` / `TransportFailed` /
/// `ProtocolError`; `None` for every other variant.
pub(crate) fn end_detail(r: &EndReason) -> Option<&str> {
    match r {
        EndReason::KeepaliveFailed { msg }
        | EndReason::TransportFailed { msg }
        | EndReason::ProtocolError { msg } => Some(msg.as_str()),
        _ => None,
    }
}
