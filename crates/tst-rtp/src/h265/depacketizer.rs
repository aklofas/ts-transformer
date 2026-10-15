//! RFC 7798 H.265 RTP depacketizer: the shared state machine
//! (`crate::depay`, the same rules as the H.264 depacketizer) with RFC
//! 7798's payload-header rules — single NAL unit packets (types 0–47),
//! aggregation packets (48) and fragmentation units (49). PACI packets
//! (50) poison the access unit; types 51–63 are ignored. Assumes
//! `sprop-max-don-diff` = 0 (no DONL fields), the default.

use tst_core::codec::VideoCodec;

use crate::depay::core::{Core, Format, Payload};
use crate::depay::{AccessUnit, DepayConfig, DepayStats};
use crate::packet::RtpHeader;

/// RFC 7798 payload-header rules.
pub(crate) struct H265;

impl Format for H265 {
    const PS_SLOTS: usize = 3; // VPS, SPS, PPS
    const AGG_PREFIX_LEN: usize = 2; // AP payload header
    const FU_PREFIX_LEN: usize = 3; // payload header + FU header

    fn classify(payload: &[u8]) -> Payload {
        if payload.len() < 2 {
            // A payload header is two bytes; anything shorter is malformed.
            return Payload::Poison;
        }
        match (payload[0] >> 1) & 0x3F {
            0..=47 => Payload::Single,
            48 => Payload::Aggregation,
            49 => Payload::Fragment,
            50 => Payload::Poison,
            _ => Payload::Reserved,
        }
    }

    fn nal_type(nal: &[u8]) -> u8 {
        (nal[0] >> 1) & 0x3F
    }

    fn is_idr(nal_type: u8) -> bool {
        tst_core::codec::nal::is_idr(VideoCodec::H265, nal_type)
    }

    fn ps_slot(nal_type: u8) -> Option<usize> {
        match nal_type {
            32 => Some(0),
            33 => Some(1),
            34 => Some(2),
            _ => None,
        }
    }

    fn fu_nal_header(fu: &[u8]) -> Vec<u8> {
        // F and the LayerId high bit from the payload header, Type from the
        // FU header, byte 1 (LayerId low bits, TID) unchanged.
        vec![(fu[0] & 0x81) | ((fu[2] & 0x3F) << 1), fu[1]]
    }
}

/// RFC 7798 H.265 RTP depacketizer. Same contract as
/// [`crate::h264::H264Depacketizer`]: [`feed`](Self::feed) every packet in
/// arrival order, drain [`next_au`](Self::next_au); never panics on any
/// input.
pub struct H265Depacketizer {
    core: Core<H265>,
}

impl H265Depacketizer {
    /// Construct a depacketizer.
    pub fn new(config: DepayConfig) -> Self {
        Self {
            core: Core::new(config),
        }
    }

    /// Feed one RTP packet.
    pub fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
        self.core.feed(header, payload);
    }

    /// The next completed access unit, if any.
    pub fn next_au(&mut self) -> Option<AccessUnit> {
        self.core.next_au()
    }

    /// Complete any open access unit and return it. Drain
    /// [`Self::next_au`] first.
    pub fn flush(&mut self) -> Option<AccessUnit> {
        self.core.flush()
    }

    /// A snapshot of the counters.
    pub fn stats(&self) -> DepayStats {
        self.core.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depay::ParameterSetInjection;
    use crate::h265::packetizer::{MIN_MAX_PAYLOAD, packetize};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn depacketize_after_packetize_is_the_access_unit(
            nals in proptest::collection::vec((0u8..=47, proptest::collection::vec(any::<u8>(), 0..3000)), 1..8),
            max_payload in MIN_MAX_PAYLOAD..1500usize,
            ssrc in any::<u32>(),
        ) {
            // F = 0, LayerId 0, TID 1.
            let nals: Vec<Vec<u8>> = nals.into_iter().map(|(t, body)| { let mut n = vec![t << 1, 0x01]; n.extend(body); n }).collect();
            let chunks = packetize(nals.iter().map(Vec::as_slice), max_payload);
            let mut d = H265Depacketizer::new(DepayConfig {
                parameter_set_injection: ParameterSetInjection::None,
                ..DepayConfig::default()
            });
            for (i, c) in chunks.iter().enumerate() {
                let mut h = RtpHeader::new(i as u16, 90_000, ssrc);
                h.marker = c.marker;
                d.feed(&h, &c.payload);
            }
            let au = d.next_au().expect("one access unit");
            let annexb: Vec<u8> = nals.iter().flat_map(|n| [0u8, 0, 0, 1].into_iter().chain(n.iter().copied())).collect();
            prop_assert_eq!(&au.annexb, &annexb);
            prop_assert_eq!(au.key_frame, nals.iter().any(|n| matches!(n[0] >> 1, 19 | 20)));
            prop_assert!(d.next_au().is_none());
        }
    }

    fn hdr(seq: u16, marker: bool) -> RtpHeader {
        let mut h = RtpHeader::new(seq, 3000, 1);
        h.marker = marker;
        h
    }

    #[test]
    fn an_aggregation_packet_unpacks_its_units_in_order() {
        // AP header (type 48), then [size][VPS][size][IDR].
        let ap = [
            0x60, 0x01, 0x00, 0x03, 0x40, 0x01, 0xAA, 0x00, 0x03, 0x26, 0x01, 0xBB,
        ];
        let mut d = H265Depacketizer::new(DepayConfig {
            parameter_set_injection: ParameterSetInjection::None,
            ..DepayConfig::default()
        });
        d.feed(&hdr(1, true), &ap);
        let au = d.next_au().unwrap();
        assert_eq!(
            au.annexb,
            vec![0, 0, 0, 1, 0x40, 0x01, 0xAA, 0, 0, 0, 1, 0x26, 0x01, 0xBB]
        );
        assert!(au.key_frame);
    }

    #[test]
    fn paci_poisons_and_short_payloads_are_discarded() {
        let mut d = H265Depacketizer::new(DepayConfig::default());
        d.feed(&hdr(1, false), &[0x64, 0x01, 0x00]); // type 50, PACI
        d.feed(&hdr(2, true), &[0x26, 0x01, 0xCC]);
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().aus_dropped, 1);
        d.feed(&hdr(3, true), &[0x26]); // 1-byte payload
        assert_eq!(d.stats().packets_discarded, 2);
    }

    #[test]
    fn cached_vps_sps_pps_are_injected_before_an_idr_in_that_order() {
        let vps = vec![0x40, 0x01, 0x0C];
        let sps = vec![0x42, 0x01, 0x01];
        let pps = vec![0x44, 0x01, 0xC1];
        let mut d = H265Depacketizer::new(DepayConfig {
            initial_parameter_sets: vec![pps.clone(), sps.clone(), vps.clone()],
            ..DepayConfig::default()
        });
        d.feed(&hdr(1, true), &[0x26, 0x01, 0xDD]);
        let au = d.next_au().unwrap();
        let mut want = Vec::new();
        for n in [&vps, &sps, &pps] {
            want.extend([0, 0, 0, 1]);
            want.extend(n.iter());
        }
        want.extend([0, 0, 0, 1, 0x26, 0x01, 0xDD]);
        assert_eq!(au.annexb, want);
    }
}
