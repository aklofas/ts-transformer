//! Domain harness: conformance-corpus + demux error/recovery discrimination tests
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `conformance::<file>::` prefix.

// Shared test helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;
// serde_json (the sidecar parser) is a std-only dependency.
#[cfg(feature = "std")]
#[path = "codec_conformance/conformance.rs"]
mod conformance;
#[path = "codec_conformance/demux_error_discrimination.rs"]
mod demux_error_discrimination;
#[path = "codec_conformance/demux_malformed_pes_recovery.rs"]
mod demux_malformed_pes_recovery;
