//! `PairingDemuxer` self-validating golden round-trip.
//!
//! The oracle is the bare `Demuxer` + `Pairer` path fed the SAME TS
//! bytes. `PairingDemuxer` must produce a byte-identical `PairerOutput`
//! sequence — proving the composite is a faithful, lossless wrapper.

mod common;

use crate::common::{build_h264_au, drain_mux, dummy_klv};

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_pipeline::ext::pairing::{
    Pairer, PairerConfig, PairerMode, PairerOutput, PairingDemuxer, PairingDemuxerConfig,
};

const VIDEO_PID: u16 = 0x100;
const KLV_PID: u16 = 0x102;

/// Sync-KLV stream, 5 video frames + 5 KLV records at matching PTS.
fn sync_klv_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    for i in 0..5 {
        let pts = 90_000 + i * 3000;
        mux.push_video(&build_h264_au(0xAA), Pts90khz::new(pts), true)
            .unwrap();
        mux.push_klv(&dummy_klv(), Pts90khz::new(pts), 0x00)
            .unwrap();
    }
    drain_mux(&mut mux)
}

fn nearest_config() -> PairerConfig {
    let mut opts = PairerConfig::default();
    opts.mode = PairerMode::Realtime;
    opts.tolerance = Duration::from_millis(100);
    opts.max_buffered_klv = 16;
    opts.max_buffered_video = 16;
    opts
}

/// Oracle: bare Demuxer (default config) + the given Pairer, same bytes.
fn oracle(bytes: &[u8], mut pairer: Pairer) -> Vec<PairerOutput> {
    let mut demux = Demuxer::new();
    let mut out = Vec::new();
    demux.feed(bytes).unwrap();
    while let Some(e) = demux.next_event() {
        out.extend(pairer.feed(e));
    }
    demux.flush();
    while let Some(e) = demux.next_event() {
        out.extend(pairer.feed(e));
    }
    out.extend(pairer.flush());
    out
}

#[test]
fn with_config_matches_bare_pairer_oracle() {
    let bytes = sync_klv_bytes();

    let expected = oracle(
        &bytes,
        Pairer::with_config(VIDEO_PID, KLV_PID, nearest_config()),
    );

    let mut cfg = PairingDemuxerConfig::default();
    cfg.pairer = nearest_config();
    // cfg.demuxer keeps its Default value (default parsing settings).
    let mut pd = PairingDemuxer::with_config(VIDEO_PID, KLV_PID, cfg);
    let mut got = pd.feed(&bytes).unwrap();
    got.extend(pd.flush());

    assert_eq!(
        got, expected,
        "PairingDemuxer diverged from bare Pairer oracle"
    );
    // Sanity: the sync stream pairs all 5 frames.
    let paired = got
        .iter()
        .filter(|o| matches!(o, PairerOutput::Paired { .. }))
        .count();
    assert_eq!(paired, 5, "expected 5 Paired, got {paired}");
    assert_eq!(pd.stats().paired, 5);
}

/// Async (private-data) KLV at 1:5 cadence — exercises last_before_pts.
fn async_klv_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let video_pts: Vec<i64> = (0..15).map(|i| 90_000 + i * 3000).collect();
    let klv_pts: Vec<i64> = (0..3).map(|i| 90_000 + i * 15_000).collect();
    let (mut vi, mut ki) = (0usize, 0usize);
    while vi < video_pts.len() || ki < klv_pts.len() {
        match (video_pts.get(vi).copied(), klv_pts.get(ki).copied()) {
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
    drain_mux(&mut mux)
}

#[test]
fn last_before_pts_matches_bare_pairer_oracle() {
    let bytes = async_klv_bytes();

    let expected = oracle(&bytes, Pairer::last_before_pts(VIDEO_PID, KLV_PID, None));

    let mut pd =
        PairingDemuxer::last_before_pts(VIDEO_PID, KLV_PID, None, DemuxerConfig::default());
    let mut got = pd.feed(&bytes).unwrap();
    got.extend(pd.flush());

    assert_eq!(
        got, expected,
        "PairingDemuxer diverged from last_before oracle"
    );
    let paired = got
        .iter()
        .filter(|o| matches!(o, PairerOutput::Paired { .. }))
        .count();
    assert!(paired >= 10, "expected >=10 Paired, got {paired}");
}

#[test]
fn buffered_mode_flush_drains_via_oracle() {
    // async_klv_bytes has trailing video (PTS 123k–132k) after the last
    // KLV (120k). In Buffered mode those frames stay in the video buffer
    // through feed() and are only released by flush() — so flush() is
    // load-bearing here and the oracle equality genuinely validates it.
    let bytes = async_klv_bytes();
    let mut cfg = nearest_config();
    cfg.mode = PairerMode::Buffered {
        max_lag: Duration::from_millis(100),
    };

    let expected = oracle(&bytes, Pairer::with_config(VIDEO_PID, KLV_PID, cfg.clone()));

    let mut pdcfg = PairingDemuxerConfig::default();
    pdcfg.pairer = cfg;
    let mut pd = PairingDemuxer::with_config(VIDEO_PID, KLV_PID, pdcfg);
    let during_feed = pd.feed(&bytes).unwrap();
    let during_flush = pd.flush();

    // Guard against a vacuous test: flush() must actually release buffered
    // video, otherwise the oracle comparison wouldn't be testing the drain.
    assert!(
        !during_flush.is_empty(),
        "flush() drained nothing — buffered video was not held past feed()"
    );

    let mut got = during_feed;
    got.extend(during_flush);
    assert_eq!(got, expected, "buffered flush diverged from oracle");
}

#[test]
fn demuxer_stats_and_reset_stats() {
    let bytes = sync_klv_bytes();
    let mut pd = PairingDemuxer::new(VIDEO_PID, KLV_PID);
    let mut all = pd.feed(&bytes).unwrap();
    all.extend(pd.flush());

    // Demuxer parsed the PMT, so its program-map counter is non-zero.
    // (DemuxerStats has no packet counter; program_maps_seen is the
    // reliable "the demuxer did real work" signal for this fixture.)
    assert!(
        pd.demuxer_stats().program_maps_seen > 0,
        "demuxer_stats should reflect the parsed PMT"
    );
    // Pairing happened.
    assert!(pd.stats().paired > 0);

    // reset_stats clears the pairer counters only (not demuxer stats).
    pd.reset_stats();
    assert_eq!(
        pd.stats(),
        tst_pipeline::ext::pairing::PairerStats::default()
    );
}
