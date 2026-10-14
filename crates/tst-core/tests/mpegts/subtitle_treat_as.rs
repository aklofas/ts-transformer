//! Fixture-driven `stream_kind_overrides` (a.k.a. `treat_as`) tests for the
//! non-conformant subtitle fixture. The fixture was built via the audio config
//! path (PID 0x200, stream_type 0x03 MP2) — without an override the demuxer
//! classifies the PID as `Audio(Mp2)`; with an override it reclassifies as
//! `Subtitle(WebVttInTs)` and additionally emits `SubtitleMissingDescriptor`
//! since the PMT entry has no recognized subtitle descriptor.

use crate::common::fixtures::subtitle_fixture as load;
use crate::common::mux_demux::collect_events;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, SamplePayload, StreamKind,
    SubtitleCodec,
};

#[test]
fn non_conformant_classifies_as_audio_without_treat_as() {
    let bytes = load("non_conformant_subtitle_missing_descriptor.ts");
    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let events = collect_events(&mut demux);
    let subtitle_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::Sample {
                    payload: SamplePayload::Subtitle { .. },
                    ..
                }
            )
        })
        .count();
    let audio_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::Sample {
                    payload: SamplePayload::Audio { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        subtitle_count, 0,
        "should not classify as subtitle without treat_as"
    );
    assert!(audio_count >= 1, "expected Audio Sample event(s)");
}

#[test]
fn treat_as_reclassifies_non_conformant_pid_to_webvtt() {
    let bytes = load("non_conformant_subtitle_missing_descriptor.ts");
    let mut opts = DemuxerConfig::default();
    opts.stream_kind_overrides
        .insert(0x200, StreamKind::Subtitle(SubtitleCodec::WebVttInTs));
    let mut demux = Demuxer::with_config(opts);
    demux.feed(&bytes).unwrap();
    demux.flush();
    let events = collect_events(&mut demux);
    let subtitle_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::Sample {
                    payload: SamplePayload::Subtitle {
                        codec: SubtitleCodec::WebVttInTs,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert!(
        subtitle_count >= 1,
        "expected >= 1 WebVttInTs Sample event after stream_kind_overrides remap, got {subtitle_count}"
    );
}

#[test]
fn treat_as_with_missing_descriptor_emits_non_conformant_issue() {
    let bytes = load("non_conformant_subtitle_missing_descriptor.ts");
    let mut opts = DemuxerConfig::default();
    opts.stream_kind_overrides
        .insert(0x200, StreamKind::Subtitle(SubtitleCodec::WebVttInTs));
    let mut demux = Demuxer::with_config(opts);
    demux.feed(&bytes).unwrap();
    demux.flush();
    let events = collect_events(&mut demux);
    let nc = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::SubtitleMissingDescriptor { .. },
                ..
            }
        )
    });
    assert!(
        nc,
        "expected SubtitleMissingDescriptor NonConformant event when stream_kind_overrides routes a PID with no recognized subtitle descriptor"
    );
}
