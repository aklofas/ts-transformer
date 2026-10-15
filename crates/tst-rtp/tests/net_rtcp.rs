//! Domain harness: RTCP receiver/sender reports over RTP and RTSP-interleaved transports
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `rtcp::<file>::` prefix.

// Shared fixtures (loopback RTSP server, self-signed TLS certs), declared
// once at the binary root so `crate::fixtures::*` resolves for every member.
#[path = "fixtures/mod.rs"]
mod fixtures;
#[path = "rtcp/interleaved.rs"]
mod interleaved;
#[path = "rtcp/loopback.rs"]
mod loopback;
#[path = "rtcp/via_rtsp.rs"]
mod via_rtsp;
