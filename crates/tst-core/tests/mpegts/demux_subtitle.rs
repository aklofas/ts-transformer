//! Integration tests for subtitle / caption receiver side
//! (`mpegts::demux`).

use crate::common::mux_demux::{demux_bytes_flushed as collect_events, drain_mux as drain_all};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, SamplePayload, StreamKind, StrictMode,
    SubtitleCodec as DemuxSub,
};
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSub, VideoCodec,
};

/// Build a TS byte stream containing a single subtitle PES on PID 0x200,
/// alongside a placeholder H.264 video PID required by the muxer config.
fn build_ts_with_one_subtitle_pes(codec: MuxSub, payload: &[u8]) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, codec);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), payload)
        .unwrap();
    drain_all(&mut mux)
}

#[test]
fn demux_classifies_webvtt_in_ts_via_vttc_format_identifier() {
    let bytes = build_ts_with_one_subtitle_pes(MuxSub::WebVttInTs, b"WEBVTT\nx-cue\n");
    let events = collect_events(&bytes);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                stream,
                payload: SamplePayload::Subtitle { codec: DemuxSub::WebVttInTs, .. },
                ..
            } if stream.pid == 0x200
        )),
        "expected WebVttInTs subtitle Sample on PID 0x200, got {events:?}"
    );
}

#[test]
fn demux_classifies_dvb_subtitling_via_descriptor_tag_0x59() {
    let bytes = build_ts_with_one_subtitle_pes(
        MuxSub::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
        // Synthetic page composition segment header bytes.
        &[0x20, 0x00, 0x10, 0x01, 0x00, 0x14, 0x00, 0x00],
    );
    let events = collect_events(&bytes);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Subtitle {
                    codec: DemuxSub::DvbSubtitling,
                    ..
                },
                ..
            }
        )),
        "expected DvbSubtitling subtitle Sample, got {events:?}"
    );
}

#[test]
fn demux_classifies_dvb_teletext_via_descriptor_tag_0x56() {
    let bytes = build_ts_with_one_subtitle_pes(
        MuxSub::DvbTeletext {
            language: *b"eng",
            teletext_type: 0x02,
            magazine_number: 1,
            page_number: 0x88,
        },
        // Synthetic teletext data unit prefix bytes.
        &[0x10, 0x02, 0x10, 0x00, 0x00],
    );
    let events = collect_events(&bytes);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Subtitle {
                    codec: DemuxSub::DvbTeletext,
                    ..
                },
                ..
            }
        )),
        "expected DvbTeletext subtitle Sample, got {events:?}"
    );
}

#[test]
fn demux_classifies_cea708_standalone_via_ga94_format_identifier() {
    let bytes = build_ts_with_one_subtitle_pes(
        MuxSub::Cea708Standalone,
        // Synthetic cc_data bytes.
        &[0xFE, 0x80, 0x80, 0x80],
    );
    let events = collect_events(&bytes);
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Subtitle {
                    codec: DemuxSub::Cea708Standalone,
                    ..
                },
                ..
            }
        )),
        "expected Cea708Standalone subtitle Sample, got {events:?}"
    );
}

/// Patch the `data_identifier` byte (offset 0 of the §6.2 envelope) inside
/// a muxed DVB-subtitle TS byte stream. Finds the envelope by locating the
/// triplet `[0x20, 0x00, marker]` (the muxer auto-prepends data_id=0x20,
/// stream_id=0x00, and the first caller-supplied segment byte is `marker`).
/// Returns the patched bytes.
fn patch_dvb_sub_data_identifier(mut bytes: Vec<u8>, marker: u8, new_id: u8) -> Vec<u8> {
    let needle = [0x20u8, 0x00, marker];
    let pos = bytes
        .windows(3)
        .position(|w| w == needle)
        .expect("muxed TS should contain the §6.2 envelope header followed by the marker");
    bytes[pos] = new_id;
    bytes
}

#[test]
fn demux_dvb_sub_non_conformant_data_identifier_lenient_emits_sample_and_issue() {
    // Use a recognizable first-segment byte (0x0F == DVB-sub segment sync per
    // §7.2) so the patcher can locate the envelope unambiguously.
    let segments = vec![0x0F, 0xAB, 0xCD, 0xEF];
    let mut raw = build_ts_with_one_subtitle_pes(
        MuxSub::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
        &segments,
    );
    // Flip data_identifier from the §6.2 binding (0x20) to a value that
    // sits in the legacy §7.1 permissive range but is non-conformant per
    // §6.2 Table 3.
    raw = patch_dvb_sub_data_identifier(raw, 0x0F, 0x21);
    let events = collect_events(&raw);

    // Lenient (default) — both a NonConformant issue AND the stripped Sample.
    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::DvbSubDataIdentifier { observed: 0x21 },
                ..
            }
        )
    });
    let sample_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Subtitle {
                    codec: DemuxSub::DvbSubtitling,
                    payload,
                },
                ..
            } if payload.as_slice() == segments.as_slice()
        )
    });
    assert!(
        issue_seen,
        "expected DvbSubDataIdentifier {{ observed: 0x21 }}, got {events:?}"
    );
    assert!(
        sample_seen,
        "lenient mode should still emit the stripped subtitle sample, got {events:?}"
    );
}

#[test]
fn demux_dvb_sub_non_conformant_data_identifier_strict_suppresses_sample() {
    let segments = vec![0x0F, 0xAB, 0xCD, 0xEF];
    let mut raw = build_ts_with_one_subtitle_pes(
        MuxSub::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
        &segments,
    );
    raw = patch_dvb_sub_data_identifier(raw, 0x0F, 0x21);

    let mut demux = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    // StrictMode::Full converts the first DvbSubDataIdentifier to a fatal —
    // feed() may surface it as Err. Drain events regardless.
    let _ = demux.feed(&raw);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::DvbSubDataIdentifier { observed: 0x21 },
                ..
            }
        )
    });
    let dvb_sub_sample_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Subtitle {
                    codec: DemuxSub::DvbSubtitling,
                    ..
                },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "strict mode should still surface the DvbSubDataIdentifier issue, got {events:?}"
    );
    assert!(
        !dvb_sub_sample_seen,
        "strict mode should suppress the DVB-sub Sample event, got {events:?}"
    );
}

#[test]
fn demux_program_map_event_lists_subtitle_pid_with_program_number() {
    let bytes = build_ts_with_one_subtitle_pes(MuxSub::WebVttInTs, b"WEBVTT\n");
    let events = collect_events(&bytes);
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap event");
    let sub = pm
        .streams
        .iter()
        .find(|s| s.pid == 0x200)
        .expect("subtitle stream on PID 0x200 in ProgramMap");
    assert!(matches!(
        sub.kind,
        StreamKind::Subtitle(DemuxSub::WebVttInTs)
    ));
    assert_eq!(sub.program_number, 1);
}
