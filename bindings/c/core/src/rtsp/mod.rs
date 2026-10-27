//! `tst_rtsp_*` C ABI entry points. Gated on `feature = "rtp"` because
//! RTSP requires the underlying RTP data plane.
//!
//! Provides RTSP client (camera ingest) and server (publishing) C ABI.

pub(crate) mod client;
pub(crate) mod server;
