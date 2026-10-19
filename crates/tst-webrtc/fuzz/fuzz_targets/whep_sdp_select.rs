#![no_main]

use std::time::Instant;

use libfuzzer_sys::fuzz_target;
use str0m::change::SdpOffer;
use str0m::{Candidate, Rtc};
use tst_webrtc::VideoCodec;
use tst_webrtc::server::negotiate::{Negotiated, select_video};

fn server() -> Rtc {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    // A TEST-NET-1 address: advertised in the answer, never bound.
    let candidate = Candidate::host("192.0.2.1:40000".parse().unwrap(), "udp").unwrap();
    rtc.add_local_candidate(candidate);
    rtc
}

fn select_both(rtc: &Rtc, sdp: &str) {
    for want in [VideoCodec::H264, VideoCodec::H265] {
        for sps in [None, Some([0x42, 0xC0, 0x1E])] {
            if let Negotiated::Video { codec, .. } = select_video(rtc, sdp, want, sps) {
                assert_eq!(codec, want);
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(sdp) = std::str::from_utf8(data) else {
        return;
    };
    // The offer reader runs on any text, accepted or not.
    select_both(&server(), sdp);
    // str0m's parser and answerer are theirs; the selection over what they
    // accepted is the target.
    let Ok(offer) = SdpOffer::from_sdp_string(sdp) else {
        return;
    };
    let mut rtc = server();
    if rtc.sdp_api().accept_offer(offer).is_ok() {
        select_both(&rtc, sdp);
    }
});
