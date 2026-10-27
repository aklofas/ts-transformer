//! Domain harness: file I/O and timing smoke tests
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `io::<file>::` prefix.
#[path = "io/io_file_smoke.rs"]
mod io_file_smoke;
#[cfg(feature = "std")]
#[path = "io/timing_smoke.rs"]
mod timing_smoke;
