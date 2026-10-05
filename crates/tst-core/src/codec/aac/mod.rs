//! AAC frame iterator (ADTS framing; LATM frames are not iterated).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See [`crate::codec`] for umbrella architecture and design rationale.
//!
//! ## Spec coverage
//!
//! Parsed per ISO/IEC 13818-7 §1.A (ADTS) over MPEG-2 / MPEG-4 AAC:
//! - ADTS sync word, MPEG version, layer validation.
//! - Per-frame: profile, sample_rate, channel_configuration, channel_layout,
//!   frame_length, samples_per_frame, num_raw_data_blocks, has_crc.
//!
//! A LATM/LOAS sync validator at [`latm`] covers the AAC-LATM PES path
//! (stream_type 0x11). The validator is consumed by the demuxer's PES
//! emission layer and surfaces non-conformant framing to
//! [`crate::mpegts::demux::NonConformantIssue::LatmFraming`].
//!
//! ## Not parsed (deferred)
//!
//! - LATM/LOAS full audioMuxElement decode (only the sync word is
//!   validated today; AudioSpecificConfig walks are deferred).
//! - AudioSpecificConfig / SBR / PS extension headers inside raw data blocks.
//! - MPEG-4 audio object types beyond the 4 legacy ADTS profiles.
//! - Program Config Element (PCE) parsing for `channel_configuration == 0`.
//!   The iterator surfaces [`AacChannelLayout::PceDefined`] so callers
//!   know the channel count is not derivable from the ADTS header.

mod adts;
pub mod latm;
mod tables;
#[cfg(test)]
mod tests;

use crate::codec::CodecParseError;
use alloc::vec::Vec;

/// AAC profile per ADTS §1.A (legacy MPEG-2 AAC profile names; most
/// real-world ADTS encodes AAC-LC regardless of which MPEG-4 audio
/// object type the encoder used).
///
/// Per ISO/IEC 13818-7 §1.A Table 8, the `profile` field's interpretation
/// depends on the ADTS `ID` bit (MPEG version):
/// - `ID == 0` (MPEG-4): profile is an MPEG-4 audio object type minus
///   one; values `0..=3` map to Main / Lc / Ssr / LongTermPrediction.
/// - `ID == 1` (MPEG-2): profile is the MPEG-2 audio Profile; values
///   `0..=2` map to Main / Lc / Ssr and value `3` is reserved. The
///   parser surfaces reserved profile=3 in MPEG-2 streams as
///   [`crate::codec::CodecParseError::ReservedValue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AacProfile {
    Main,
    Lc,
    Ssr,
    LongTermPrediction,
}

/// ADTS MPEG version bit (one bit; 0 = MPEG-4, 1 = MPEG-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersion {
    Mpeg2,
    Mpeg4,
}

/// AAC channel layout decoded from `channel_configuration` (3 bits)
/// per ISO/IEC 14496-3 Table 1.19.
///
/// `channel_configuration == 0` is a valid streaming shape: the channel
/// layout is carried in a Program Config Element (PCE) inside the
/// raw_data_block. The demuxer surfaces [`Self::PceDefined`] so callers
/// know the channel count cannot be derived from the ADTS header alone.
/// Walking the PCE to recover the exact count is deferred.
///
/// `channel_configuration` values `1..=7` map to canonical channel
/// counts; value `7` carries 8 channels (7.1).
///
/// `#[non_exhaustive]` — future variants (e.g. a `Pce` variant carrying
/// the walked PCE details) can be added without breaking matchers.
///
/// Prior to this enum `decode_channels(0)` returned
/// `CodecParseError::ReservedValue`, which terminated the ADTS iterator
/// and dropped every subsequent frame on streams using PCE-defined
/// channel layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AacChannelLayout {
    /// `channel_configuration == 0` — channel layout is defined by a
    /// Program Config Element (PCE) inside the raw_data_block. The ADTS
    /// header alone is insufficient to determine the channel count.
    PceDefined,
    /// Canonical channel count from `channel_configuration` `1..=7`.
    /// Note: index `7` decodes to 8 channels (7.1) per Table 1.19.
    Channels(u8),
}

impl AacChannelLayout {
    /// Convenience accessor: returns the canonical channel count when
    /// known, or `None` when the layout is PCE-defined (and therefore
    /// not derivable from the ADTS header alone).
    #[must_use]
    pub fn channels(&self) -> Option<u8> {
        match self {
            Self::Channels(n) => Some(*n),
            Self::PceDefined => None,
            // `#[non_exhaustive]` — future variants may map to channel
            // counts (e.g. Pce { channels }); update this match alongside.
        }
    }
}

