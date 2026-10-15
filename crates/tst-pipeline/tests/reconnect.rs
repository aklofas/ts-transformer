//! Domain harness: the send-side reconnect machinery — Background mode,
//! Blocking-mode message ownership, cancelling a parked send, the Reject
//! overflow policy, reconnect tracing (consolidated from the former
//! per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! test path gained a `<file>::` prefix.

// Shared mocks and helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;

#[path = "reconnect/reconnect_background.rs"]
mod reconnect_background;
#[path = "reconnect/reconnect_blocking_send_ownership.rs"]
mod reconnect_blocking_send_ownership;
#[path = "reconnect/reconnect_cancel_parked_send.rs"]
mod reconnect_cancel_parked_send;
#[path = "reconnect/reconnect_reject_overflow.rs"]
mod reconnect_reject_overflow;
#[path = "reconnect/reconnect_tracing.rs"]
mod reconnect_tracing;
