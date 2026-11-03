//! Mux → demux → pair round-trip.
//!
//! Builds a `Muxer` with one H.264 video stream + one sync-KLV stream,
//! pushes a controlled sequence of frames + KLV records, drains the
//! TS bytes, runs them through `Demuxer`, and feeds the resulting
//! `DemuxEvent`s into a `Pairer`. Asserts the integration with the
//! real demuxer (including the H.222.0 §2.12.4.2 AU cell unwrap on
//! sync-KLV, which is what the `KlvSample.kind = KlvSyncAuCell` event
//! arm exposes).

use crate::common::{build_h264_au, drain_mux, dummy_klv, to_ts_packets};

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DiscontinuityKind};
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

/// Set `discontinuity_indicator` on the `nth` (0-based) payload-unit-start
/// packet of `pid`. The muxer's one-packet test PES carries a stuffing
/// adaptation field, so the flag byte already exists.
fn flag_discontinuity_on_nth_pusi(bytes: &mut [u8], pid: u16, nth: usize) {
    let mut seen = 0;
    for pkt in bytes.chunks_exact_mut(188) {
        let pkt_pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pkt_pid != pid || pkt[1] & 0x40 == 0 {
            continue;
        }
        if seen == nth {
            assert!(
                pkt[3] & 0x20 != 0 && pkt[4] > 0,
                "target packet has no adaptation-field flags byte"
            );
            pkt[5] |= 0x80;
            return;
        }
        seen += 1;
    }
    panic!("pid {pid:#x} has no payload-unit-start packet #{nth}");
}

/// A signalled discontinuity on the video PID, through the real demuxer.
/// The demuxer reports the flag before it completes the previous unbounded
/// video PES (the flagged packet's PUSI is what ends it), so the old-clock
/// frame in flight leaves unpaired after the history is cleared.
#[test]
fn last_before_pts_clears_at_a_signalled_video_discontinuity() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    // Old clock: two frames, each preceded by its KLV.
    for pts in [90_000, 93_000] {
        mux.push_klv(&dummy_klv(), Pts90khz::new(pts), 0x00)
            .unwrap();
        mux.push_video(&build_h264_au(0xAA), Pts90khz::new(pts), true)
            .unwrap();
    }
    // New clock, later than every old-clock PTS so the old KLV would be
    // eligible if it were kept. Its first frame is the flagged packet.
    mux.push_video(&build_h264_au(0xBB), Pts90khz::new(900_000), true)
        .unwrap();
    mux.push_klv(&dummy_klv(), Pts90khz::new(900_000), 0x00)
        .unwrap();
    mux.push_video(&build_h264_au(0xBB), Pts90khz::new(903_000), true)
        .unwrap();
    let mut bytes = drain_mux(&mut mux);
    flag_discontinuity_on_nth_pusi(&mut bytes, VIDEO_PID, 2);

    let mut demux = Demuxer::new();
    let mut pairer = Pairer::last_before_pts(VIDEO_PID, KLV_PID, None);
    let mut out = Vec::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    while let Some(e) = demux.next_event() {
        out.extend(pairer.feed(e));
    }
    out.extend(pairer.flush());

    // Project to (video PTS, KLV PTS) shapes; keep only the video-PID
    // signalled discontinuity among the pass-throughs.
    let shape: Vec<String> = out
        .iter()
        .filter_map(|o| match o {
            PairerOutput::Paired { video, klv } => Some(format!(
                "paired {} {}",
                video.pts.as_ticks(),
                klv.pts.as_ticks()
            )),
            PairerOutput::UnpairedVideo(v) => Some(format!("video {}", v.pts.as_ticks())),
            PairerOutput::UnpairedKlv(k) => Some(format!("klv {}", k.pts.as_ticks())),
            PairerOutput::PassThrough(DemuxEvent::Discontinuity {
                stream,
                kind: DiscontinuityKind::AdaptationFieldFlag,
            }) if stream.pid == VIDEO_PID => Some("discontinuity".to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        shape,
        [
            "paired 90000 90000",
            "klv 93000",
            "discontinuity",
            "video 93000",
            "paired 900000 900000",
            "paired 903000 900000",
        ],
        "full output: {out:?}"
    );
}

/// A KLV local set whose PES spans two TS packets (the second carries the
/// muxer's stuffing adaptation field, so it has a flags byte to set).
fn two_packet_klv() -> Vec<u8> {
    let mut v = dummy_klv()[..16].to_vec();
    v.extend_from_slice(&[0x81, 230]); // BER long form, 230-byte body
    v.extend(std::iter::repeat_n(0x5A, 230));
    v
}

fn pid_of(pkt: &[u8]) -> u16 {
    (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2])
}

/// Index of the `nth` (0-based) payload-unit-start packet of `pid`.
fn nth_pusi_index(pkts: &[[u8; 188]], pid: u16, nth: usize) -> usize {
    pkts.iter()
        .enumerate()
        .filter(|(_, p)| pid_of(&p[..]) == pid && p[1] & 0x40 != 0)
        .nth(nth)
        .map(|(i, _)| i)
        .unwrap_or_else(|| panic!("pid {pid:#x} has no payload-unit-start packet #{nth}"))
}

fn set_discontinuity(pkt: &mut [u8; 188]) {
    assert!(
        pkt[3] & 0x20 != 0 && pkt[4] > 0,
        "target packet has no adaptation-field flags byte"
    );
    pkt[5] |= 0x80;
}

