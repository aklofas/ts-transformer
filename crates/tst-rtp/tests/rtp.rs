//! Domain harness: raw RTP-over-UDP unicast + multicast loopback
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `rtp::<file>::` prefix.
// Port discovery shared with `tests/conformance.rs`.
#[path = "fixtures/ports.rs"]
mod ports;
#[path = "rtp/loopback_multicast.rs"]
mod loopback_multicast;
#[path = "rtp/loopback_unicast.rs"]
mod loopback_unicast;
