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
pub(crate) mod receiver;
pub(crate) mod sender;

use pyo3::prelude::*;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "shells")?;
    m.add_class::<cancel::PySocketStats>()?;
    m.add_class::<cancel::PyCancelHandle>()?;
    m.add_class::<sender::PySender>()?;
    m.add_class::<receiver::PyReceiver>()?;
    m.add_class::<mux_sender::PyMuxSender>()?;
    m.add_class::<demux_receiver::PyDemuxReceiver>()?;
    parent.add_submodule(&m)?;
    Ok(())
}
