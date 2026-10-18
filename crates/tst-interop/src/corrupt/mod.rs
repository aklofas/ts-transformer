//! Seeded sender-side TS corruption tap (spec §4.1) and the receiver-side
//! attribution engine that judges what the corruption did (spec §4.3).
//!
//! The two halves are deliberately split by a FILE, not by a shared
//! in-memory object: [`Corrupter`] wraps the send-side [`Transport`](tst_core::transport::Transport) and
//! writes one JSONL line per injection; [`Attribution`] reads that log
//! back and matches it against the error events a receiver actually
//! surfaced. Nothing is carried across in process memory, so the same
//! judgement can be made live (`recv`), offline (`verify` over a capture),
//! or days later from an archived soak directory — and a receiver can
//! never "know" about an injection except through the same evidence a
//! human would read.
//!
//! # Why a packet-coordinate instead of a byte offset
//!
//! Sender byte offsets are useless at the receiver: the transport loses,
//! duplicates and reorders, and the corruption itself changes the wire
//! length (truncation, inserted garbage). Every injection is therefore
//! logged at a [`Coord`] — the most recent PCR base seen *before* the
//! packet, plus the number of packets since that PCR packet. PCR bases are
//! carried in the stream itself, so both ends can name the same instant
//! without a shared clock, and a receiver resolves a coordinate the moment
//! it sees that base (or, if the injection destroyed the packet carrying
//! it, the first base after it — see [`Attribution::on_pcr`]). An
//! injection whose log line is read only AFTER its anchor base was seen —
//! a zero-latency link can deliver the PCR before the sender logs the
//! injection behind it — is resolved by replaying the recent PCR history
//! (see [`Attribution::append`]).
//!
//! Determinism is the other invariant: given the same seed, the same
//! config and the same input bytes, the tap emits byte-identical wire
//! output and a byte-identical log. Nothing here reads the clock.

mod attribution;
mod coord;
mod tap;
#[doc(hidden)]
pub mod testing;

pub use attribution::*;
pub use coord::*;
pub use tap::*;
