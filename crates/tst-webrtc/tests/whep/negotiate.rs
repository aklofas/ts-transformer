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
use tst_webrtc::server::negotiate::{
    Negotiated, payload_types_are_consistent, profile_level_id_from_sps, select_video,
};

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

/// An offer with one BUNDLE over `sections`, each `(kind, mid, payload
/// numbers, payload lines)`, every section `recvonly`.
fn offer_with(sections: &[(&str, &str, &str, &str)]) -> String {
    let mids: Vec<&str> = sections.iter().map(|s| s.1).collect();
    let mut sdp = format!(
        "v=0\r\no=- 1 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
         a=fingerprint:sha-256 {}\r\na=group:BUNDLE {}\r\n",
        ["AA"; 32].join(":"),
        mids.join(" ")
    );
    for (kind, mid, pts, lines) in sections {
        sdp.push_str(&format!(
            "m={kind} 9 UDP/TLS/RTP/SAVPF {pts}\r\nc=IN IP4 0.0.0.0\r\na=recvonly\r\n\
             a=mid:{mid}\r\na=ice-ufrag:00000000\r\n\
             a=ice-pwd:00000000000000000000000000000000\r\na=setup:actpass\r\n\
             a=rtcp-mux\r\n{lines}"
        ));
    }
    sdp
}

/// The guard over str0m's rendering of `offer`, as the server runs it.
fn consistent(offer: &str) -> bool {
    let parsed = SdpOffer::from_sdp_string(offer).expect("the offer parses");
    payload_types_are_consistent(&parsed.to_sdp_string())
}

#[test]
fn browser_offers_keep_one_format_per_payload_type() {
    for (name, offer) in [("chrome", CHROME), ("firefox", FIREFOX), ("safari", SAFARI)] {
        assert!(consistent(offer), "{name}");
    }
    // A BUNDLE that repeats one numbering in two sections is consistent.
    let vp8 = "a=rtpmap:120 VP8/90000\r\na=rtpmap:124 rtx/90000\r\na=fmtp:124 apt=120\r\n";
    let twice = offer_with(&[
        ("video", "0", "120 124", vp8),
        ("video", "1", "120 124", vp8),
    ]);
    assert!(consistent(&twice));
    accept(&twice);
}

/// Each offer here panics str0m 0.24's `accept_offer` ("Pt locked multiple
/// times"); the guard refuses all of them.
#[test]
fn offers_that_give_one_payload_type_two_formats_are_refused() {
    let vp8_vp9 = "a=rtpmap:120 VP8/90000\r\na=rtpmap:121 VP9/90000\r\na=rtpmap:125 rtx/90000\r\n";
    let rtx_two_lines = format!("{vp8_vp9}a=fmtp:125 apt=120\r\na=fmtp:125 apt=121\r\n");
    let rtx_one_line = format!("{vp8_vp9}a=fmtp:125 apt=120;apt=121\r\n");
    let vp8_rtx = "a=rtpmap:120 VP8/90000\r\na=rtpmap:124 rtx/90000\r\na=fmtp:124 apt=120\r\n";
    let h264 = |plid: &str| {
        format!(
            "a=rtpmap:120 H264/90000\r\n\
             a=fmtp:120 packetization-mode=1;profile-level-id={plid}\r\n"
        )
    };
    let (baseline, high) = (h264("42e01f"), h264("64001f"));
    let cases = [
        (
            "an rtx apt on two lines",
            offer_with(&[("video", "0", "120 121 125", &rtx_two_lines)]),
        ),
        (
            "an rtx apt naming two payloads on one line",
            offer_with(&[("video", "0", "120 121 125", &rtx_one_line)]),
        ),
        (
            "rtx in one section, VP9 in the next",
            offer_with(&[
                ("video", "0", "120 124", vp8_rtx),
                ("video", "1", "124", "a=rtpmap:124 VP9/90000\r\n"),
            ]),
        ),
        (
            "two H.264 profiles under one number",
            offer_with(&[
                ("video", "0", "120", &baseline),
                ("video", "1", "120", &high),
            ]),
        ),
        (
            "VP8 in video, Opus in audio",
            offer_with(&[
                ("video", "0", "120", "a=rtpmap:120 VP8/90000\r\n"),
                ("audio", "1", "120", "a=rtpmap:120 opus/48000/2\r\n"),
            ]),
        ),
    ];
    for (name, offer) in &cases {
        assert!(!consistent(offer), "{name}");
    }
}

/// A section whose `m=` line lists only 120 and that maps 121 twice. str0m
/// checks only the numbers in `m=`, but claims every `a=rtpmap`, so
/// `accept_offer` panics on the second claim of 121.
fn unlisted_number_mapped_twice() -> String {
    offer_with(&[(
        "video",
        "0",
        "120",
        "a=rtpmap:120 H264/90000\r\n\
         a=fmtp:120 packetization-mode=1;profile-level-id=42001f\r\n\
         a=rtpmap:121 VP8/90000\r\n\
         a=rtpmap:121 VP9/90000\r\n",
    )])
}

#[test]
fn a_number_outside_the_m_line_mapped_twice_is_refused() {
    let offer = unlisted_number_mapped_twice();
    assert!(!consistent(&offer));
    // The guard is what stands between this offer and the panic.
    let panicked = std::panic::catch_unwind(|| accept(&offer)).is_err();
    assert!(panicked, "str0m accepted the offer without the guard");
}

#[test]
fn a_section_repeating_or_stray_payload_lines_is_refused() {
    let h264 = "a=rtpmap:120 H264/90000\r\n\
                a=fmtp:120 packetization-mode=1;profile-level-id=42001f\r\n";
    // str0m's parser refuses this one itself; the guard does too.
    let mapped_twice = offer_with(&[(
        "video",
        "0",
        "120",
        &format!("{h264}a=rtpmap:120 H264/90000\r\n"),
    )]);
    assert!(!payload_types_are_consistent(&mapped_twice));
    let cases = [
        (
            "one number with two fmtp lines",
            format!("{h264}a=fmtp:120 packetization-mode=1\r\n"),
        ),
        (
            "an rtpmap for a number not in m=",
            format!("{h264}a=rtpmap:121 VP8/90000\r\n"),
        ),
        (
            "an fmtp for a number not in m=",
            format!("{h264}a=fmtp:121 apt=120\r\n"),
        ),
    ];
    for (name, lines) in &cases {
        let offer = offer_with(&[("video", "0", "120", lines)]);
        assert!(!consistent(&offer), "{name}");
    }
    // The same section without the extra line is consistent.
    let offer = offer_with(&[("video", "0", "120", h264)]);
    assert!(consistent(&offer));
    accept(&offer);
}
