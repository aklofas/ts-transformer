//! Domain harness: SRT socket/listener construction, options, lifecycle, and I/O
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `builder::<file>::` prefix.

// Shared loopback helpers + the `require_loopback!` macro, declared once at
// the binary root so `crate::common::*` and the macro resolve for every member.
#[macro_use]
#[path = "common/mod.rs"]
mod common;
#[path = "builder/io.rs"]
mod io;
#[path = "builder/lifecycle.rs"]
mod lifecycle;
#[path = "builder/linger.rs"]
mod linger;
#[path = "builder/listener.rs"]
mod listener;
#[path = "builder/options.rs"]
mod options;
#[path = "builder/stream_id.rs"]
mod stream_id;
#[path = "builder/udp_buffer.rs"]
mod udp_buffer;
#[path = "builder/srt_buf_bytes.rs"]
mod srt_buf_bytes;
