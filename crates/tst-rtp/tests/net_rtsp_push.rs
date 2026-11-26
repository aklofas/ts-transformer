//! Domain harness: the RTSP push client (`RtspPush`) publishing into this
//! library's own `RtspServer`: MPEG-TS and elementary pushes arriving
//! whole, the server's refusals, auth, how a push ends, and sender shells
//! over a push.
//!
//! Every member needs the server, so the whole binary is gated on the
//! `rtsp-server` feature (default-on); under `--no-default-features` it
//! is empty.
#![cfg(feature = "rtsp-server")]

// Shared fixtures, declared once at the binary root so `crate::fixtures::*`
// resolves for every member.
#[path = "fixtures/mod.rs"]
mod fixtures;
#[path = "rtsp_push/common.rs"]
mod common;
#[path = "rtsp_push/auth.rs"]
mod auth;
#[path = "rtsp_push/ends.rs"]
mod ends;
#[path = "rtsp_push/es_h264.rs"]
mod es_h264;
#[path = "rtsp_push/es_h264_klv.rs"]
mod es_h264_klv;
#[path = "rtsp_push/mp2t.rs"]
mod mp2t;
#[path = "rtsp_push/publisher_change.rs"]
mod publisher_change;
#[path = "rtsp_push/refusals.rs"]
mod refusals;
#[path = "rtsp_push/shells.rs"]
mod shells;
#[cfg(feature = "rtsp-server-tls")]
#[path = "rtsp_push/tls.rs"]
mod tls;
