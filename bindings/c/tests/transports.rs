//! Domain harness: C ABI transport open smokes: HLS, RIST, RTP, TCP, UDP
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `transports::<file>::` prefix. Per-file `#![cfg(feature=…)]`
//! gates still apply (a gated-out member compiles to an empty module).
#[path = "transports/hls_publish_smoke.rs"]
mod hls_publish_smoke;
#[path = "transports/rist_open_smoke.rs"]
mod rist_open_smoke;
#[path = "transports/rtp_cancel_first.rs"]
mod rtp_cancel_first;
#[path = "transports/rtp_open_smoke.rs"]
mod rtp_open_smoke;
#[path = "transports/tcp_open_smoke.rs"]
mod tcp_open_smoke;
#[path = "transports/udp_close_cancels_first.rs"]
mod udp_close_cancels_first;
#[path = "transports/udp_open_smoke.rs"]
mod udp_open_smoke;
