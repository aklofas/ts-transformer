//! `tstrans.shells` — one Python class per pipeline shell (`Sender`,
//! `Receiver`, `MuxSender`, `DemuxReceiver`) over every transport. A shell
//! is opened from a URL (`Class.open(url, …)`); the scheme picks the
//! transport crate's opener (`srt://`, `rtp://`, `udp://`, `tcp://`,
//! `tcps://`, `rist://`), and the shell holds it type-erased
//! (`Box<dyn Transport>`). The transport's error domain travels in the
//! shell's snapshot, so a `CLOSED` on an `rtp://` shell raises `RtpError`
//! and on a `udp://` shell `UdpError`.

pub(crate) mod cancel;
pub(crate) mod demux_receiver;
pub(crate) mod end_reason;
pub(crate) mod mux_sender;
pub(crate) mod open;
pub(crate) mod open_cancel;
pub(crate) mod receiver;
pub(crate) mod sender;

use pyo3::prelude::*;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "shells")?;
    crate::generated::stats_shells::register(&m)?;
    m.add_class::<cancel::PyCancelHandle>()?;
    m.add_class::<open_cancel::PyOpenCancel>()?;
    m.add_class::<sender::PySender>()?;
    m.add_class::<receiver::PyReceiver>()?;
    m.add_class::<mux_sender::PyMuxSender>()?;
    m.add_class::<demux_receiver::PyDemuxReceiver>()?;
    #[cfg(feature = "py-test-hooks")]
    m.add_function(wrap_pyfunction!(cancel::slow_cancel_handle_for_test, &m)?)?;
    #[cfg(feature = "py-test-hooks")]
    m.add_function(wrap_pyfunction!(cancel::slow_drop_for_test, &m)?)?;
    #[cfg(feature = "py-test-hooks")]
    m.add_function(wrap_pyfunction!(
        open_cancel::open_cancel_with_slow_open_for_test,
        &m
    )?)?;
    #[cfg(feature = "py-test-hooks")]
    m.add_function(wrap_pyfunction!(slot_held_for_test, &m)?)?;
    parent.add_submodule(&m)?;
    Ok(())
}

/// `_slot_held_for_test(obj)`: whether a call on another thread holds the
/// slot of `obj` (a shell, a udp or rist receive transport, a tcp
/// transport), so a test reads a getter only once that call is parked.
/// Never waits.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_slot_held_for_test")]
fn slot_held_for_test(obj: &Bound<'_, PyAny>) -> PyResult<bool> {
    if let Ok(s) = obj.cast::<sender::PySender>() {
        return Ok(s.borrow().slot_held());
    }
    if let Ok(s) = obj.cast::<receiver::PyReceiver>() {
        return Ok(s.borrow().slot_held());
    }
    if let Ok(s) = obj.cast::<mux_sender::PyMuxSender>() {
        return Ok(s.borrow().slot_held());
    }
    if let Ok(s) = obj.cast::<demux_receiver::PyDemuxReceiver>() {
        return Ok(s.borrow().slot_held());
    }
    #[cfg(feature = "udp")]
    if let Ok(s) = obj.cast::<crate::udp::PyUdpRecvTransport>() {
        return Ok(s.borrow().slot_held());
    }
    #[cfg(feature = "rist")]
    if let Ok(s) = obj.cast::<crate::rist::PyRistRecvTransport>() {
        return Ok(s.borrow().slot_held());
    }
    #[cfg(feature = "tcp")]
    if let Ok(s) = obj.cast::<crate::tcp::PyTcpTransport>() {
        return Ok(s.borrow().slot_held());
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "no slot probe for {}",
        obj.get_type().name()?
    )))
}