/// Mux the old clock (KLV 90 000 and its frame, the frame at 93 000, then a
/// two-packet KLV PES at 93 000) and the new clock (frames at 900 000 and
/// 903 000, and only then KLV at 906 000), so no new-clock KLV precedes the
/// first new-clock frame.
fn old_klv_in_flight_stream() -> Vec<[u8; 188]> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_klv(&dummy_klv(), Pts90khz::new(90_000), 0x00)
        .unwrap();
    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(93_000), true)
        .unwrap();
    mux.push_klv(&two_packet_klv(), Pts90khz::new(93_000), 0x00)
        .unwrap();
    mux.push_video(&build_h264_au(0xBB), Pts90khz::new(900_000), true)
        .unwrap();
    mux.push_video(&build_h264_au(0xBB), Pts90khz::new(903_000), true)
        .unwrap();
    mux.push_klv(&dummy_klv(), Pts90khz::new(906_000), 0x00)
        .unwrap();
    let pkts = to_ts_packets(&drain_mux(&mut mux));
    // The two-packet KLV PES is the second on its PID, and its packets are
    // adjacent on the KLV PID.
    let start = nth_pusi_index(&pkts, KLV_PID, 1);
    let cont = (start + 1..pkts.len())
        .find(|&i| pid_of(&pkts[i][..]) == KLV_PID)
        .unwrap();
    assert_eq!(pkts[cont][1] & 0x40, 0, "KLV PES must span two packets");
    pkts
}

fn pair_shape(pkts: &[[u8; 188]]) -> Vec<String> {
    let bytes: Vec<u8> = pkts.iter().flatten().copied().collect();
    let mut demux = Demuxer::new();
    let mut pairer = Pairer::last_before_pts(VIDEO_PID, KLV_PID, None);
    let mut out = Vec::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    while let Some(e) = demux.next_event() {
        out.extend(pairer.feed(e));
    }
    out.extend(pairer.flush());
    out.iter()
        .filter_map(|o| match o {
            PairerOutput::Paired { video, klv } => Some(format!(
                "paired {} {}",
                video.pts.as_ticks(),
                klv.pts.as_ticks()
            )),
            PairerOutput::UnpairedVideo(v) => Some(format!("video {}", v.pts.as_ticks())),
            PairerOutput::UnpairedKlv(k) => Some(format!("klv {}", k.pts.as_ticks())),
            PairerOutput::PassThrough(DemuxEvent::Discontinuity {
                stream,
                kind: DiscontinuityKind::AdaptationFieldFlag,
            }) if stream.pid == VIDEO_PID || stream.pid == KLV_PID => {
                Some("discontinuity".to_owned())
            }
            _ => None,
        })
        .collect()
}

/// The flag rides the continuation packet of a KLV PES whose header (and
/// PTS) arrived on the old clock. The PES completes after the flag; that
/// old-clock sample must not pair with a new-clock frame.
#[test]
fn last_before_pts_drops_the_old_clock_klv_completing_after_a_klv_pid_discontinuity() {
    let mut pkts = old_klv_in_flight_stream();
    let start = nth_pusi_index(&pkts, KLV_PID, 1);
    let cont = (start + 1..pkts.len())
        .find(|&i| pid_of(&pkts[i][..]) == KLV_PID)
        .unwrap();
    set_discontinuity(&mut pkts[cont]);
    assert_eq!(
        pair_shape(&pkts),
        [
            "paired 90000 90000",
            "discontinuity",
            "klv 93000",
            "video 93000",
            "video 900000",
            "video 903000",
            "klv 906000",
        ]
    );
}

/// Only the video PID signals the discontinuity, between the two packets of
/// an old-clock KLV PES. Its KLV PID carries no flag, but the sample's PTS
/// was coded on the old clock, so it must not pair with a new-clock frame.
#[test]
fn last_before_pts_drops_the_old_clock_klv_completing_after_a_video_pid_discontinuity() {
    let mut pkts = old_klv_in_flight_stream();
    let start = nth_pusi_index(&pkts, KLV_PID, 1);
    let mut flagged = pkts.remove(nth_pusi_index(&pkts, VIDEO_PID, 2));
    set_discontinuity(&mut flagged);
    // `start` precedes the removed packet, so it still names the KLV start.
    pkts.insert(start + 1, flagged);
    assert_eq!(
        pair_shape(&pkts),
        [
            "paired 90000 90000",
            "discontinuity",
            "video 93000",
            "klv 93000",
            "video 900000",
            "video 903000",
            "klv 906000",
        ]
    );
}

/// Only the KLV PID signals a backward clock restart. The video PES at
/// 903 000 is still in flight (unbounded, it ends at the next video PUSI)
/// when the flagged new-clock KLV at 0 completes, so it completes after the
/// flag with a new-clock KLV in the history. That old-clock frame must not
/// pair with it; the new-clock frames do.
#[test]
fn last_before_pts_drops_the_old_clock_video_completing_after_new_clock_klv() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, MuxVideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_klv(&dummy_klv(), Pts90khz::new(900_000), 0x00)
        .unwrap();
    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(900_000), true)
        .unwrap();
    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(903_000), true)
        .unwrap();
    mux.push_klv(&dummy_klv(), Pts90khz::new(0), 0x00).unwrap();
    for pts in [3_000, 6_000] {
        mux.push_video(&build_h264_au(0xBB), Pts90khz::new(pts), true)
            .unwrap();
    }
    let mut pkts = to_ts_packets(&drain_mux(&mut mux));
    let klv_start = nth_pusi_index(&pkts, KLV_PID, 1);
    assert!(
        klv_start < nth_pusi_index(&pkts, VIDEO_PID, 2),
        "the new-clock KLV must land before the next video PUSI"
    );
    set_discontinuity(&mut pkts[klv_start]);
    assert_eq!(
        pair_shape(&pkts),
        [
            "paired 900000 900000",
            "discontinuity",
            "video 903000",
            "paired 3000 0",
            "paired 6000 0",
        ]
    );
}
