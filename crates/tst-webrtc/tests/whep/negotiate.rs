//! Video payload selection over browser offers. Chrome's offer is a
//! headless capture; the Firefox and Safari offers are reconstructions (see
//! `tests/fixtures/offers/README.md`). The tests assert what the selection
//! does with each offer's payload lines. No socket is opened: the local
//! candidate only has to exist for str0m to answer.

use std::time::Instant;

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::{Candidate, Rtc};
use tst_webrtc::VideoCodec;
use tst_webrtc::server::negotiate::{Negotiated, profile_level_id_from_sps, select_video};

const CHROME: &str = include_str!("../fixtures/offers/chrome-155.sdp");
const FIREFOX: &str = include_str!("../fixtures/offers/firefox-144.sdp");
const SAFARI: &str = include_str!("../fixtures/offers/safari-26.sdp");

/// A constrained-baseline level 3.0 SPS NAL unit (header byte included).
const SPS_NAL: &[u8] = &[
    0x67, 0x42, 0xC0, 0x1E, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08, 0x84, 0x6A,
];

/// An ice-lite, RTP-mode server that has accepted `offer`.
fn accept(offer: &str) -> Rtc {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    // A TEST-NET-1 address: the candidate is advertised, never bound.
    let candidate = Candidate::host("192.0.2.1:40000".parse().unwrap(), "udp").unwrap();
    rtc.add_local_candidate(candidate);
    let parsed = SdpOffer::from_sdp_string(offer).expect("the offer parses");
    rtc.sdp_api()
        .accept_offer(parsed)
        .expect("the offer is accepted");
    rtc
}

fn select(offer: &str, want: VideoCodec, sps: Option<[u8; 3]>) -> Negotiated {
    select_video(&accept(offer), offer, want, sps)
}

/// The offer's `profile-level-id` for the chosen H.264 payload, after
/// checking that str0m holds a mode-1 H.264 entry under that number.
fn chosen_profile_level_id(offer: &str, sps: Option<[u8; 3]>) -> String {
    let rtc = accept(offer);
    let Negotiated::Video { pt, .. } = select_video(&rtc, offer, VideoCodec::H264, sps) else {
        panic!("no H.264 payload chosen")
    };
    let local = rtc
        .codec_config()
        .params()
        .iter()
        .find(|p| p.pt() == pt)
        .expect("the chosen pt is configured");
    assert_eq!(local.spec().codec, Codec::H264);
    assert_eq!(local.spec().format.packetization_mode, Some(1));
    let fmtp = offer
        .lines()
        .find_map(|l| l.strip_prefix(&format!("a=fmtp:{} ", *pt)))
        .expect("the offer has an fmtp line for the chosen pt");
    assert!(fmtp.contains("packetization-mode=1"), "{fmtp}");
    let plid = fmtp
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix("profile-level-id="))
        .expect("a profile-level-id");
    plid.to_string()
}

#[test]
fn every_browser_offer_negotiates_h264() {
    for (name, offer) in [("chrome", CHROME), ("firefox", FIREFOX), ("safari", SAFARI)] {
        let got = select(offer, VideoCodec::H264, None);
        assert!(
            matches!(
                got,
                Negotiated::Video {
                    codec: VideoCodec::H264,
                    ..
                }
            ),
            "{name}: {got:?}"
        );
    }
}

#[test]
fn the_sps_profile_picks_the_matching_payload() {
    let sps = profile_level_id_from_sps(VideoCodec::H264, SPS_NAL);
    for (name, offer) in [("chrome", CHROME), ("firefox", FIREFOX), ("safari", SAFARI)] {
        let plid = chosen_profile_level_id(offer, sps);
        assert!(plid.starts_with("42"), "{name}: profile-level-id {plid}");
    }
    // Chrome lists baseline before Main: a Main SPS moves the choice past
    // the first mode-1 payload, and without an SPS the first one wins.
    assert_eq!(
        chosen_profile_level_id(CHROME, Some([0x4D, 0x40, 0x1F])),
        "4d001f"
    );
    assert_eq!(chosen_profile_level_id(CHROME, None), "42001f");
}