/// Decoded ADTS frame. Borrows from the source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdtsFrame<'a> {
    pub profile: AacProfile,
    pub sample_rate_hz: u32,
    /// Raw `channel_configuration` field (3 bits) from the ADTS header.
    /// `0` indicates the channel layout is PCE-defined (see
    /// [`channel_layout`](Self::channel_layout)); `1..=7` are canonical
    /// channel-count indices per ISO/IEC 14496-3 Table 1.19.
    pub channel_configuration: u8,
    /// Typed channel layout. [`AacChannelLayout::PceDefined`] when
    /// `channel_configuration == 0`; [`AacChannelLayout::Channels`]`(n)`
    /// otherwise.
    pub channel_layout: AacChannelLayout,
    pub frame_length_bytes: u32,
    pub samples_per_frame: u16,
    pub num_raw_data_blocks: u8,
    pub has_crc: bool,
    pub mpeg_version: MpegVersion,
    pub raw_header: Vec<u8>,
    pub(super) body: &'a [u8],
}

impl<'a> AdtsFrame<'a> {
    /// Full-frame slice (header + body). `raw_header` is the first 7
    /// (no CRC) or 9 (with CRC) bytes of this slice copied for
    /// ownership convenience.
    pub fn bytes(&self) -> &'a [u8] {
        self.body
    }

    /// Convenience accessor: canonical channel count when derivable
    /// from the ADTS header, or `None` when the layout is PCE-defined.
    /// Equivalent to `self.channel_layout.channels()`.
    #[must_use]
    pub fn channels(&self) -> Option<u8> {
        self.channel_layout.channels()
    }

    /// Promote this borrowed frame to an [`AdtsFrameOwned`] by copying `body`.
    pub fn to_owned(&self) -> AdtsFrameOwned {
        AdtsFrameOwned {
            profile: self.profile,
            sample_rate_hz: self.sample_rate_hz,
            channel_configuration: self.channel_configuration,
            channel_layout: self.channel_layout,
            frame_length_bytes: self.frame_length_bytes,
            samples_per_frame: self.samples_per_frame,
            num_raw_data_blocks: self.num_raw_data_blocks,
            has_crc: self.has_crc,
            mpeg_version: self.mpeg_version,
            raw_header: self.raw_header.clone(),
            body: self.body.to_vec(),
        }
    }
}

/// Owned variant of [`AdtsFrame`].
///
/// `body: Vec<u8>` instead of `&'a [u8]` — usable across FFI / thread / async
/// boundaries where the borrowed source slice doesn't outlive the consumer.
///
/// Round-trip with [`AdtsFrame::to_owned`] / [`Self::as_ref`].
///
/// # Example — collect owned frames from a borrowed iterator
/// ```
/// use tst_core::codec::aac::{frames, AdtsFrameOwned};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let payload: &[u8] = &[/* ADTS-framed AAC bytes */];
/// # let payload: &[u8] = &[];
/// let owned: Vec<AdtsFrameOwned> = frames(payload)
///     .filter_map(Result::ok)
///     .map(|f| f.to_owned())
///     .collect();
/// // `owned` outlives `payload` — safe to ship over FFI.
/// let _ = owned;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdtsFrameOwned {
    pub profile: AacProfile,
    pub sample_rate_hz: u32,
    /// See [`AdtsFrame::channel_configuration`].
    pub channel_configuration: u8,
    /// See [`AdtsFrame::channel_layout`].
    pub channel_layout: AacChannelLayout,
    pub frame_length_bytes: u32,
    pub samples_per_frame: u16,
    pub num_raw_data_blocks: u8,
    pub has_crc: bool,
    pub mpeg_version: MpegVersion,
    pub raw_header: Vec<u8>,
    pub body: Vec<u8>,
}

impl AdtsFrameOwned {
    /// Borrow this owned frame as an [`AdtsFrame`] — zero-copy.
    pub fn as_ref(&self) -> AdtsFrame<'_> {
        AdtsFrame {
            profile: self.profile,
            sample_rate_hz: self.sample_rate_hz,
            channel_configuration: self.channel_configuration,
            channel_layout: self.channel_layout,
            frame_length_bytes: self.frame_length_bytes,
            samples_per_frame: self.samples_per_frame,
            num_raw_data_blocks: self.num_raw_data_blocks,
            has_crc: self.has_crc,
            mpeg_version: self.mpeg_version,
            raw_header: self.raw_header.clone(),
            body: &self.body,
        }
    }

    /// Convenience accessor: canonical channel count when derivable
    /// from the ADTS header, or `None` when the layout is PCE-defined.
    /// Equivalent to `self.channel_layout.channels()`.
    #[must_use]
    pub fn channels(&self) -> Option<u8> {
        self.channel_layout.channels()
    }
}

