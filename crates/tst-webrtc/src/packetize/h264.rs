//! RFC 6184 packetization mode 1: single NAL unit payloads and FU-A.

use super::{Packetizer, RtpPayload, clamp};

/// The H.264 [`Packetizer`]: RFC 6184 single NAL unit payloads (§5.6) and
/// FU-A fragments (§5.8), no STAP-A. Parameter sets travel as single NAL
/// units.
#[derive(Debug, Clone, Copy, Default)]
pub struct H264Packetizer;

impl Packetizer for H264Packetizer {
    fn packetize(&self, nals: &[&[u8]], mtu_payload: usize) -> Vec<RtpPayload> {
        tst_rtp::h264::packetizer::packetize(nals.iter().copied(), clamp(mtu_payload))
    }
}
