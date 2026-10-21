//! Access unit → RTP payloads, for the video track of a WebRTC session.
//!
//! The packetization itself is tst-rtp's (RFC 6184 for H.264, RFC 7798 for
//! H.265, both without aggregation packets); this module adapts it to the
//! server's per-codec dispatch and keeps the payload size workable. The RTP
//! header (sequence number, timestamp, SSRC, payload type) is written by the
//! WebRTC stack, so a payload carries only its bytes and its marker bit.

mod h264;
mod h265;

pub use h264::H264Packetizer;
pub use h265::H265Packetizer;

/// One RTP payload of an access unit with its marker bit, which is set on
/// the access unit's last payload.
pub use tst_rtp::packet::Chunk as RtpPayload;

use crate::VideoCodec;

/// The RTP payload size to use when nothing else is known: 1180 bytes, so
/// the RTP packet with str0m's header extensions and the SRTP tag stays at
/// or under the 1200-byte packet every other WebRTC sender emits, inside
/// the 1280-byte IPv6 minimum MTU with the IP and UDP headers.
pub const DEFAULT_MTU_PAYLOAD: usize = 1180;

/// The smallest RTP payload size a [`Packetizer`] uses. A smaller
/// `mtu_payload` is raised to this value.
pub const MIN_MTU_PAYLOAD: usize = 64;

/// Splits one access unit into RTP payloads.
pub trait Packetizer: Send {
    /// Packetize one access unit.
    ///
    /// `nals` are its NAL units in decoding order, each starting with its
    /// NAL header and without a start code. A NAL unit of at most
    /// `mtu_payload` bytes travels as a single NAL unit payload; a larger
    /// one is split into fragmentation units of at most `mtu_payload`
    /// bytes. Empty NAL units are skipped. The last payload carries
    /// `marker = true`; an access unit with no non-empty NAL unit yields
    /// nothing. An `mtu_payload` below [`MIN_MTU_PAYLOAD`] is raised to it.
    fn packetize(&self, nals: &[&[u8]], mtu_payload: usize) -> Vec<RtpPayload>;
}

/// The packetizer for `codec`: [`H264Packetizer`] or [`H265Packetizer`].
///
/// `None` for a codec WebRTC playback does not carry (H.266, AV1); a mount
/// with such a codec is already refused by
/// [`MountConfig::validate`](crate::MountConfig::validate).
pub fn packetizer_for(codec: VideoCodec) -> Option<Box<dyn Packetizer>> {
    match codec {
        VideoCodec::H264 => Some(Box::new(H264Packetizer)),
        VideoCodec::H265 => Some(Box::new(H265Packetizer)),
        VideoCodec::H266 | VideoCodec::Av1 => None,
    }
}

/// The payload size handed to tst-rtp, which panics below its own minimum.
fn clamp(mtu_payload: usize) -> usize {
    mtu_payload.max(MIN_MTU_PAYLOAD)
}
