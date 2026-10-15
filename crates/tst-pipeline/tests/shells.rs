//! Domain harness: the six pipeline shells over in-memory transports —
//! mux/demux composition, terminal flush, the MuxSender golden, tracing
//! spans and back-pressure warnings (consolidated from the former per-file
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

#[path = "shells/demux_receiver_malformed_pes_recovery.rs"]
mod demux_receiver_malformed_pes_recovery;
#[path = "shells/demux_receiver_terminal_flush.rs"]
mod demux_receiver_terminal_flush;
#[path = "shells/mux_sender_backpressure_tracing.rs"]
mod mux_sender_backpressure_tracing;
#[path = "shells/mux_sender_golden.rs"]
mod mux_sender_golden;
#[path = "shells/pipeline_raw_sender.rs"]
mod pipeline_raw_sender;
#[path = "shells/pipeline_receiver.rs"]
mod pipeline_receiver;
#[path = "shells/pipeline_sender_unit.rs"]
mod pipeline_sender_unit;
#[path = "shells/pipeline_ts_sender.rs"]
mod pipeline_ts_sender;
#[path = "shells/shell_lifetime_spans.rs"]
mod shell_lifetime_spans;
