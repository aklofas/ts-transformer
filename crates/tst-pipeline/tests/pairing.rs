//! Domain harness: video/KLV pairing over a real mux → demux round trip
//! (consolidated from the former per-file tests/*.rs — see
//! tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! test path gained a `<file>::` prefix.

// Shared mocks and helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;

#[path = "pairing/pairing_demuxer_round_trip.rs"]
mod pairing_demuxer_round_trip;
#[path = "pairing/pairing_round_trip.rs"]
mod pairing_round_trip;
