//! Mount configuration.

pub use tst_core::codec::VideoCodec;

use crate::error::WhepServerError;

/// What a mount is fed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountKind {
    /// Encoded access units, KLV records and audio frames pushed one by one.
    Elementary,
    /// A complete MPEG-TS byte stream, demultiplexed by the mount.
    Mp2t,
}

/// What happens to audio pushed into a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioPolicy {
    /// Audio is discarded; viewers get video and KLV only.
    Drop,
    /// Opus frames are forwarded to viewers unchanged.
    PassthroughOpus,
}

/// Where KLV records travel to the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KlvCarriage {
    /// In an SEI message inside the video access unit and on the `klv` data
    /// channel.
    SeiAndDataChannel,
    /// In an SEI message inside the video access unit only.
    SeiOnly,
    /// On the `klv` data channel only.
    DataChannelOnly,
}

/// The largest per-access-unit SEI budget a mount accepts (1 MiB).
const MAX_SEI_BUDGET_BYTES: u32 = 1024 * 1024;

/// The smallest per-viewer queue a mount accepts, in RTP packets.
const MIN_VIEWER_QUEUE_PACKETS: u32 = 16;

/// The configuration of one mount.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MountConfig {
    /// What the mount is fed with.
    pub kind: MountKind,
    /// The video codec. H.264 and H.265 are supported.
    pub video: VideoCodec,
    /// What happens to audio.
    pub audio: AudioPolicy,
    /// Where KLV records travel.
    ///
    /// A data-channel record is at most
    /// [`KLV_DC_LIMIT`](crate::KLV_DC_LIMIT) bytes (131 059). A record
    /// above the viewer's advertised `max-message-size` (64 KiB when the
    /// browser states none) is browser-dependent: it may be dropped for
    /// that viewer, and the drop is counted.
    pub klv: KlvCarriage,
    /// The most KLV bytes carried in SEI per access unit (1 to 1 MiB).
    pub sei_budget_bytes: u32,
    /// Packets queued per viewer before the slowest viewer drops (at least 16).
    pub viewer_queue_packets: u32,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            kind: MountKind::Elementary,
            video: VideoCodec::H264,
            audio: AudioPolicy::Drop,
            klv: KlvCarriage::SeiAndDataChannel,
            sei_budget_bytes: 64 * 1024,
            viewer_queue_packets: 2048,
        }
    }
}

impl MountConfig {
    /// Checks every field against the ranges above.
    ///
    /// # Errors
    ///
    /// [`WhepServerError::InvalidConfig`] for an H.266 or AV1 video codec, a
    /// `sei_budget_bytes` of 0 or above 1 MiB, or a `viewer_queue_packets`
    /// below 16.
    pub fn validate(&self) -> Result<(), WhepServerError> {
        let invalid = |detail: String| Err(WhepServerError::InvalidConfig { detail });
        match self.video {
            VideoCodec::H264 | VideoCodec::H265 => {}
            VideoCodec::H266 | VideoCodec::Av1 => {
                return invalid(format!(
                    "video codec {:?} is not supported over WebRTC (H.264 and H.265 are)",
                    self.video
                ));
            }
        }
        if self.sei_budget_bytes == 0 || self.sei_budget_bytes > MAX_SEI_BUDGET_BYTES {
            return invalid(format!(
                "sei_budget_bytes {} is outside 1..={MAX_SEI_BUDGET_BYTES}",
                self.sei_budget_bytes
            ));
        }
        if self.viewer_queue_packets < MIN_VIEWER_QUEUE_PACKETS {
            return invalid(format!(
                "viewer_queue_packets {} is below {MIN_VIEWER_QUEUE_PACKETS}",
                self.viewer_queue_packets
            ));
        }
        Ok(())
    }
}
