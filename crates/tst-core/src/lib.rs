//! TS Transformer core — pure MPEG-TS mux/demux, KLV (MISB ST 0601 and
//! the companion local sets under [`klv`]), codec parameter-set parsers,
//! and transport trait definitions.
//!
//! No transport implementations. The shells that consume the
//! [`Transport`] / [`RecvTransport`] traits live in the companion crate
//! `tst-pipeline`; concrete transport impls (SRT/UDP/RTP/RTSP/TCP/RIST)
//! live in their own crates, and `tst-hls` implements the
//! [`Publisher`](publisher::Publisher) trait instead. The only I/O here is
//! the `io_file` helpers
//! (`file` feature) and the `net` socket helpers the transport crates
//! share (`std` feature).
//!
//! Two trait-adjacent types carry SRT-flavored naming: [`SrtCancelHandle`]
//! (used only by `tst-srt`) and [`SocketStats`] (its field set mirrors
//! libsrt's statistics; every transport crate fills the fields it can).
//! The code is transport-generic (no `srt-sys` dependency from this
//! crate); the names reflect contract shape, not call sites.
//!
//! ## Quick start — round-trip a ST 0601 record
//!
//! ```
//! use tst_core::klv::st0601;
//! use tst_core::UasDatalinkLs;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Build a minimal record. `Default` populates the canonical ST 0601
//! // Universal Label and leaves every typed tag at `None`; `encode_to_vec`
//! // appends Tag 65 (Version) and Tag 1 (Checksum) automatically.
//! let mut ls = UasDatalinkLs::default();
//! ls.timestamp_us = Some(1_700_000_000_000_000);
//!
//! let bytes = st0601::encode_to_vec(&ls)?;
//! let decoded = st0601::decode(&bytes)?;
//! assert_eq!(decoded.timestamp_us, Some(1_700_000_000_000_000));
//! # Ok(())
//! # }
//! ```
//!
//! # Cargo features
//!
//! - `std` (default-on) — pulls in the standard library: the `net`
//!   helpers, the blocking thread/`Barrier` cancel path, and JSON/TOML
//!   (`serde_json`/`toml`) export. With `--no-default-features` the crate
//!   is `#![no_std]` + `alloc`: MPEG-TS mux/demux, KLV, the codec parsers
//!   (H.264/H.265/H.266/AV1 parameter sets, MPEG audio, AAC, AC-3), and the
//!   transport traits all compile for bare-metal / FreeRTOS targets. The
//!   embedding binary must supply a `#[global_allocator]`. Verified in CI
//!   against `thumbv7em-none-eabihf` (Cortex-M4F/M7F = STM32F4/F7/H7) and
//!   `riscv32imac-unknown-none-elf` (e.g. ESP32-P4 bare-metal). The no_std
//!   floor is a target with native atomic load/store/CAS — both verified
//!   targets have it, and `alloc::sync::Arc` requires it; atomics-less
//!   cores (e.g. `thumbv6m`) are not supported. On 32-bit cores the cancel
//!   handle's 64-bit atomic comes from `portable-atomic`'s `fallback`
//!   implementation (a lock shim over the target's native atomics — no
//!   embedder-supplied hook needed), and KLV IMAP float math uses
//!   `libm`. (The MPEG-TS PTS/PCR pacing math is integer-only and needs no
//!   FPU; only KLV coordinate IMAP-B mapping uses `f64`.)
//! - `file` (default-on; `file` and `std` imply each other) — enables std::fs-using helpers in
//!   `io_file`. Embedded users without a filesystem disable via
//!   `tst-core = { default-features = false }`.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(rustdoc::broken_intra_doc_links)]

#[macro_use]
extern crate alloc;

pub mod cancel;
pub mod codec;
pub mod error;
#[cfg(not(feature = "std"))]
mod float_ext;
#[cfg(feature = "file")]
pub mod io_file;
pub mod klv;
pub mod mpegts;
#[cfg(feature = "std")]
pub mod net;
pub mod publisher;
pub mod shared;
pub mod transport;
pub mod url;

#[cfg(feature = "std")]
pub use cancel::CancelSlot;
pub use cancel::SrtCancelHandle;
pub use error::{
    CotError, DemuxError, KlvDecodeError, KlvEncodeError, KlvFieldError, KlvPatchError, MuxError,
};
pub use klv::st0601::UasDatalinkLs;
pub use transport::{
    BrokenCause, RecvTransport, SocketStats, Transport, TransportCancel, TransportError,
};
pub use url::{ParsedUrl, UrlError};
