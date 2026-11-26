//! MPEG-1 / MPEG-2 / MPEG-2.5 audio frame iterator.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See [`crate::codec`] for umbrella architecture and design rationale.
//!
//! ## Spec coverage
//!
//! Parsed per ISO/IEC 11172-3 (MPEG-1 Audio) §2.4, ISO/IEC 13818-3
//! (MPEG-2 Lower Sampling Frequencies), and the de-facto MPEG-2.5
//! half-rate extension:
//! - Layer I, II, III.
//! - Version: MPEG-1, MPEG-2, MPEG-2.5.
//! - Per-frame: bitrate, sample_rate, channel_mode, channels,
//!   frame_length, samples_per_frame, has_crc.
//!
//! ## Not parsed (deferred)
//!
//! - Side information / main data beyond the 4-byte header.
//! - Psychoacoustic model fields.
//! - ID3 / APEv2 tag detection (tag bytes are surfaced as parse errors
//!   and terminate the iterator).
//!
//! ## Not checked
//!
//! - Layer II's allowed bitrate / mode pairs (ISO/IEC 11172-3 §2.4.2.3,
//!   Table 3-B.2). The parser reads the header's syntax and does not
//!   police the pair: a pair the table does not allow is parsed as
//!   written.

mod decode;
mod model;
mod tables;

pub use decode::{frames, frames_with_resync};
pub use model::{ChannelMode, Frame, FrameOwned, Frames, Layer, Version};
