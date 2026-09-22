//! Domain harness: C ABI receiver loopbacks (demux/raw/TS) and sender+receiver live pairing
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `receiving::<file>::` prefix. Each member uses a
//! distinct loopback port band so the concurrent binary never collides.
#[path = "receiving/cancel_first.rs"]
mod cancel_first;
#[path = "receiving/demux_receiver_loopback.rs"]
mod demux_receiver_loopback;
#[path = "receiving/live_pair.rs"]
mod live_pair;
#[path = "receiving/managed_listener_cancel.rs"]
mod managed_listener_cancel;
#[path = "receiving/raw_receiver_loopback.rs"]
mod raw_receiver_loopback;
#[path = "receiving/reconnect_attempts.rs"]
mod reconnect_attempts;
#[path = "receiving/ts_receiver_loopback.rs"]
mod ts_receiver_loopback;
