//! Domain harness: RTSP server: mounts, auth, multicast, TLS, shutdown, interleaved/UDP transports
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `rtsp_server::<file>::` prefix.
//!
//! Every member here tests `tst_rtp::RtspServer` directly, so the whole
//! binary is gated on the `rtsp-server` feature (default-on) — under
//! `--no-default-features` this binary is empty (0 tests), which is the
//! client-only build's expected shape.
#![cfg(feature = "rtsp-server")]

// Shared fixtures (loopback RTSP server, self-signed TLS certs), declared
// once at the binary root so `crate::fixtures::*` resolves for every member.
#[path = "fixtures/mod.rs"]
mod fixtures;
#[path = "rtsp_server/auth_basic.rs"]
mod auth_basic;
#[path = "rtsp_server/auth_digest.rs"]
mod auth_digest;
#[path = "rtsp_server/bind.rs"]
mod bind;
#[path = "rtsp_server/concurrent.rs"]
mod concurrent;
#[path = "rtsp_server/lagging_peer.rs"]
mod lagging_peer;
#[path = "rtsp_server/loopback_interleaved.rs"]
mod loopback_interleaved;
#[path = "rtsp_server/loopback_udp.rs"]
mod loopback_udp;
#[path = "rtsp_server/mount.rs"]
mod mount;
#[path = "rtsp_server/multicast.rs"]
mod multicast;
#[path = "rtsp_server/notice_5402.rs"]
mod notice_5402;
#[path = "rtsp_server/session_keepalive.rs"]
mod session_keepalive;
#[path = "rtsp_server/shutdown.rs"]
mod shutdown;
#[path = "rtsp_server/tls.rs"]
mod tls;
#[path = "rtsp_server/tls_handshake_timeout.rs"]
mod tls_handshake_timeout;
#[path = "rtsp_server/oom_guard.rs"]
mod oom_guard;
#[path = "rtsp_server/orphan_fanout.rs"]
mod orphan_fanout;
#[path = "rtsp_server/idle_timeout.rs"]
mod idle_timeout;
#[path = "rtsp_server/interleaved_frame_boundary.rs"]
mod interleaved_frame_boundary;
#[path = "rtsp_server/unsupported_method.rs"]
mod unsupported_method;
#[path = "rtsp_server/publish/conformance.rs"]
mod publish_conformance;
#[path = "rtsp_server/publish/es_h264.rs"]
mod publish_es_h264;
#[path = "rtsp_server/publish/es_h264_klv.rs"]
mod publish_es_h264_klv;
#[path = "rtsp_server/publish/malformed_sdp.rs"]
mod publish_malformed_sdp;
#[path = "rtsp_server/publish/lifecycle.rs"]
mod publish_lifecycle;
#[path = "rtsp_server/publish/mp2t_interleaved.rs"]
mod publish_mp2t_interleaved;
#[path = "rtsp_server/publish/mp2t_udp.rs"]
mod publish_mp2t_udp;
#[path = "rtsp_server/publish/on_demand.rs"]
mod publish_on_demand;
#[path = "rtsp_server/publish/republish.rs"]
mod publish_republish;
#[path = "rtsp_server/publish/rtsps_record.rs"]
mod publish_rtsps_record;
#[path = "rtsp_server/publish/second_publisher.rs"]
mod publish_second_publisher;
#[path = "rtsp_server/publish/stats.rs"]
mod publish_stats;
