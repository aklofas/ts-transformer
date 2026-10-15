//! Mux → demux → pair round-trip.
//!
//! Builds a `Muxer` with one H.264 video stream + one sync-KLV stream,
//! pushes a controlled sequence of frames + KLV records, drains the
//! TS bytes, runs them through `Demuxer`, and feeds the resulting
//! `DemuxEvent`s into a `Pairer`. Asserts the integration with the
//! real demuxer (including the H.222.0 §2.12.4.2 AU cell unwrap on
//! sync-KLV, which is what the `KlvSample.kind = KlvSyncAuCell` event
//! arm exposes).

use crate::common::{build_h264_au, drain_mux, dummy_klv};

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_pipeline::ext::pairing::{Pairer, PairerConfig, PairerMode, PairerOutput};

const VIDEO_PID: u16 = 0x100;
const KLV_PID: u16 = 0x102;

#[test]
fn with_config_pairs_sync_klv_with_video() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Push 5 frames + 5 KLV records at matching PTS.
    let pts_ticks: Vec<i64> = (0..5).map(|i| 90_000 + i * 3000).collect();
    for &pts in &pts_ticks {
        mux.push_video(&build_h264_au(0xAA), Pts90khz::new(pts), true)
            .unwrap();
        mux.push_klv(&dummy_klv(), Pts90khz::new(pts), 0x00)
            .unwrap();
    }
    let bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();

    let mut opts = PairerConfig::default();
    opts.mode = PairerMode::Realtime;
    opts.tolerance = Duration::from_millis(100); // 0.1 s tolerance
    opts.max_buffered_klv = 16;
    opts.max_buffered_video = 16;
    let mut pairer = Pairer::with_config(VIDEO_PID, KLV_PID, opts);
    let mut paired = 0;
    let mut unpaired_video = 0;
    let mut unpaired_klv = 0;
    while let Some(e) = demux.next_event() {
        for o in pairer.feed(e) {
            match o {
                PairerOutput::Paired { .. } => paired += 1,
                PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
                PairerOutput::PassThrough(_) => {} // PMT, etc.
            }
        }
    }
    for o in pairer.flush() {
        match o {
            PairerOutput::Paired { .. } => paired += 1,
            PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
            PairerOutput::UnpairedKlv(_) => unpaired_klv += 1,
            PairerOutput::PassThrough(_) => {}
        }
    }
    assert_eq!(
        paired, 5,
        "expected 5 Paired, got paired={paired} uv={unpaired_video} uk={unpaired_klv}"
    );
    assert_eq!(unpaired_video, 0);
    assert_eq!(unpaired_klv, 0);
}

#[test]
fn last_before_pts_pairs_async_klv_at_lower_cadence() {
    // 1:5 cadence — 1 KLV record per 5 video frames.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Interleave video and KLV in PTS order so the mux emits them with
    // correct temporal ordering. One KLV per 5 video frames at 3000-tick
    // frame interval → KLV every 15_000 ticks.
    //
    // Timeline (ticks, KLV at 0/15k/30k, video at 0..42k):
    //   PTS 90000: KLV[0], video[0..4]
    //   PTS 105000: KLV[1], video[5..9]
    //   PTS 120000: KLV[2], video[10..14]
    let video_pts: Vec<i64> = (0..15).map(|i| 90_000 + i * 3000).collect();
    let klv_pts: Vec<i64> = (0..3).map(|i| 90_000 + i * 15_000).collect();

    let mut vi = 0usize;
    let mut ki = 0usize;
    while vi < video_pts.len() || ki < klv_pts.len() {
        let next_v = video_pts.get(vi).copied();
        let next_k = klv_pts.get(ki).copied();
        match (next_v, next_k) {
            (Some(vp), Some(kp)) if kp <= vp => {
                mux.push_klv(&dummy_klv(), Pts90khz::new(kp), 0x00).unwrap();
                ki += 1;
            }
            (Some(vp), _) => {
                mux.push_video(&build_h264_au(0xAA), Pts90khz::new(vp), true)
                    .unwrap();
                vi += 1;
            }
            (None, Some(kp)) => {
                mux.push_klv(&dummy_klv(), Pts90khz::new(kp), 0x00).unwrap();
                ki += 1;
            }
            (None, None) => break,
        }
    }
    let bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();

    // freshness=None to attach regardless of staleness.
    let mut pairer = Pairer::last_before_pts(VIDEO_PID, KLV_PID, None);
    let mut paired = 0;
    let mut unpaired_video = 0;
    while let Some(e) = demux.next_event() {
        for o in pairer.feed(e) {
            match o {
                PairerOutput::Paired { .. } => paired += 1,
                PairerOutput::UnpairedVideo(_) => unpaired_video += 1,
                _ => {}
            }
        }
    }
    let _ = pairer.flush();
    // Some early frames may arrive before the first KLV (depending on
    // PES interleave); the check is loose: most should pair.
    assert!(
        paired >= 10,
        "expected ≥10 Paired, got paired={paired} uv={unpaired_video}"
    );
}
