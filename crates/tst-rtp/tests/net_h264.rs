//! Domain harness: RFC 6184 H.264-over-RTP integration tests.
//!
//! Covers the full `H264Receiver` + `H264Depacketizer` pipeline:
//! UDP loopback round-trips (single and FU-A fragmented AUs, randomized-loss
//! soak) and RTSP session setup via `setup_h264_auto` / `into_h264_receiver`.
//!
//! Each `mod` below corresponds to one test file under `tests/h264/`.
//! Shared helpers (`common`) are declared once at the binary root so
//! `crate::common::*` resolves for every member.

// Shared fixtures (loopback RTSP server fixture shared with other domains).
#[path = "fixtures/mod.rs"]
mod fixtures;

// Test-only RFC 6184 payloader + LCG PRNG + Annex B helper.
#[path = "h264/common.rs"]
mod common;

#[path = "h264/udp_loopback.rs"]
mod udp_loopback;

#[path = "h264/rtsp_session.rs"]
mod rtsp_session;
