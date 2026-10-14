//! Fixture-driven tests: load the synthetic .ts fixtures from
//! `tests/fixtures/audio/`, demux each, assert the audio stream
//! is correctly classified and produces non-empty frames.

use crate::common::fixtures::audio_fixture_path as fixture_path;
use std::fs;
use std::path::Path;
use tst_core::mpegts::demux::{
    Demuxer,
    event::{AudioCodec, DemuxEvent, SamplePayload, StreamKind},
};

fn demux_fixture(path: &Path) -> Vec<DemuxEvent> {
    let bytes = fs::read(path).expect("fixture present");
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).expect("feed succeeds");
    demuxer.flush();
    let mut events = Vec::new();
    while let Some(e) = demuxer.next_event() {
        events.push(e);
    }
    events
}

#[test]
fn fixture_mp2_classified_as_mp2() {
    let events = demux_fixture(&fixture_path("mp2.ts"));
    let audio_kind = events.iter().find_map(|e| match e {
        DemuxEvent::Sample {
            stream,
            payload: SamplePayload::Audio { codec, .. },
            ..
        } if matches!(stream.kind, StreamKind::Audio(_)) => Some(*codec),
        _ => None,
    });
    assert_eq!(audio_kind, Some(AudioCodec::Mp2));
}

#[test]
fn fixture_aac_adts_classified_as_aac() {
    let events = demux_fixture(&fixture_path("aac-adts.ts"));
    assert!(events.iter().any(|e| matches!(
        e,
        DemuxEvent::Sample {
            payload: SamplePayload::Audio {
                codec: AudioCodec::Aac,
                ..
            },
            ..
        }
    )));
}

#[test]
fn fixture_aac_latm_classified_as_aac_latm() {
    let events = demux_fixture(&fixture_path("aac-latm.ts"));
    assert!(events.iter().any(|e| matches!(
        e,
        DemuxEvent::Sample {
            payload: SamplePayload::Audio {
                codec: AudioCodec::AacLatm,
                ..
            },
            ..
        }
    )));
}

#[test]
fn fixture_ac3_classified_as_ac3() {
    let events = demux_fixture(&fixture_path("ac3.ts"));
    assert!(events.iter().any(|e| matches!(
        e,
        DemuxEvent::Sample {
            payload: SamplePayload::Audio {
                codec: AudioCodec::Ac3,
                ..
            },
            ..
        }
    )));
}

#[test]
fn fixture_audio_frames_non_empty() {
    for name in &["mp2.ts", "aac-adts.ts", "aac-latm.ts", "ac3.ts"] {
        let events = demux_fixture(&fixture_path(name));
        let audio_event = events.iter().find_map(|e| match e {
            DemuxEvent::Sample {
                payload: SamplePayload::Audio { frames, .. },
                ..
            } => Some(frames.len()),
            _ => None,
        });
        assert!(audio_event.is_some(), "{name}: no audio events");
        assert!(audio_event.unwrap() > 0, "{name}: empty frames");
    }
}