#[test]
fn h265_negotiates_only_where_offered() {
    assert!(matches!(
        select(SAFARI, VideoCodec::H265, None),
        Negotiated::Video {
            codec: VideoCodec::H265,
            ..
        }
    ));
    for (name, offer) in [("chrome", CHROME), ("firefox", FIREFOX)] {
        let got = select(offer, VideoCodec::H265, None);
        assert_eq!(
            got,
            Negotiated::Rejected {
                reason: "no H.265 payload in the offer"
            },
            "{name}"
        );
    }
}

#[test]
fn an_offer_without_mode_1_h264_is_rejected() {
    for (name, offer) in [("chrome", CHROME), ("firefox", FIREFOX), ("safari", SAFARI)] {
        assert!(offer.contains("packetization-mode=1"), "{name}");
        let mode0 = offer.replace("packetization-mode=1", "packetization-mode=0");
        assert_eq!(
            select(&mode0, VideoCodec::H264, None),
            Negotiated::Rejected {
                reason: "no packetization-mode=1 H.264 payload in the offer"
            },
            "{name}"
        );
    }
}

#[test]
fn codecs_webrtc_does_not_carry_are_rejected() {
    for want in [VideoCodec::H266, VideoCodec::Av1] {
        assert!(matches!(
            select(CHROME, want, None),
            Negotiated::Rejected { .. }
        ));
    }
}

#[test]
fn sps_profile_level_id() {
    assert_eq!(
        profile_level_id_from_sps(VideoCodec::H264, SPS_NAL),
        Some([0x42, 0xC0, 0x1E])
    );
    assert_eq!(profile_level_id_from_sps(VideoCodec::H265, SPS_NAL), None);
    // A PPS header byte, and a truncated SPS.
    assert_eq!(
        profile_level_id_from_sps(VideoCodec::H264, &[0x68, 0xCE]),
        None
    );
    assert_eq!(profile_level_id_from_sps(VideoCodec::H264, &[0x67]), None);
    assert_eq!(profile_level_id_from_sps(VideoCodec::H264, &[]), None);
}

#[test]
fn a_payload_str0m_holds_under_another_profile_is_not_chosen() {
    // A second Constrained Baseline mode-1 payload, 105, listed first.
    // str0m's Constrained Baseline entry is taken by 108, and 105 is the
    // number str0m's own (unmatched) High entry, 64001f, sits on: str0m
    // lists 105 as a remote payload with a mode-1 H.264 entry, but not
    // the profile the offer names.
    let offer = CHROME
        .replace("UDP/TLS/RTP/SAVPF 96 97", "UDP/TLS/RTP/SAVPF 105 96 97")
        .replace(
            "a=rtpmap:102 H264/90000",
            "a=rtpmap:105 H264/90000\r\n\
             a=fmtp:105 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
             a=rtpmap:102 H264/90000",
        );
    let rtc = accept(&offer);
    let entry = rtc
        .codec_config()
        .params()
        .iter()
        .find(|p| *p.pt() == 105)
        .expect("str0m holds an entry under 105");
    assert_eq!(entry.spec().codec, Codec::H264);
    assert_eq!(entry.spec().format.packetization_mode, Some(1));
    assert_eq!(entry.spec().format.profile_level_id, Some(0x64001f));
    let media = rtc
        .media(str0m::media::Mid::from("0"))
        .expect("video media");
    assert!(media.remote_pts().iter().any(|pt| **pt == 105));

    let Negotiated::Video { pt, .. } = select_video(&rtc, &offer, VideoCodec::H264, None) else {
        panic!("no H.264 payload chosen")
    };
    assert_eq!(*pt, 102);
}

#[test]
fn an_h264_payload_without_profile_level_id_negotiates_as_baseline() {
    // Chrome's first mode-1 payload, 102, with its profile-level-id left
    // out: RFC 6184 reads that as Baseline, and so does str0m.
    let line = "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f";
    assert!(CHROME.contains(line));
    let offer = CHROME.replace(
        line,
        "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1",
    );
    let Negotiated::Video { pt, .. } = select(&offer, VideoCodec::H264, None) else {
        panic!("no H.264 payload chosen")
    };
    assert_eq!(*pt, 102);
}
