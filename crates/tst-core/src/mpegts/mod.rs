//! MPEG-TS muxing and demuxing — multi-program TS with video, audio, KLV
//! metadata, and subtitle/caption carriage.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! ## Quick start
//!
//! ```no_run
//! use tst_core::mpegts::common::Pts90khz;
//! use tst_core::mpegts::mux::{MuxerConfig, Muxer};
//!
//! let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
//!
//! // Push one access unit (Annex-B framed) per frame:
//! # let access_unit_bytes = vec![0u8; 0];
//! # let pts = Pts90khz::new(0);
//! mux.push_video(&access_unit_bytes, pts, /*key_frame=*/ true).unwrap();
//!
//! // Push KLV metadata at any cadence:
//! # let klv_bytes = vec![0u8; 0];
//! mux.push_klv(&klv_bytes, pts, 0x00).unwrap();
//!
//! // Drain TS packets into your transport:
//! let mut buf = [0u8; 1316]; // SRT live-mode payload size = 7 × 188
//! loop {
//!     let n = mux.pull(&mut buf);
//!     if n == 0 { break; }
//!     // socket.send(&buf[..n]).unwrap();
//! }
//! ```
//!
//! ## Capabilities
//!
//! - **Multi-program TS** — up to 16 programs per muxer; per-program PCR pin
//! - **Video** — H.264 (stream_type 0x1B), H.265 (0x24), H.266 (0x33), AV1
//!   (0x06 with AV01 registration); up to 16 streams per program
//! - **KLV metadata** — both `PrivateData` (0x06) and `SynchronousMetadata`
//!   (0x15) carriage; sync streams auto-wrap inside [`mux::Muxer::push_klv`]
//!   with a 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 §2.12.4.2
//!   (see [`crate::mpegts::au_cell`]); multi-stream supported; typed decoders
//!   for ST 0601, ST 0102, ST 0605, ST 0903 live in [`crate::klv`]
//! - **Audio** — MP2 (stream_type 0x03; demuxer also accepts 0x04), AAC ADTS (0x0F), AAC LATM
//!   (0x11), AC-3 (0x81 with registration); up to 16 streams per program
//! - **Subtitles / captions** — DVB subtitling, teletext, CEA-708, and
//!   WebVTT-in-TS, all carried via `stream_type 0x06` with descriptor
//!   disambiguation; up to 16 streams per program
//! - **Output** — VBR, no null padding; Annex-B input for H.264/H.265/H.266,
//!   OBU input for AV1; one access unit per [`mux::Muxer::push_video`] call
//!
//! Defaults are conservative: [`mux::MuxerConfig::default`] produces a
//! single-program H.264 + KLV stream. See `docs/reference/compatibility.md` for the
//! full feature matrix.

pub mod au_cell;
pub mod common;
pub mod demux;
pub mod descriptors;
pub mod mux;
pub mod stats;
#[doc(hidden)]
pub mod sync;

pub use stats::StreamStats;
