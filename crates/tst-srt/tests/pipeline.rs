//! Domain harness: pipeline shells (MuxSender/Receiver/Managed) over SRT transport
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `pipeline::<file>::` prefix.

// Shared loopback helpers + the `require_loopback!` macro, declared once at
// the binary root so `crate::common::*` and the macro resolve for every member.
#[macro_use]
#[path = "common/mod.rs"]
mod common;
#[path = "pipeline/pipeline_managed.rs"]
mod pipeline_managed;
#[path = "pipeline/pipeline_receiver_live.rs"]
mod pipeline_receiver_live;
#[path = "pipeline/pipeline_receiver_live_corpus.rs"]
mod pipeline_receiver_live_corpus;
#[path = "pipeline/pipeline_sender.rs"]
mod pipeline_sender;
#[path = "pipeline/shells_from_url.rs"]
mod shells_from_url;
