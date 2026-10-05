//! End-to-end test: feed a TS fixture through the demuxer, collect Audio
//! events, parse frames with the new iterator, and assert the reported
//! (sample_rate, channel_count) is consistent across all frames per PID.
//!
//! Fixtures are committed: ~3-second 440 Hz mono sine waves
//! muxed with a synthetic H.264 video. We assert iterator behavior, not
//! audio content (the parser doesn't decode audio).
//!
//! All three fixtures are ffmpeg-generated at 44100 Hz mono. Verified with
//! `ffprobe -show_streams -select_streams a <file>.ts`.

use std::path::Path;
use tst_core::codec;
use tst_core::mpegts::demux::{AudioCodec, DemuxEvent, Demuxer, SamplePayload};
use tst_core::shared::SharedBytes;

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join(name)
}

/// Feed a fixture file through the demuxer; return all (pid, codec, frames)
/// tuples from Audio sample events.
fn collect_audio_pes(name: &str) -> Vec<(u16, AudioCodec, SharedBytes)> {
    let path = fixture_path(name);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {:?} should exist: {e}", path));
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).expect("fixture should feed cleanly");
    demuxer.flush();
    let mut out = Vec::new();
    while let Some(ev) = demuxer.next_event() {
        if let DemuxEvent::Sample {
            stream,
            payload: SamplePayload::Audio { codec, frames },
            ..
        } = ev
        {
            out.push((stream.pid, codec, frames));
        }
    }
    out
}

#[test]
fn mp2_roundtrip_parses_frames_consistently() {
    let pess = collect_audio_pes("mp2.ts");
    assert!(
        !pess.is_empty(),
        "mp2.ts should produce at least one Audio event"
    );

    let mut total_frames = 0;
    let mut seen_sample_rate: Option<u32> = None;
    let mut seen_channels: Option<u8> = None;

    for (_pid, codec, frames) in &pess {
        assert_eq!(*codec, AudioCodec::Mp2);
        for frame_result in codec::mpegaudio::frames(frames) {
            let f = frame_result.expect("mp2.ts frames should parse cleanly");
            match seen_sample_rate {
                Some(sr) => assert_eq!(f.sample_rate_hz, sr, "sample rate jumped mid-stream"),
                None => seen_sample_rate = Some(f.sample_rate_hz),
            }
            match seen_channels {
                Some(c) => assert_eq!(f.channels, c, "channel count jumped mid-stream"),
                None => seen_channels = Some(f.channels),
            }
            total_frames += 1;
        }
    }

    assert!(total_frames > 0, "should have parsed at least one frame");
    // ffprobe reports 44100 Hz mono for this fixture.
    assert_eq!(seen_sample_rate, Some(44100));
    assert_eq!(seen_channels, Some(1));
}

#[test]
fn aac_adts_roundtrip_parses_frames_consistently() {
    let pess = collect_audio_pes("aac-adts.ts");
    assert!(
        !pess.is_empty(),
        "aac-adts.ts should produce at least one Audio event"
    );

    let mut total_frames = 0;
    let mut seen_sample_rate: Option<u32> = None;
    let mut seen_channels: Option<u8> = None;

    for (_pid, codec, frames) in &pess {
        assert_eq!(*codec, AudioCodec::Aac);
        for frame_result in codec::aac::frames(frames) {
            let f = frame_result.expect("aac-adts.ts frames should parse cleanly");
            match seen_sample_rate {
                Some(sr) => assert_eq!(f.sample_rate_hz, sr, "sample rate jumped mid-stream"),
                None => seen_sample_rate = Some(f.sample_rate_hz),
            }
            // `.channels()` returns `Some` for canonical layouts
            // (`channel_configuration` 1..=7) and `None` for PCE-defined
            // (`channel_configuration == 0`). The conformant aac-adts
            // fixture uses canonical mono.
            let ch = f
                .channels()
                .expect("aac-adts fixture uses canonical channel_configuration");
            match seen_channels {
                Some(c) => assert_eq!(ch, c, "channel count jumped mid-stream"),
                None => seen_channels = Some(ch),
            }
            total_frames += 1;
        }
    }

    assert!(total_frames > 0);
    // ffprobe reports 44100 Hz mono for this fixture.
    assert_eq!(seen_sample_rate, Some(44100));
    assert_eq!(seen_channels, Some(1));
}

#[test]
fn mp3_conformant_roundtrip_parses_layer3() {
    let pess = collect_audio_pes("mp3-conformant.ts");
    assert!(
        !pess.is_empty(),
        "mp3-conformant.ts should produce at least one Audio event"
    );

    let mut total_frames = 0;
    let mut seen_layer: Option<codec::mpegaudio::Layer> = None;
    let mut seen_sample_rate: Option<u32> = None;

    for (_pid, _codec, frames) in &pess {
        // mp3-conformant.ts uses stream_type 0x03, so the demuxer classifies
        // it as AudioCodec::Mp2. The frame-level layer field in the MPEG audio
        // header reveals it is actually Layer III (MP3).
        for frame_result in codec::mpegaudio::frames(frames) {
            let f = frame_result.expect("mp3 frames should parse cleanly");
            match seen_layer {
                Some(l) => assert_eq!(f.layer, l, "layer changed mid-stream"),
                None => seen_layer = Some(f.layer),
            }
            match seen_sample_rate {
                Some(sr) => assert_eq!(f.sample_rate_hz, sr),
                None => seen_sample_rate = Some(f.sample_rate_hz),
            }
            total_frames += 1;
        }
    }

    assert!(total_frames > 0);
    assert_eq!(seen_layer, Some(codec::mpegaudio::Layer::III));
    // ffprobe reports 44100 Hz for this fixture.
    assert_eq!(seen_sample_rate, Some(44100));
}
