//! RFC 7798 packetization without DONL: single NAL unit payloads and FU.

use super::{Packetizer, RtpPayload, clamp};

/// The H.265 [`Packetizer`]: RFC 7798 single NAL unit payloads (§4.4.1) and
/// fragmentation units (§4.4.3), no aggregation packets and no DONL
/// (`sprop-max-don-diff` = 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct H265Packetizer;

impl Packetizer for H265Packetizer {
    fn packetize(&self, nals: &[&[u8]], mtu_payload: usize) -> Vec<RtpPayload> {
        tst_rtp::h265::packetizer::packetize(nals.iter().copied(), clamp(mtu_payload))
    }
}
