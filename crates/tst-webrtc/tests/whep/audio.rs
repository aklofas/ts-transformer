//! The audio policy: `Drop` counts and discards, `PassthroughOpus` forwards
//! one Opus packet per push at the 48 kHz clock. The mount is driven
//! without a server; negotiation is judged on offers made by a str0m client.

use std::time::Instant;

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind};
use str0m::{Candidate, Rtc};
use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::mount::MediaMsg;
use tst_webrtc::server::negotiate::select_audio;
use tst_webrtc::{AudioPolicy, MountConfig, MountHandle, PushError};

/// The largest Opus packet: three 1275-byte frames (120 ms).
const MAX_OPUS_PACKET: usize = 1275 * 3;

fn opus_config() -> MountConfig {
    let mut cfg = MountConfig::default();
    cfg.audio = AudioPolicy::PassthroughOpus;
    cfg
}

/// A synthetic Opus packet: a 3-byte header-like prefix, then `n` bytes.
fn packet(n: u8) -> Vec<u8> {
    let mut p = vec![0xFC, 0x01, n];
    p.extend(std::iter::repeat_n(n, 40));
    p
}

fn audio_msgs(rx: &std::sync::mpsc::Receiver<MediaMsg>) -> Vec<(u32, Vec<u8>)> {
    rx.try_iter()
        .filter_map(|m| match m {
            MediaMsg::Audio { packet, .. } => Some((packet.rtp_ts, packet.payload.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn drop_counts_every_frame_and_returns_ok() {
    let (m, rx) = MountHandle::for_test(MountConfig::default());
    for i in 0..5 {
        m.push_audio(&packet(i), Pts90khz::new(i64::from(i) * 1800))
            .unwrap();
    }
    // Under Drop even a frame Opus would refuse is just counted.
    m.push_audio(&[], Pts90khz::new(0)).unwrap();
    let s = m.stats();
    assert_eq!((s.audio_frames_dropped, s.audio_frames_pushed), (6, 0));
    assert!(audio_msgs(&rx).is_empty());
}

#[test]
fn passthrough_forwards_one_packet_per_push_unchanged() {
    let (m, rx) = MountHandle::for_test(opus_config());
    for i in 0..4u8 {
        m.push_audio(&packet(i), Pts90khz::new(i64::from(i) * 1800))
            .unwrap();
    }
    let got = audio_msgs(&rx);
    assert_eq!(got.len(), 4);
    for (i, (_, payload)) in got.iter().enumerate() {
        assert_eq!(payload, &packet(i as u8));
    }
    let s = m.stats();
    assert_eq!((s.audio_frames_pushed, s.audio_frames_dropped), (4, 0));
}

#[test]
fn the_rtp_clock_is_48_khz_derived_from_the_90_khz_pts() {
    let (m, rx) = MountHandle::for_test(opus_config());
    // 20 ms frames: 1800 ticks are exactly 960 samples.
    for pts in [0i64, 1800, 3600, 90_000] {
        m.push_audio(&packet(1), Pts90khz::new(pts)).unwrap();
    }
    // Rounding: 48 kHz ticks are floor(pts * 8 / 15), taken from the PTS
    // itself, so the error never accumulates.
    for pts in [1i64, 2, 14, 15, 16] {
        m.push_audio(&packet(1), Pts90khz::new(pts)).unwrap();
    }
    let ts: Vec<u32> = audio_msgs(&rx).into_iter().map(|(t, _)| t).collect();
    let d = |a: usize, b: usize| ts[b].wrapping_sub(ts[a]);
    assert_eq!((d(0, 1), d(1, 2), d(0, 3)), (960, 960, 48_000));
    // The PTS 0 frame is the base of the second group.
    assert_eq!(
        [d(0, 4), d(0, 5), d(0, 6), d(0, 7), d(0, 8)],
        [0, 1, 7, 8, 8]
    );
}

#[test]
fn an_empty_or_oversize_packet_is_invalid_under_passthrough() {
    let (m, rx) = MountHandle::for_test(opus_config());
    assert!(matches!(
        m.push_audio(&[], Pts90khz::new(0)),
        Err(PushError::InvalidAudioFrame)
    ));
    assert!(matches!(
        m.push_audio(&vec![0u8; MAX_OPUS_PACKET + 1], Pts90khz::new(0)),
        Err(PushError::InvalidAudioFrame)
    ));
    m.push_audio(&vec![0u8; MAX_OPUS_PACKET], Pts90khz::new(0))
        .unwrap();
    let s = m.stats();
    assert_eq!((s.audio_frames_pushed, s.audio_frames_dropped), (1, 0));
    assert_eq!(audio_msgs(&rx).len(), 1);
}

#[test]
fn a_closed_mount_refuses_audio() {
    let (m, _rx) = MountHandle::for_test(opus_config());
    m.close_for_test();
    assert!(matches!(
        m.push_audio(&packet(1), Pts90khz::new(0)),
        Err(PushError::Closed)
    ));
}

/// The offer a str0m client makes: video, plus audio when asked.
fn offer(with_audio: bool) -> String {
    let mut client = Rtc::builder().build(Instant::now());
    let mut api = client.sdp_api();
    api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    if with_audio {
        api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    }
    let (offer, _pending) = api.apply().expect("an offer");
    offer.to_sdp_string()
}

fn accept(offer: &str) -> Rtc {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    let candidate = Candidate::host("192.0.2.1:40000".parse().unwrap(), "udp").unwrap();
    rtc.add_local_candidate(candidate);
    let parsed = SdpOffer::from_sdp_string(offer).expect("the offer parses");
    rtc.sdp_api().accept_offer(parsed).expect("accepted");
    rtc
}

#[test]
fn an_offer_with_opus_audio_selects_the_opus_payload() {
    let sdp = offer(true);
    let rtc = accept(&sdp);
    let (mid, pt) = select_audio(&rtc, &sdp).expect("an Opus payload");
    assert_eq!(rtc.media(mid).unwrap().kind(), MediaKind::Audio);
    let local = rtc
        .codec_config()
        .params()
        .iter()
        .find(|p| p.pt() == pt)
        .expect("the pt is configured");
    assert_eq!(local.spec().codec, Codec::Opus);
}

#[test]
fn an_offer_without_audio_selects_nothing() {
    let sdp = offer(false);
    assert!(select_audio(&accept(&sdp), &sdp).is_none());
}

#[test]
fn an_audio_offer_without_opus_selects_nothing() {
    // An audio section whose only payload is PCMU.
    let sdp = offer(true);
    let no_opus: String = sdp
        .lines()
        .filter(|l| !l.to_ascii_lowercase().contains("opus"))
        .map(|l| format!("{l}\r\n"))
        .collect();
    let rtc = accept(&sdp);
    assert!(select_audio(&rtc, &no_opus).is_none());
}

#[test]
fn a_push_the_media_queue_refuses_is_not_counted_as_forwarded() {
    let (m, rx) = MountHandle::for_test(opus_config());
    drop(rx);
    assert!(matches!(
        m.push_audio(&packet(1), Pts90khz::new(0)),
        Err(PushError::Closed)
    ));
    assert_eq!(m.stats().audio_frames_pushed, 0);
}

/// A P-slice access unit: one NAL unit, no parameter sets needed.
const P_FRAME: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x00, 0x01];

#[test]
fn a_pts_jump_rebases_the_audio_clock_and_leaves_the_video_clock_alone() {
    let (m, rx) = MountHandle::for_test(opus_config());
    let video = |pts: i64| m.push_video(P_FRAME, Pts90khz::new(pts), false).unwrap();
    video(0);
    video(3000);
    m.push_audio(&packet(1), Pts90khz::new(8_000_000_000))
        .unwrap();
    // The source wrapped or restarted: the audio PTS falls back by far
    // more than 10 s.
    m.push_audio(&packet(2), Pts90khz::new(0)).unwrap();
    m.push_audio(&packet(3), Pts90khz::new(1800)).unwrap();
    video(6000);
    let (mut audio, mut v) = (Vec::new(), Vec::new());
    for msg in rx.try_iter() {
        match msg {
            MediaMsg::Audio { packet, .. } => audio.push(packet.rtp_ts),
            MediaMsg::Frame { packet, .. } => v.push(packet.rtp_ts),
            _ => {}
        }
    }
    assert_eq!(audio.len(), 3);
    assert_eq!(
        audio[1].wrapping_sub(audio[0]),
        960,
        "contiguous across the jump"
    );
    assert_eq!(
        audio[2].wrapping_sub(audio[1]),
        960,
        "and continuing from it"
    );
    assert_eq!(m.stats().rtp_clock_rebases, 1);
    // The video clock moved only by its own PTS steps.
    assert_eq!(v.len(), 3);
    assert_eq!(
        (v[1].wrapping_sub(v[0]), v[2].wrapping_sub(v[1])),
        (3000, 3000)
    );
}

#[test]
fn only_rejected_media_lines_get_a_format() {
    use tst_webrtc::server::negotiate::repair_rejected_media;
    let answer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=audio 0 UDP/TLS/RTP/SAVPF \r\n\
                  a=mid:1\r\nm=audio 9 UDP/TLS/RTP/SAVPF \r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    let fixed = repair_rejected_media(answer);
    assert_eq!(
        fixed,
        "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=audio 0 UDP/TLS/RTP/SAVPF 0\r\n\
         a=mid:1\r\nm=audio 9 UDP/TLS/RTP/SAVPF \r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n"
    );
}
