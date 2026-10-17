//! `tstrans.rtp` PyO3 bindings. Gated on `feature = "rtp"`.
//!
//! Module structure mirrors the Rust `tst_rtp` crate:
//! - `transport`       — `Transport` + `RecvTransport` (raw RTP; the shells live in `tstrans.shells`)
//! - `client`          — `RtspClient` + `RtspSession` + auth dataclasses
//! - `server`          — `RtspServer` + `MountHandle` (16 push methods)
//! - `publish`         — `PublishMount` + `PublishMountStats` +
//!                        `PublisherInfo` (the RTSP publisher role)
//! - `h264_receiver`   — RFC 6184 H.264 receiver + depacketizer config/stats
//!
//! All submodules are fully implemented.

use pyo3::prelude::*;

pub(crate) mod client;
pub(crate) mod h264_receiver;
pub(crate) mod publish;
pub(crate) mod server;
pub(crate) mod transport;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "rtp")?;
    transport::register(&m)?;
    client::register(&m)?;
    server::register(&m)?;
    publish::register(&m)?;
    h264_receiver::register(&m)?;
    crate::generated::stats_rtp::register(&m)?;
    parent.add_submodule(&m)?;
    Ok(())
}
