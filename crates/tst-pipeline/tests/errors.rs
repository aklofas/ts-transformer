//! Domain harness: shell error kinds, their `Display` text, and how
//! transport errors are told apart at the shells (consolidated from the
//! former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! test path gained a `<file>::` prefix.

// Shared mocks and helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;

#[path = "errors/shell_error_display.rs"]
mod shell_error_display;
#[path = "errors/shell_error_kind_derivation.rs"]
mod shell_error_kind_derivation;
#[path = "errors/transport_error_discrimination.rs"]
mod transport_error_discrimination;
