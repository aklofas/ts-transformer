//! Domain harness: the managed (auto-reconnecting) transports and the
//! managed demux receiver — reconnect accounting, cancel during backoff
//! and inside the factory, terminal flush, poison policy, reconnect
//! tracing on the receive side (consolidated from the former per-file
//! tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! test path gained a `<file>::` prefix.

// Shared mocks and helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;

#[path = "managed/managed_demux_receiver_reconnect.rs"]
mod managed_demux_receiver_reconnect;
#[path = "managed/managed_demux_receiver_terminal_flush.rs"]
mod managed_demux_receiver_terminal_flush;
#[path = "managed/managed_receive_attempts.rs"]
mod managed_receive_attempts;
#[path = "managed/managed_receive_cancel_backoff.rs"]
mod managed_receive_cancel_backoff;
#[path = "managed/managed_receive_cancel_factory.rs"]
mod managed_receive_cancel_factory;
#[path = "managed/managed_receive_tracing.rs"]
mod managed_receive_tracing;
#[path = "managed/managed_send_cancel_factory.rs"]
mod managed_send_cancel_factory;
#[path = "managed/managed_send_inner_cancelled.rs"]
mod managed_send_inner_cancelled;
#[path = "managed/poison_policy.rs"]
mod poison_policy;
