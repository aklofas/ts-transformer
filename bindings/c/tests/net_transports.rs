//! Domain harness: C ABI transport open smokes: HLS, RIST, RTP, RTSP publish, TCP, UDP
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `transports::<file>::` prefix. Per-file `#![cfg(feature=…)]`
//! gates still apply (a gated-out member compiles to an empty module).
#[allow(dead_code)]
#[path = "common/mod.rs"]
mod common;
#[path = "transports/hls_publish_smoke.rs"]
mod hls_publish_smoke;
#[path = "transports/rist_cancel_from_other_thread.rs"]
mod rist_cancel_from_other_thread;
#[path = "transports/rist_open_smoke.rs"]
mod rist_open_smoke;
#[path = "transports/rist_sender_flush.rs"]
mod rist_sender_flush;
#[path = "transports/rtp_cancel_first.rs"]
mod rtp_cancel_first;
#[path = "transports/rtp_open_smoke.rs"]
mod rtp_open_smoke;
#[path = "transports/rtp_sender_flush.rs"]
mod rtp_sender_flush;
#[path = "transports/rtsp_client_connect.rs"]
mod rtsp_client_connect;
#[path = "transports/rtsp_publish_smoke.rs"]
mod rtsp_publish_smoke;
#[path = "transports/tcp_open_smoke.rs"]
mod tcp_open_smoke;
#[path = "transports/tcps_loopback.rs"]
mod tcps_loopback;
#[path = "transports/ts_bytes_demuxed.rs"]
mod ts_bytes_demuxed;
#[path = "transports/udp_cancel_terminal.rs"]
mod udp_cancel_terminal;
#[path = "transports/udp_close_cancels_first.rs"]
mod udp_close_cancels_first;
#[path = "transports/udp_open_smoke.rs"]
mod udp_open_smoke;
#[path = "transports/udp_sender_ceiling.rs"]
mod udp_sender_ceiling;
#[path = "transports/udp_sender_flush.rs"]
mod udp_sender_flush;
