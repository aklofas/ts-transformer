//! `tstrans.rtp` PyO3 bindings. Gated on `feature = "rtp"`.
//!
//! Module structure mirrors the Rust `tst_rtp` crate:
//! - `transport`       — `Sender` + `Receiver` + `SocketStats` + `CancelHandle`
//! - `mux_sender`      — `MuxSender` convenience wrapper (Muxer + Sender)
//! - `demux_receiver`  — `DemuxReceiver` convenience wrapper (Demuxer + Receiver)
//! - `client`          — `RtspClient` + `RtspSession` + auth dataclasses
//! - `server`          — `RtspServer` + `MountHandle` (16 push methods)
//! - `publish`         — `PublishMount` + `PublishMountStats` +
//!                        `PublisherInfo` (the RTSP publisher role)
//! - `h264_receiver`   — RFC 6184 H.264 receiver + depacketizer config/stats
//! - `end_reason`      — `tst_rtp::StreamEndReason` → Python conversion
//!                        helpers, shared by `transport` / `demux_receiver`
//!                        / `h264_receiver` (no PyClasses of its own, so
//!                        it isn't `register()`-ed below)
//!
//! All submodules are fully implemented.

use pyo3::prelude::*;

pub(crate) mod client;
pub(crate) mod demux_receiver;
pub(crate) mod end_reason;
pub(crate) mod h264_receiver;
pub(crate) mod mux_sender;
pub(crate) mod publish;
pub(crate) mod server;
pub(crate) mod transport;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "rtp")?;
    transport::register(&m)?;
    mux_sender::register(&m)?;
    demux_receiver::register(&m)?;
    client::register(&m)?;
    server::register(&m)?;
    publish::register(&m)?;
    h264_receiver::register(&m)?;
    parent.add_submodule(&m)?;
    Ok(())
}
