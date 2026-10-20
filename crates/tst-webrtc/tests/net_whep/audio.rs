//! Audio over a loopback WHEP session: Opus pass-through reaches a viewer
//! that offered audio, and nothing audio-related reaches one that did not
//! or on a mount that drops audio.

use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::{AudioPolicy, MountConfig, MountHandle, WhepServer};

use crate::fixtures::{TestViewer, h264_gop, join, join_with_audio, pump_all};

const CONNECT: Duration = Duration::from_secs(5);
const DELIVER: Duration = Duration::from_secs(10);

/// 20 ms of the 90 kHz clock.
const TICKS_20_MS: i64 = 1800;

fn opus_config() -> MountConfig {
    let mut cfg = MountConfig::default();
    cfg.audio = AudioPolicy::PassthroughOpus;
    cfg
}

/// A synthetic Opus packet: a 3-byte header-like prefix, then the index.
fn opus_packet(i: u8) -> Vec<u8> {
    let mut p = vec![0xFC, 0x01, i];
    p.extend(std::iter::repeat_n(i, 30));
    p
}

fn connect(m: &MountHandle, viewers: &mut [TestViewer]) {
    let n = viewers.len();
    let ok = pump_all(
        viewers,
        |vs| {
            vs.iter().all(|v| v.connected)
                && m.sessions()
                    .iter()
                    .filter(|s| s.connected_for.is_some())
                    .count()
                    == n
        },
        CONNECT,
    );
    let traces: Vec<String> = viewers.iter().map(TestViewer::trace).collect();
    assert!(ok, "viewers did not connect: {traces:?}");
}

fn push_gop(m: &MountHandle, frames: usize) {
    for (i, (au, key)) in h264_gop().iter().take(frames).enumerate() {
        m.push_video(au, Pts90khz::new(i as i64 * 3000), *key)
            .unwrap();
    }
}

#[test]
fn fifty_opus_packets_arrive_on_the_audio_mid_at_48_khz() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", opus_config()).unwrap();
    let mut v = TestViewer::new();
    let (_, answer) = join_with_audio(srv.local_addr(), "/cam", &mut v);
    assert!(
        !answer.contains("m=audio 0 "),
        "audio is answered: {answer}"
    );
    connect(&m, std::slice::from_mut(&mut v));

    push_gop(&m, 3);
    for i in 0..50u8 {
        m.push_audio(&opus_packet(i), Pts90khz::new(i64::from(i) * TICKS_20_MS))
            .unwrap();
    }
    let ok = v.pump(
        |v| v.audio_frames.len() >= 50 && v.frames.len() >= 3,
        DELIVER,
    );
    assert!(ok, "{}", v.trace());
    assert_eq!(v.audio_frames.len(), 50);
    assert_eq!(v.frames.len(), 3);
    for (i, (ts, data)) in v.audio_frames.iter().enumerate() {
        assert_eq!(data, &opus_packet(i as u8), "packet {i}");
        if i > 0 {
            // 20 ms at 48 kHz.
            assert_eq!(ts - v.audio_frames[i - 1].0, 960, "packet {i}");
        }
    }
    let ms = m.stats();
    assert_eq!((ms.audio_frames_pushed, ms.audio_frames_dropped), (50, 0));
}

#[test]
fn a_viewer_without_audio_gets_video_unaffected_under_passthrough() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", opus_config()).unwrap();
    let mut v = TestViewer::new();
    join(srv.local_addr(), "/cam", &mut v, false);
    connect(&m, std::slice::from_mut(&mut v));

    push_gop(&m, 5);
    for i in 0..10u8 {
        m.push_audio(&opus_packet(i), Pts90khz::new(i64::from(i) * TICKS_20_MS))
            .unwrap();
    }
    assert!(v.pump(|v| v.frames.len() >= 5, DELIVER), "{}", v.trace());
    assert!(v.audio_frames.is_empty());
    assert!(v.error.is_none(), "{}", v.trace());
}

#[test]
fn a_dropping_mount_rejects_the_audio_section_and_still_serves_video() {
    let srv = WhepServer::bind("whep://127.0.0.1:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let (_, answer) = join_with_audio(srv.local_addr(), "/cam", &mut v);
    assert!(
        answer.contains("m=audio 0 "),
        "the audio section is rejected: {answer}"
    );
    connect(&m, std::slice::from_mut(&mut v));

    push_gop(&m, 5);
    for i in 0..10u8 {
        m.push_audio(&opus_packet(i), Pts90khz::new(i64::from(i) * TICKS_20_MS))
            .unwrap();
    }
    assert!(v.pump(|v| v.frames.len() >= 5, DELIVER), "{}", v.trace());
    assert!(v.audio_frames.is_empty());
    assert_eq!(m.stats().audio_frames_dropped, 10);
}
