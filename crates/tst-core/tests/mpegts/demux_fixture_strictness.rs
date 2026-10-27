//! Real-capture-shaped fixtures must keep decoding as every demux strictness
//! fix lands: same sample count in lenient and TimingOnly mode, and none of
//! the issue classes the demux strictness fixes touch.
use crate::common::fixtures::fixture_path;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, StrictMode};

const FIXTURES: &[&str] = &[
    "audio/mp2.ts",
    "audio/aac-adts.ts",
    "audio/aac-latm.ts",
    "audio/ac3.ts",
    "subtitles/dvb_subtitling_eng.ts",
    "subtitles/dvb_subtitling_multi_lang.ts",
    "subtitles/dvb_teletext_eng.ts",
    "subtitles/subtitle_with_klv_same_program.ts",
    "subtitles/webvtt_multi_program_with_klv.ts",
];

/// Feed the whole file, flush, and drain every event.
fn run(path: &str, mode: StrictMode) -> (Result<(), String>, Vec<DemuxEvent>) {
    let bytes = std::fs::read(fixture_path(path)).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut d = Demuxer::with_config(DemuxerConfig::builder().strict(mode).build());
    let fed = d.feed(&bytes).map_err(|e| e.to_string());
    d.flush();
    let mut events = Vec::new();
    while let Some(e) = d.next_event() {
        events.push(e);
    }
    (fed, events)
}

fn touched(i: &NonConformantIssue) -> bool {
    matches!(
        i,
        NonConformantIssue::MissingRequiredPts { .. }
            | NonConformantIssue::LatmFraming { .. }
            | NonConformantIssue::MalformedPes { .. }
            | NonConformantIssue::PcrAnomaly { .. }
            | NonConformantIssue::PtsAnomaly { .. }
            | NonConformantIssue::MissingMetadataDescriptor
            | NonConformantIssue::PesHeaderMalformed { .. }
            | NonConformantIssue::PsiSyntax { .. }
    )
}

#[test]
fn real_capture_fixtures_decode_without_touched_issue_classes() {
    for f in FIXTURES {
        let (off, ev_off) = run(f, StrictMode::Off);
        let (timing, ev_timing) = run(f, StrictMode::TimingOnly);
        assert!(off.is_ok() && timing.is_ok(), "{f}: {off:?} {timing:?}");
        let samples = |ev: &[DemuxEvent]| {
            ev.iter()
                .filter(|e| matches!(e, DemuxEvent::Sample { .. } | DemuxEvent::Metadata { .. }))
                .count()
        };
        assert!(samples(&ev_off) > 0, "{f}: no samples");
        assert_eq!(samples(&ev_off), samples(&ev_timing), "{f}");
        let bad: Vec<_> = ev_off
            .iter()
            .filter_map(|e| match e {
                DemuxEvent::NonConformant { issue, .. } if touched(issue) => Some(issue.clone()),
                _ => None,
            })
            .collect();
        assert!(bad.is_empty(), "{f}: {bad:?}");
    }
}
