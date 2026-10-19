//! RFC 7798 packetization through the tst-webrtc adapter: single NAL unit
//! payloads, fragmentation units, the MTU clamp, and a round trip through
//! the tst-rtp depacketizer.

use tst_core::codec::annexb::nals;
use tst_rtp::RtpHeader;
use tst_rtp::depay::DepayConfig;
use tst_rtp::h265::H265Depacketizer;
use tst_webrtc::VideoCodec;
use tst_webrtc::packetize::{MIN_MTU_PAYLOAD, RtpPayload, packetizer_for};

fn h265(nals: &[&[u8]], mtu: usize) -> Vec<RtpPayload> {
    packetizer_for(VideoCodec::H265)
        .expect("H.265 has a packetizer")
        .packetize(nals, mtu)
}

fn pairs(out: &[RtpPayload]) -> Vec<(Vec<u8>, bool)> {
    out.iter().map(|p| (p.payload.clone(), p.marker)).collect()
}

#[test]
fn small_nals_are_single_nal_payloads_with_marker_on_last() {
    let out = h265(
        &[&[0x40, 0x01, 1], &[0x42, 0x01, 2], &[0x26, 0x01, 3, 4]],
        1200,
    );
    assert_eq!(
        pairs(&out),
        vec![
            (vec![0x40, 0x01, 1], false),
            (vec![0x42, 0x01, 2], false),
            (vec![0x26, 0x01, 3, 4], true),
        ]
    );
}

#[test]
fn large_nal_is_fragmented_with_fu_headers() {
    // IDR_W_RADL (type 19): header [0x26, 0x01].
    let mut nal = vec![0x26u8, 0x01];
    nal.extend((0..2500u32).map(|i| (i % 251) as u8));
    let out = h265(&[&nal], 1200);
    let body = nal.len() - 2;
    assert_eq!(out.len(), body.div_ceil(1200 - 3)); // 1197, 1197, 106
    assert_eq!(out[0].payload[..3], [0x62, 0x01, 0x80 | 19]);
    assert_eq!(out[1].payload[..3], [0x62, 0x01, 19]);
    assert_eq!(out[2].payload[..3], [0x62, 0x01, 0x40 | 19]);
    assert_eq!(out[0].payload.len(), 1200);
    assert_eq!(out[2].payload.len(), 106 + 3);
    assert!(!out[0].marker && !out[1].marker && out[2].marker);
    let rebuilt: Vec<u8> = [0x26, 0x01]
        .into_iter()
        .chain(out.iter().flat_map(|p| p.payload[3..].iter().copied()))
        .collect();
    assert_eq!(rebuilt, nal);
}

#[test]
fn exactly_mtu_sized_nal_is_not_fragmented() {
    let mut nal = vec![0x02u8, 0x01];
    nal.resize(1200, 0x41);
    let out = h265(&[&nal], 1200);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].payload, nal);
    assert!(out[0].marker);
}

#[test]
fn empty_input_yields_nothing() {
    assert!(h265(&[], 1200).is_empty());
    assert!(h265(&[&[]], 1200).is_empty());
}

#[test]
fn mtu_below_the_minimum_is_clamped() {
    let mut nal = vec![0x26u8, 0x01];
    nal.extend((0..200u32).map(|i| i as u8));
    for mtu in [0, 3, MIN_MTU_PAYLOAD - 1] {
        let out = h265(&[&nal], mtu);
        assert!(out.iter().all(|p| p.payload.len() <= MIN_MTU_PAYLOAD));
        assert_eq!(out.len(), 200usize.div_ceil(MIN_MTU_PAYLOAD - 3));
    }
}

#[test]
fn depacketizer_round_trip_through_tst_rtp_is_byte_identical() {
    // One access unit: VPS, SPS, PPS and an IDR, each after a 4-byte start
    // code (the framing the depacketizer emits).
    let au = include_bytes!("../fixtures/h265/idr_with_vps_sps_pps.h265");
    let nal_list: Vec<&[u8]> = nals(au).collect();
    assert_eq!(nal_list.len(), 4);
    let out = h265(&nal_list, 1200);
    assert!(out.len() > 4, "the IDR must be fragmented");
    let mut d = H265Depacketizer::new(DepayConfig::default());
    for (i, p) in out.iter().enumerate() {
        let mut h = RtpHeader::new(i as u16, 90_000, 7);
        h.marker = p.marker;
        d.feed(&h, &p.payload);
    }
    let got = d.next_au().or_else(|| d.flush()).expect("one AU");
    assert_eq!(got.annexb, au.to_vec());
    assert!(got.key_frame);
}

#[test]
fn codecs_without_webrtc_carriage_have_no_packetizer() {
    assert!(packetizer_for(VideoCodec::H266).is_none());
    assert!(packetizer_for(VideoCodec::Av1).is_none());
}
