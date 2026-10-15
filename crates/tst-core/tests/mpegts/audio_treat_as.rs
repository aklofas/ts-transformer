//! Tests `DemuxerConfig::stream_kind_overrides` against the non-conformant
//! audio fixtures (MP3 on user-private 0xF1, MP3 mislabeled as Layer II 0x03).

use crate::common::fixtures::audio_fixture_path as fixture_path;
use std::fs;
use tst_core::mpegts::demux::{
    AudioCodec, Demuxer, DemuxerConfig,
    event::{DemuxEvent, SamplePayload, StreamKind},
};

/// Walks the PMT to find a PID whose stream_type matches the target.
/// Crude PMT parse; assumes single-section + no adaptation field.
fn find_audio_pid_by_stream_type(ts_bytes: &[u8], target_st: u8) -> u16 {
    for packet in ts_bytes.chunks_exact(188) {
        if packet[0] != 0x47 {
            continue;
        }
        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        if pid != 0x1000 {
            continue;
        }
        // Payload starts after 4-byte TS header + 1-byte pointer.
        let payload_start = 5;
        // PMT header: table_id (8) + section_syntax_indicator (1) + ... + pointer_field.
        // We're already past pointer, so skip 12 bytes to reach ES loop.
        let mut off = payload_start + 12; // past PMT header to ES loop
        while off + 5 <= packet.len() {
            let st = packet[off];
            let pid_bytes = ((packet[off + 1] as u16 & 0x1F) << 8) | packet[off + 2] as u16;
            let info_len = (((packet[off + 3] as usize) & 0x0F) << 8) | packet[off + 4] as usize;
            if st == target_st {
                return pid_bytes;
            }
            off += 5 + info_len;
        }
    }
    panic!("no PID with stream_type {target_st:#x} found in PMT");
}

#[test]
fn mp3_on_0xf1_classified_as_unknown_by_default() {
    let bytes = fs::read(fixture_path("mp3-on-0xF1.ts")).expect("fixture present");
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).expect("feed succeeds");
    demuxer.flush();

    let mut unknown_found = false;
    while let Some(e) = demuxer.next_event() {
        // Look for a ProgramMap event that reports the audio PID as Unknown(0xF1).
        if let DemuxEvent::ProgramMap(pm) = &e {
            for stream in &pm.streams {
                if matches!(stream.kind, StreamKind::Unknown(0xF1)) {
                    unknown_found = true;
                    break;
                }
            }
        }
    }
    assert!(
        unknown_found,
        "default classification: Unknown(0xF1) in ProgramMap"
    );
}

#[test]
fn mp3_on_0xf1_routes_to_audio_via_treat_as() {
    let bytes = fs::read(fixture_path("mp3-on-0xF1.ts")).expect("fixture present");
    // Find the MP3 PID by reading the PMT manually.
    let mp3_pid = find_audio_pid_by_stream_type(&bytes, 0xF1);

    let mut options = DemuxerConfig::default();
    options
        .stream_kind_overrides
        .insert(mp3_pid, StreamKind::Audio(AudioCodec::Mp2));
    let mut demuxer = Demuxer::with_config(options);
    demuxer.feed(&bytes).expect("feed succeeds");
    demuxer.flush();

    let mp2_audio = std::iter::from_fn(|| demuxer.next_event()).any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Audio {
                    codec: AudioCodec::Mp2,
                    ..
                },
                ..
            }
        )
    });
    assert!(mp2_audio, "treat_as override routed PID to typed audio");
}

#[test]
fn mp3_on_0x03_already_classified_as_mp2() {
    // Fixture has MP3 audio on stream_type 0x03 — demuxer treats 0x03 as MP2.
    // The bitstream-level mismatch (Layer III on Layer II label) is out of
    // scope for the carriage layer; caller's decoder handles it.
    let bytes = fs::read(fixture_path("mp3-on-0x03.ts")).expect("fixture present");
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).expect("feed succeeds");
    demuxer.flush();

    let mp2_audio = std::iter::from_fn(|| demuxer.next_event()).any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Audio {
                    codec: AudioCodec::Mp2,
                    ..
                },
                ..
            }
        )
    });
    assert!(
        mp2_audio,
        "stream_type 0x03 classified as Mp2 without override"
    );
}
