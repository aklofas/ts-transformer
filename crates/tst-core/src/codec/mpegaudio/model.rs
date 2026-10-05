//! MPEG audio public types.

use crate::codec::CodecParseError;
use alloc::vec::Vec;

/// MPEG audio layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    I,
    II,
    III,
}

/// MPEG audio version. MPEG-2.5 is the de-facto half-rate extension
/// (8 / 11.025 / 12 kHz Layer III); not part of any ratified ISO spec
/// but ubiquitous in consumer MP3 streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Mpeg1,
    Mpeg2,
    Mpeg2_5,
}

/// MPEG audio channel mode (header bits 25-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Stereo,
    JointStereo,
    DualChannel,
    Mono,
}

/// Decoded MPEG audio frame. Borrows from the source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub layer: Layer,
    pub version: Version,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub channel_mode: ChannelMode,
    pub channels: u8,
    pub frame_length_bytes: u32,
    pub samples_per_frame: u16,
    pub has_crc: bool,
    pub raw_header: [u8; 4],
    pub(super) body: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Full-frame slice (header + body, including CRC bytes when
    /// `has_crc`). The `raw_header` field is the first 4 bytes of this
    /// slice copied for ownership convenience.
    pub fn bytes(&self) -> &'a [u8] {
        self.body
    }

    /// Promote this borrowed frame to a [`FrameOwned`] by copying the body.
    pub fn to_owned(&self) -> FrameOwned {
        FrameOwned {
            layer: self.layer,
            version: self.version,
            bitrate_kbps: self.bitrate_kbps,
            sample_rate_hz: self.sample_rate_hz,
            channel_mode: self.channel_mode,
            channels: self.channels,
            frame_length_bytes: self.frame_length_bytes,
            samples_per_frame: self.samples_per_frame,
            has_crc: self.has_crc,
            raw_header: self.raw_header,
            body: self.body.to_vec(),
        }
    }
}

/// Owned variant of [`Frame`].
///
/// `body: Vec<u8>` instead of `&'a [u8]` — usable across FFI / thread /
/// async boundaries where the borrowed source slice doesn't outlive the
/// consumer.
///
/// Round-trip with [`Frame::to_owned`] / [`Self::as_ref`].
///
/// # Example — collect owned frames for a Java consumer
/// ```
/// use tst_core::codec::mpegaudio::{frames, FrameOwned};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let payload: &[u8] = &[/* MPEG-Audio bytes */];
/// # let payload: &[u8] = &[];
/// let owned: Vec<FrameOwned> = frames(payload)
///     .filter_map(Result::ok)
///     .map(|f| f.to_owned())
///     .collect();
/// // `owned` outlives `payload` — safe to wrap as a Java List<MpegAudioFrame>.
/// let _ = owned;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameOwned {
    pub layer: Layer,
    pub version: Version,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub channel_mode: ChannelMode,
    pub channels: u8,
    pub frame_length_bytes: u32,
    pub samples_per_frame: u16,
    pub has_crc: bool,
    pub raw_header: [u8; 4],
    pub body: Vec<u8>,
}

impl FrameOwned {
    /// Borrow this owned frame as a [`Frame`] — zero-copy.
    pub fn as_ref(&self) -> Frame<'_> {
        Frame {
            layer: self.layer,
            version: self.version,
            bitrate_kbps: self.bitrate_kbps,
            sample_rate_hz: self.sample_rate_hz,
            channel_mode: self.channel_mode,
            channels: self.channels,
            frame_length_bytes: self.frame_length_bytes,
            samples_per_frame: self.samples_per_frame,
            has_crc: self.has_crc,
            raw_header: self.raw_header,
            body: &self.body,
        }
    }
}

/// Iterator over MPEG audio frames in `bytes`.
///
/// Construct with [`super::frames`] for the strict (fail-fast) variant
/// or [`super::frames_with_resync`] for the best-effort variant that
/// scans forward for the next plausible syncword after a parse error.
#[must_use]
pub struct Frames<'a> {
    pub(super) buf: &'a [u8],
    pub(super) cursor: usize,
    pub(super) done: bool,
    /// When `true`, parse errors do NOT terminate the iterator.
    /// Instead, `frames_next` scans forward from `cursor + 1` for the
    /// next plausible 11-bit MPEG audio syncword (`0x7FF` in the top
    /// 11 bits of a 16-bit window) and repositions there. The current
    /// error is still yielded; subsequent `next()` calls resume from
    /// the new cursor.
    pub(super) resync: bool,
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<Frame<'a>, CodecParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        super::decode::frames_next(self)
    }
}
