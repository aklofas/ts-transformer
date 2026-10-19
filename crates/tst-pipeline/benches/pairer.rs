//! Pairer benches: realtime mode and buffered mode over a synthetic
//! 1000-sample stream.
//!
//! The Pairer is fed `DemuxEvent`s — the same event type that
//! `DemuxReceiver` emits. Synthetic events here skip the demux layer
//! entirely, which keeps the bench tight on pairing logic only.
//!
//! Two benchmark functions:
//! - `pairer_realtime_1000`: eager emission; no lookahead buffer.
//! - `pairer_buffered_1000`: 100 ms buffered window; bidirectional match.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tst_core::mpegts::au_cell::CellFragmentIndication;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, MetadataKind, SamplePayload, StreamId, StreamKind, VideoCodec,
};
use tst_core::shared::SharedBytes;
use tst_pipeline::ext::pairing::{Pairer, PairerConfig, PairerMode};

const VIDEO_PID: u16 = 0x0100;
const KLV_PID: u16 = 0x0200;

/// Synthetic `DemuxEvent::Sample` (H.264 video). 5 KiB payload simulates a
/// realistic non-keyframe NAL unit size for SD/HD ISR video.
///
/// PTS is in 90 kHz ticks. 33 ms per frame at ~30 fps = 2 970 ticks/frame.
fn make_video_event(frame_index: usize) -> DemuxEvent {
    let pts = (frame_index as i64) * 2_970; // ~33 ms at 90 kHz
    DemuxEvent::Sample {
        stream: StreamId {
            pid: VIDEO_PID,
            kind: StreamKind::Video(VideoCodec::H264),
            program_number: 1,
        },
        pts: Pts90khz::new(pts),
        dts: Some(Pts90khz::new(pts)),
        payload: SamplePayload::Video {
            codec: VideoCodec::H264,
            // Raw-first: the demuxer emits the encoded access unit; the pairer
            // now opt-in `split_video`s it. Build a realistic 5 KiB Annex-B AU
            // (start code + NAL header + body) so the benchmark exercises the
            // per-event split cost the pairer incurs in the new pipeline.
            raw: {
                let mut au = vec![0x00, 0x00, 0x00, 0x01]; // Annex-B start code
                // IDR vs P NAL header
                au.push(if frame_index.is_multiple_of(30) {
                    0x65
                } else {
                    0x41
                });
                au.extend(std::iter::repeat_n(0xA5u8, 5_000)); // body
                SharedBytes::from_vec(au)
            },
            random_access_indicator: frame_index.is_multiple_of(30),
            av1_carriage: None,
        },
    }
}

/// Synthetic `DemuxEvent::Metadata` (KLV). 200-byte payload approximates
/// a minimal ST 0601 local set (UL key + BER length + a few tags).
///
/// PTS is offset by +450 ticks (~5 ms) from video to simulate realistic
/// encoder-side KLV vs video clock skew.
fn make_klv_event(frame_index: usize) -> DemuxEvent {
    let pts = (frame_index as i64) * 2_970 + 450; // same cadence, +5 ms skew
    DemuxEvent::Metadata {
        stream: StreamId {
            pid: KLV_PID,
            kind: StreamKind::KlvSync {
                declared_link: Some(VIDEO_PID),
            },
            program_number: 1,
        },
        pts: Pts90khz::new(pts),
        kind: MetadataKind::KlvSyncAuCell {
            metadata_service_id: 0x00,
            sequence_number: frame_index as u8,
            cell_fragment_indication: CellFragmentIndication::Complete,
            decoder_config_flag: false,
            random_access_indicator: true,
            was_reassembled: false,
            cell_count: 1,
        },
        payload: vec![0x42; 200],
    }
}

/// Realtime mode: emit immediately on each feed call. No lookahead buffer;
/// KLV that hasn't arrived by the time video is fed is treated as unpaired.
fn bench_realtime(c: &mut Criterion) {
    // Build event lists once outside the measured loop — allocation is not
    // what we're timing here.
    let video_events: Vec<DemuxEvent> = (0..1000).map(make_video_event).collect();
    let klv_events: Vec<DemuxEvent> = (0..1000).map(make_klv_event).collect();

    // Interleave video and KLV in arrival order (alternating) to give the
    // pairer a realistic feed pattern. In realtime mode, pairing succeeds
    // only when KLV arrives before or at the same feed step as video, so
    // we send KLV first within each "frame window".
    let events: Vec<DemuxEvent> = (0..1000)
        .flat_map(|i| [klv_events[i].clone(), video_events[i].clone()])
        .collect();

    let mut opts = PairerConfig::default();
    opts.mode = PairerMode::Realtime;
    opts.tolerance = Duration::from_millis(20);
    opts.max_buffered_klv = 64;
    opts.max_buffered_video = 64;

    c.bench_function("pairer_realtime_1000", |b| {
        b.iter(|| {
            let mut p = Pairer::with_config(VIDEO_PID, KLV_PID, opts.clone());
            for ev in &events {
                let outputs = p.feed(ev.clone());
                black_box(outputs);
            }
            let flushed = p.flush();
            black_box(flushed);
        })
    });
}

/// Buffered mode: hold video AUs for up to `max_lag` to allow KLV arriving
/// slightly late to still match. Higher pairing completeness at the cost of
/// latency. The bench verifies this path doesn't regress relative to
/// realtime.
fn bench_buffered(c: &mut Criterion) {
    let video_events: Vec<DemuxEvent> = (0..1000).map(make_video_event).collect();
    let klv_events: Vec<DemuxEvent> = (0..1000).map(make_klv_event).collect();

    // In buffered mode the pairer holds video until a within-tolerance KLV
    // arrives or the max_lag window expires. Interleave video-first to
    // exercise the buffer (video arrives, waits for KLV); the 100 ms max_lag
    // comfortably covers the 5 ms synthetic skew.
    let events: Vec<DemuxEvent> = (0..1000)
        .flat_map(|i| [video_events[i].clone(), klv_events[i].clone()])
        .collect();

    let mut opts = PairerConfig::default();
    opts.mode = PairerMode::Buffered {
        max_lag: Duration::from_millis(100),
    };
    opts.tolerance = Duration::from_millis(20);
    opts.max_buffered_klv = 64;
    opts.max_buffered_video = 64;

    c.bench_function("pairer_buffered_1000", |b| {
        b.iter(|| {
            let mut p = Pairer::with_config(VIDEO_PID, KLV_PID, opts.clone());
            for ev in &events {
                let outputs = p.feed(ev.clone());
                black_box(outputs);
            }
            let flushed = p.flush();
            black_box(flushed);
        })
    });
}

criterion_group!(benches, bench_realtime, bench_buffered);
criterion_main!(benches);
