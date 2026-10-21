//! Mount configuration.

pub use tst_core::codec::VideoCodec;

use crate::error::WhepServerError;

/// What a mount is fed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountKind {
    /// Encoded access units, KLV records and audio frames pushed one by one.
    Elementary,
    /// A complete MPEG-TS byte stream, demultiplexed by the mount: feed it
    /// with [`MountHandle::push_ts`](crate::MountHandle::push_ts).
    Mp2t,
}

/// What happens to audio pushed into a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioPolicy {
    /// Audio is discarded; viewers get video and KLV only.
    Drop,
    /// Each pushed buffer is one Opus packet (RFC 6716), forwarded
    /// unchanged to viewers whose offer asks to receive Opus, one RTP
    /// payload per packet at the 48 kHz clock (RFC 7587). Other viewers
    /// (no Opus offered, or an audio section that is `sendonly` or
    /// `inactive`) get video and KLV only.
    PassthroughOpus,
}

/// Where KLV records travel to the viewer.
///
/// The `klv` data channel is opened by the server (DCEP). The viewer's
/// offer must carry a data-channel section (create any data channel before
/// `createOffer`), and the channel arrives in `ondatachannel` with the
/// label `klv`. It is reliable and unordered: a record never waits behind
/// an earlier one, and each message names its frame's RTP timestamp.
/// Without such a section the viewer gets no channel, and no drop is
/// counted for it.
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
    /// [`KLV_DC_LIMIT`](crate::KLV_DC_LIMIT) bytes (256 KiB), and the
    /// server's SCTP send buffer is sized for it. A record is dropped for a
    /// viewer whose send buffer has no room for it. A record is sent as one
    /// data-channel message of the record plus a 13-byte header; for a
    /// viewer whose `max-message-size` is smaller than that message the
    /// record is dropped. The size is 64 KiB when the viewer states none
    /// and 256 KiB in current Chrome and Safari, so their largest record is
    /// 262 131 bytes; Firefox takes any record up to the limit. Each drop
    /// is counted.
    pub klv: KlvCarriage,
    /// The most KLV bytes carried in SEI per access unit (1 to 1 MiB).
    pub sei_budget_bytes: u32,
    /// The RTP payloads one viewer's queue holds (at least 16).
    ///
    /// The server writes each viewer's queue out on every pass of its media
    /// thread, so this is a budget per pass: a frame that does not fit what
    /// is left is dropped whole for that viewer, and the viewer waits for the
    /// next keyframe. A keyframe larger than the whole budget is admitted
    /// into an empty queue, so the queue can exceed it by one keyframe. A
    /// viewer whose queue stays full, with nothing written, for 30 s is
    /// closed as disconnected.
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
    /// below 16, and for [`MountKind::Mp2t`] with
    /// [`AudioPolicy::PassthroughOpus`] (the TS demuxer has no Opus mapping,
    /// so the negotiated audio track would never carry a packet).
    pub fn validate(&self) -> Result<(), WhepServerError> {
        let invalid = |detail: String| Err(WhepServerError::InvalidConfig { detail });
        match self.kind {
            MountKind::Elementary | MountKind::Mp2t => {}
        }
        match (self.kind, self.audio) {
            (_, AudioPolicy::Drop) | (MountKind::Elementary, AudioPolicy::PassthroughOpus) => {}
            (MountKind::Mp2t, AudioPolicy::PassthroughOpus) => {
                return invalid(
                    "AudioPolicy::PassthroughOpus needs an elementary mount: the MPEG-TS \
                     demuxer has no Opus mapping"
                        .into(),
                );
            }
        }
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
