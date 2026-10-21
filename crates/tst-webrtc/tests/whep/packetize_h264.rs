//! RFC 6184 packetization through the tst-webrtc adapter: single NAL unit
//! payloads, FU-A fragments, the MTU clamp, and a round trip through the
//! tst-rtp depacketizer.

use tst_core::codec::annexb::nals;
use tst_rtp::RtpHeader;
use tst_rtp::h264::{H264Depacketizer, H264DepayConfig};
use tst_webrtc::VideoCodec;
use tst_webrtc::packetize::{DEFAULT_MTU_PAYLOAD, MIN_MTU_PAYLOAD, RtpPayload, packetizer_for};

fn h264(nals: &[&[u8]], mtu: usize) -> Vec<RtpPayload> {
    packetizer_for(VideoCodec::H264)
        .expect("H.264 has a packetizer")
        .packetize(nals, mtu)
}

fn pairs(out: &[RtpPayload]) -> Vec<(Vec<u8>, bool)> {
    out.iter().map(|p| (p.payload.clone(), p.marker)).collect()
}

#[test]
fn small_nals_are_single_nal_payloads_with_marker_on_last() {
    let out = h264(&[&[0x67, 1, 2], &[0x68, 3], &[0x65, 4, 5, 6]], 1200);
    assert_eq!(
        pairs(&out),
        vec![
            (vec![0x67, 1, 2], false),
            (vec![0x68, 3], false),
            (vec![0x65, 4, 5, 6], true),
        ]
    );
}

#[test]
fn large_nal_is_fragmented_with_fu_a_headers() {
    let mut nal = vec![0x65u8];
    nal.extend((0..2500u32).map(|i| (i % 251) as u8));
    let out = h264(&[&nal], 1200);
    assert_eq!(out.len(), 3); // body 2500 / (1200 - 2) = 1198, 1198, 104
    assert_eq!(out[0].payload[0], (0x65 & 0xE0) | 28);
    assert_eq!(out[0].payload[1], 0x80 | 5);
    assert_eq!(out[1].payload[1], 5);
    assert_eq!(out[2].payload[1], 0x40 | 5);
    assert_eq!(out[0].payload.len(), 1200);
    assert_eq!(out[2].payload.len(), 104 + 2);
    assert!(!out[0].marker && !out[1].marker && out[2].marker);
    let rebuilt: Vec<u8> = std::iter::once(0x65)
        .chain(out.iter().flat_map(|p| p.payload[2..].iter().copied()))
        .collect();
    assert_eq!(rebuilt, nal);
}

#[test]
fn exactly_mtu_sized_nal_is_not_fragmented() {
    let nal = vec![0x41u8; 1200];
    let out = h264(&[&nal], 1200);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].payload, nal);
    assert!(out[0].marker);
}

#[test]
fn empty_input_yields_nothing() {
    assert!(h264(&[], 1200).is_empty());
    assert!(h264(&[&[], &[]], 1200).is_empty());
}

#[test]
fn default_mtu_payload_keeps_the_srtp_packet_inside_the_ipv6_minimum_mtu() {
    assert_eq!(DEFAULT_MTU_PAYLOAD, 1180);
    // RTP header 12, str0m's extensions up to 16, SRTP tag up to 16: the
    // packet is at most 1226 bytes, inside 1280 with 48 bytes of IP + UDP.
    assert!(12 + 16 + DEFAULT_MTU_PAYLOAD + 16 + 48 <= 1280);
    let nal = vec![0x41u8; 3000];
    let out = h264(&[&nal], DEFAULT_MTU_PAYLOAD);
    assert!(out.iter().all(|p| p.payload.len() <= DEFAULT_MTU_PAYLOAD));
}

#[test]
fn mtu_below_the_minimum_is_clamped() {
    let mut nal = vec![0x65u8];
    nal.extend((0..200u32).map(|i| i as u8));
    for mtu in [0, 1, 2, MIN_MTU_PAYLOAD - 1] {
        let out = h264(&[&nal], mtu);
        assert!(out.iter().all(|p| p.payload.len() <= MIN_MTU_PAYLOAD));
        assert_eq!(out.len(), 200usize.div_ceil(MIN_MTU_PAYLOAD - 2));
        assert_eq!(out[0].payload.len(), MIN_MTU_PAYLOAD);
    }
}

#[test]
fn depacketizer_round_trip_through_tst_rtp_is_byte_identical() {
    // One access unit: SPS, PPS and an IDR, each after a 4-byte start code
    // (the framing the depacketizer emits).
    let au = include_bytes!("../fixtures/h264/idr_with_sps_pps.h264");
    let nal_list: Vec<&[u8]> = nals(au).collect();
    assert_eq!(nal_list.len(), 3);
    let out = h264(&nal_list, 1200);
    assert!(out.len() > 3, "the IDR must be fragmented");
    let mut d = H264Depacketizer::new(H264DepayConfig::default());
    for (i, p) in out.iter().enumerate() {
        let mut h = RtpHeader::new(i as u16, 90_000, 7);
        h.marker = p.marker;
        d.feed(&h, &p.payload);
    }
    let got = d.next_au().or_else(|| d.flush()).expect("one AU");
    assert_eq!(got.annexb, au.to_vec());
    assert!(got.key_frame);
}