/// Iterator over ADTS frames in `bytes`.
///
/// Construct with [`frames`] for the strict (fail-fast) variant or
/// [`frames_with_resync`] for the best-effort variant that scans
/// forward for the next plausible ADTS syncword after a parse error.
#[must_use]
pub struct AdtsFrames<'a> {
    pub(super) buf: &'a [u8],
    pub(super) cursor: usize,
    pub(super) done: bool,
    /// When `true`, parse errors do NOT terminate the iterator.
    /// Instead, `next()` scans forward from `cursor + 1` for the next
    /// plausible 12-bit ADTS syncword (`0xFFF`) and repositions there.
    /// The current error is still yielded; subsequent `next()` calls
    /// resume from the new cursor.
    pub(super) resync: bool,
}

/// Scan `buf[start..]` for the next plausible 12-bit ADTS syncword
/// (`buf[i] == 0xFF` and `(buf[i+1] & 0xF0) == 0xF0`). Returns the
/// absolute position of the first candidate, or `None` if no candidate
/// exists before `buf.len() - 1`.
fn find_next_adts_sync(buf: &[u8], start: usize) -> Option<usize> {
    crate::codec::framing::scan_for_sync(buf, start, |b0, b1| b0 == 0xFF && (b1 & 0xF0) == 0xF0)
}

impl<'a> Iterator for AdtsFrames<'a> {
    type Item = Result<AdtsFrame<'a>, CodecParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.cursor >= self.buf.len() {
            self.done = true;
            return None;
        }
        let remaining = &self.buf[self.cursor..];
        let header = match adts::parse_header(remaining) {
            Ok(h) => h,
            Err(e) => {
                if self.resync {
                    // Advance cursor to next plausible syncword
                    // (or to end-of-buffer to terminate on subsequent
                    // call). The error is still yielded so the caller
                    // can count corruption.
                    match find_next_adts_sync(self.buf, self.cursor + 1) {
                        Some(next) => self.cursor = next,
                        None => self.done = true,
                    }
                } else {
                    self.done = true;
                }
                return Some(Err(e));
            }
        };
        let len = header.frame_length_bytes as usize;
        if remaining.len() < len {
            if self.resync {
                match find_next_adts_sync(self.buf, self.cursor + 1) {
                    Some(next) => self.cursor = next,
                    None => self.done = true,
                }
            } else {
                self.done = true;
            }
            return Some(Err(CodecParseError::Truncated {
                needed: header.frame_length_bytes,
                had: remaining.len() as u32,
            }));
        }
        let body = &remaining[..len];
        let raw_header = body[..header.raw_header_len].to_vec();
        let frame = AdtsFrame {
            profile: header.profile,
            sample_rate_hz: header.sample_rate_hz,
            channel_configuration: header.channel_configuration,
            channel_layout: header.channel_layout,
            frame_length_bytes: header.frame_length_bytes,
            samples_per_frame: header.samples_per_frame,
            num_raw_data_blocks: header.num_raw_data_blocks,
            has_crc: header.has_crc,
            mpeg_version: header.mpeg_version,
            raw_header,
            body,
        };
        self.cursor += len;
        Some(Ok(frame))
    }
}

/// Construct a strict (fail-fast) ADTS frame iterator over an AAC
/// elementary stream (PES payload bytes).
///
/// The first parse error terminates the iterator. Use
/// [`frames_with_resync`] when populating stats from possibly-corrupted
/// streams, where dropping every frame after the first malformed one is
/// worse than yielding an error and continuing.
pub fn frames(bytes: &[u8]) -> AdtsFrames<'_> {
    AdtsFrames {
        buf: bytes,
        cursor: 0,
        done: false,
        resync: false,
    }
}

/// Construct a best-effort ADTS frame iterator that scans forward for
/// the next plausible 12-bit ADTS syncword (`0xFFF`) after each parse
/// error.
///
/// On a parse error at position `C`, the iterator yields the error
/// once, then advances the cursor to the next plausible syncword in
/// `buf[C+1..]` (or to the end-of-buffer if none is found, after which
/// the iterator terminates).
///
/// Strict callers (fuzzers, conformance tests) should use [`frames`];
/// stats/telemetry callers should prefer `frames_with_resync` to
/// avoid stream-wide stat undercount on first bit-flip.
pub fn frames_with_resync(bytes: &[u8]) -> AdtsFrames<'_> {
    AdtsFrames {
        buf: bytes,
        cursor: 0,
        done: false,
        resync: true,
    }
}
