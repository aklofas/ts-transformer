//! Domain harness: SRT socket and transport statistics
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `stats::<file>::` prefix.

// Shared loopback helpers + the `require_loopback!` macro, declared once at
// the binary root so `crate::common::*` and the macro resolve for every member.
#[macro_use]
#[path = "common/mod.rs"]
mod common;
#[path = "stats/socket_stats.rs"]
mod socket_stats;
#[path = "stats/stats.rs"]
mod stats;
