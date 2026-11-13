//! The muxer's program clock, read off the wire.
//!
//! PCR = decode time − `pcr_lead_ms` (default 100 ms), never decreasing
//! within a program. The reader below is written from H.222.0 §2.4.3.4
//! (adaptation field) and §2.4.3.6 (PES header), not from the crate's own
//! demuxer, so a mistake shared by the muxer and the demuxer cannot hide.

use crate::common::mux_demux::{demux_bytes_flushed, drain_mux as drain_all};
use proptest::prelude::*;
use tst_core::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, NonConformantIssue};
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec,
    VideoCodec,
};
use tst_test_helpers::synthetic_nal;

/// The default `pcr_lead_ms` (100) in 90 kHz ticks.
const LEAD: i64 = 9_000;
const MOD33: i64 = 1 << 33;

// ── A wire reader from the spec ─────────────────────────────────────────────

/// `a − b` on the 33-bit circle, in `-2^32 .. 2^32`.
fn diff33(a: u64, b: u64) -> i64 {
    let d = (a as i64 - b as i64).rem_euclid(MOD33);
    if d >= MOD33 / 2 { d - MOD33 } else { d }
}

fn m33(x: i64) -> u64 {
    x.rem_euclid(MOD33) as u64
}

fn pid(p: &[u8]) -> u16 {
    ((p[1] as u16 & 0x1F) << 8) | p[2] as u16
}

struct Pkt {
    pid: u16,
    /// Adaptation-field flags byte and 33-bit PCR base, when PCR_flag is set.
    pcr: Option<(u8, u64)>,
    /// DTS when the PES header has one, else PTS, when a PES starts here
    /// and carries a timestamp.
    decode_ts: Option<u64>,
    has_payload: bool,
}

fn ts33(b: &[u8]) -> u64 {
    ((b[0] as u64 >> 1) & 0x07) << 30
        | (b[1] as u64) << 22
        | (b[2] as u64 >> 1) << 15
        | (b[3] as u64) << 7
        | (b[4] as u64 >> 1)
}

fn parse(ts: &[u8]) -> Vec<Pkt> {
    assert_eq!(ts.len() % 188, 0);
    ts.chunks_exact(188)
        .map(|p| {
            assert_eq!(p[0], 0x47);
            let afc = (p[3] >> 4) & 0x3;
            let has_af = afc & 0b10 != 0;
            let has_payload = afc & 0b01 != 0;
            let mut off = 4;
            let mut pcr = None;
            if has_af {
                let len = p[4] as usize;
                if len > 0 && p[5] & 0x10 != 0 {
                    let base = (p[6] as u64) << 25
                        | (p[7] as u64) << 17
                        | (p[8] as u64) << 9
                        | (p[9] as u64) << 1
                        | (p[10] as u64) >> 7;
                    pcr = Some((p[5], base));
                }
                off += 1 + len;
            }
            let pusi = p[1] & 0x40 != 0;
            let mut decode_ts = None;
            if pusi && has_payload && p[off..].starts_with(&[0x00, 0x00, 0x01]) {
                let pes = &p[off..];
                match pes[7] >> 6 {
                    0b10 => decode_ts = Some(ts33(&pes[9..14])),
                    0b11 => decode_ts = Some(ts33(&pes[14..19])),
                    _ => {}
                }
            }
            Pkt {
                pid: pid(p),
                pcr,
                decode_ts,
                has_payload,
            }
        })
        .collect()
}

/// (PID, adaptation flags byte, 33-bit PCR base) of every PCR on the wire.
fn pcr_packets(ts: &[u8]) -> Vec<(u16, u8, u64)> {
    parse(ts)
        .iter()
        .filter_map(|p| p.pcr.map(|(flags, base)| (p.pid, flags, base)))
        .collect()
}

fn pcr_bases(ts: &[u8]) -> Vec<u64> {
    pcr_packets(ts).iter().map(|p| p.2).collect()
}

/// For every PES that starts with a timestamp: its PID and how far its
/// decode time (DTS, else PTS) is ahead of the latest PCR on `pcr_pid`. A
/// packet's own PCR counts for the PES it starts.
fn leads(ts: &[u8], pcr_pid: u16) -> Vec<(u16, i64)> {
    let mut last = None;
    let mut out = Vec::new();
    for p in parse(ts) {
        if p.pid == pcr_pid
            && let Some((_, base)) = p.pcr
        {
            last = Some(base);
        }
        if let (Some(d), Some(pcr)) = (p.decode_ts, last) {
            out.push((p.pid, diff33(d, pcr)));
        }
    }
    out
}

fn assert_monotonic(bases: &[u64]) {
    for w in bases.windows(2) {
        assert!(diff33(w[1], w[0]) >= 0, "PCR stepped back: {bases:?}");
    }
}

fn h264(idr: bool) -> Vec<u8> {
    synthetic_nal::h264_au(400, idr)
}

fn klv() -> Vec<u8> {
    synthetic_nal::klv_blob(32)
}

fn default_mux() -> Muxer {
    Muxer::new(MuxerConfig::default()).unwrap()
}

/// One program: H.264 0x100, AAC 0x101, synchronous KLV 0x102 (its PES
/// carries a PTS), data 0x103 with a PTS, WebVTT 0x104.
fn five_stream_config() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_audio(0x101, AudioCodec::Aac);
    prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
    prog.add_data(0x103, 0xF0, true);
    prog.add_subtitle(0x104, SubtitleCodec::WebVttInTs);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

// ── The lead ────────────────────────────────────────────────────────────────

/// H.222.0 §2.4.2.3: every byte of an access unit arrives before its decode
/// time. With a DTS the PCR leads the DTS, not the PTS.
#[test]
fn pcr_leads_explicit_dts_by_pcr_lead() {
    let mut mux = default_mux();
    let h = mux.video_stream_handle(0).unwrap();
    mux.push_video_to_with_dts(
        h,
        &h264(true),
        Pts90khz::new(200_000),
        Pts90khz::new(100_000),
        true,
    )
    .unwrap();
    let ts = drain_all(&mut mux);
    assert_eq!(pcr_bases(&ts)[0], 91_000);
    let first = ts
        .chunks_exact(188)
        .find(|p| p[3] & 0x20 != 0 && p[5] & 0x10 != 0)
        .unwrap();
    assert_eq!(&first[6..12], &[0x00, 0x00, 0xB1, 0xBC, 0x7E, 0x00]);
}

#[test]
fn pcr_leads_pts_when_no_dts() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    assert_eq!(pcr_bases(&drain_all(&mut mux))[0], 81_000);
}

/// A stream that starts at 0 keeps the full lead: the first PCR base is
/// 2^33 − 9 000, and the crate's own demuxer reads the wrap 100 ms later
/// as ordinary forward motion.
#[test]
fn pcr_wraps_below_zero_at_stream_start() {
    let mut mux = default_mux();
    for i in 0..5i64 {
        mux.push_video(&h264(i == 0), Pts90khz::new(i * 3_000), i == 0)
            .unwrap();
    }
    let ts = drain_all(&mut mux);
    let first = ts
        .chunks_exact(188)
        .find(|p| p[3] & 0x20 != 0 && p[5] & 0x10 != 0)
        .unwrap();
    assert_eq!(&first[6..12], &[0xFF, 0xFF, 0xEE, 0x6C, 0x7E, 0x00]); // base 8 589 925 592
    assert_monotonic(&pcr_bases(&ts));
    let events = demux_bytes_flushed(&ts);
    assert!(!events.iter().any(|e| matches!(
        e,
        DemuxEvent::NonConformant {
            issue: NonConformantIssue::PcrAnomaly { .. },
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, DemuxEvent::Sample { .. }))
            .count(),
        5
    );
}

/// `0` is PCR = decode time.
#[test]
fn pcr_lead_zero_is_pcr_equals_dts() {
    let mut cfg = MuxerConfig::default();
    cfg.pcr_lead_ms = 0;
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_stream_handle(0).unwrap();
    mux.push_video_to_with_dts(
        h,
        &h264(true),
        Pts90khz::new(96_000),
        Pts90khz::new(90_000),
        true,
    )
    .unwrap();
    assert_eq!(pcr_bases(&drain_all(&mut mux)), vec![90_000]);
}

#[test]
fn a_lead_of_250ms() {
    let mut cfg = MuxerConfig::default();
    cfg.pcr_lead_ms = 250;
    let mut mux = Muxer::new(cfg).unwrap();
    for i in 0..30i64 {
        mux.push_video(&h264(i == 0), Pts90khz::new(90_000 + i * 3_000), i == 0)
            .unwrap();
    }
    let ts = drain_all(&mut mux);
    assert_eq!(pcr_bases(&ts)[0], 67_500);
    assert!(leads(&ts, 0x1011).iter().all(|(_, l)| *l >= 22_500));
}

#[test]
fn two_key_frames_10ms_apart_each_carry_their_own_pcr() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_video(&h264(true), Pts90khz::new(90_900), true)
        .unwrap();
    assert_eq!(pcr_bases(&drain_all(&mut mux)), vec![81_000, 81_900]);
}

/// The lead does not change how often a PCR is sent: 30 fps against the
/// default 40 ms interval puts one on every second frame.
#[test]
fn plain_30fps_keeps_the_pcr_interval() {
    let mut mux = default_mux();
    for i in 0..30i64 {
        mux.push_video(&h264(i == 0), Pts90khz::new(90_000 + i * 3_000), i == 0)
            .unwrap();
    }
    let bases = pcr_bases(&drain_all(&mut mux));
    assert_eq!(bases[0], 81_000);
    assert_eq!(bases.len(), 15);
    assert!(bases.windows(2).all(|w| diff33(w[1], w[0]) == 6_000));
}

#[test]
fn pcr_interval_setting_does_not_change_the_lead() {
    for (interval_ms, expected_pcrs, gap) in [(1u32, 30usize, 3_000i64), (100, 10, 9_000)] {
        let mut cfg = MuxerConfig::default();
        cfg.pcr_interval_ms = interval_ms;
        let mut mux = Muxer::new(cfg).unwrap();
        for i in 0..30i64 {
            mux.push_video(&h264(i == 0), Pts90khz::new(90_000 + i * 3_000), i == 0)
                .unwrap();
        }
        let ts = drain_all(&mut mux);
        let bases = pcr_bases(&ts);
        assert_eq!(bases.len(), expected_pcrs, "interval {interval_ms}");
        assert!(bases.windows(2).all(|w| diff33(w[1], w[0]) == gap));
        // Each PCR rides the first packet of an AU and is that AU's time − lead.
        for p in parse(&ts).iter().filter(|p| p.pcr.is_some()) {
            assert_eq!(diff33(p.decode_ts.unwrap(), p.pcr.unwrap().1), LEAD);
        }
        assert!(leads(&ts, 0x1011).iter().all(|(_, l)| *l >= LEAD));
    }
}

// ── One clock per program ───────────────────────────────────────────────────

/// A key frame after another PID's push may not step the PCR back.
#[test]
fn keyframe_pcr_never_steps_back_after_klv_push() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_klv(&klv(), Pts90khz::new(99_000), 0).unwrap();
    mux.push_video(&h264(true), Pts90khz::new(93_600), true)
        .unwrap();
    assert_eq!(pcr_bases(&drain_all(&mut mux)), vec![81_000, 84_600]);
}

/// Capture-stamped KLV 150 ms ahead of encoder-delayed video: the PCR
/// neither steps back nor passes the video's decode time.
#[test]
fn leading_klv_does_not_pull_pcr_past_video_dts() {
    let mut mux = default_mux();
    for i in 0..10i64 {
        let v = 90_000 + i * 3_000;
        mux.push_video(&h264(i == 0), Pts90khz::new(v), i == 0)
            .unwrap();
        mux.push_klv(&klv(), Pts90khz::new(v + 13_500), 0).unwrap();
    }
    let bases = pcr_bases(&drain_all(&mut mux));
    assert_monotonic(&bases);
    // Every PCR is a video AU's time − 9 000; a KLV-driven one would be off
    // this grid (94 500 + k·3 000).
    assert!(
        bases
            .iter()
            .all(|b| *b >= 81_000 && (*b - 81_000) % 3_000 == 0),
        "{bases:?}"
    );
}

/// A PCR PID that never receives payload still gets PCR-only packets from
/// the other PIDs' pushes, leading their timestamps.
#[test]
fn klv_only_pushes_inject_leading_pcr_on_silent_video_pid() {
    let mut mux = default_mux();
    for i in 0..4i64 {
        mux.push_klv(&klv(), Pts90khz::new(90_000 + i * 3_600), 0)
            .unwrap();
    }
    let ts = drain_all(&mut mux);
    assert_eq!(pcr_bases(&ts), vec![81_000, 84_600, 88_200, 91_800]);
    assert!(
        parse(&ts)
            .iter()
            .filter(|p| p.pcr.is_some())
            .all(|p| p.pid == 0x1011 && !p.has_payload)
    );
}

/// A push on another PID moves the clock only when the PCR stream is silent:
/// its timestamp is more than one second past the PCR stream's last one.
#[test]
fn m2_boundary_exactly_one_second_and_just_past() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_klv(&klv(), Pts90khz::new(180_000), 0).unwrap();
    assert_eq!(
        pcr_bases(&drain_all(&mut mux)),
        vec![81_000],
        "exactly 1 s ahead"
    );

    mux.push_klv(&klv(), Pts90khz::new(180_001), 0).unwrap();
    let ts = drain_all(&mut mux);
    let pcrs: Vec<_> = parse(&ts).into_iter().filter(|p| p.pcr.is_some()).collect();
    assert_eq!(pcrs.len(), 1, "one tick past 1 s");
    assert_eq!(pcrs[0].pid, 0x1011);
    assert!(!pcrs[0].has_payload);
    assert_eq!(pcrs[0].pcr.unwrap().1, 171_001);

    // The video that follows is behind the clock now: its key frame repeats
    // the clock value and signals nothing.
    mux.push_video(&h264(true), Pts90khz::new(93_000), true)
        .unwrap();
    let pcrs = pcr_packets(&drain_all(&mut mux));
    assert_eq!(pcrs.len(), 1);
    assert_eq!(pcrs[0].2, 171_001);
    assert_eq!(pcrs[0].1 & 0x80, 0);
}

#[test]
fn video_audio_klv_share_one_monotonic_clock() {
    let mut mux = Muxer::new(five_stream_config()).unwrap();
    let (v, a, k) = (
        mux.video_handles()[0],
        mux.audio_handles()[0],
        mux.klv_handles()[0],
    );
    for i in 0..60i64 {
        let pts = Pts90khz::new(90_000 + i * 3_000);
        mux.push_video_to(v, &h264(i % 30 == 0), pts, i % 30 == 0)
            .unwrap();
        mux.push_audio_to(a, pts, &[0xA5; 200]).unwrap();
        mux.push_klv_to(k, &klv(), pts, 0).unwrap();
    }
    let ts = drain_all(&mut mux);
    let pkts = parse(&ts);
    assert_monotonic(&pcr_bases(&ts));
    assert!(
        pkts.iter()
            .filter(|p| p.pcr.is_some())
            .all(|p| p.pid == 0x100 && p.has_payload),
        "every PCR rides a video packet"
    );
    let leads = leads(&ts, 0x100);
    assert_eq!(leads.len(), 180);
    for (pid, lead) in leads {
        let floor = if pid == 0x100 { LEAD } else { 0 };
        assert!(lead >= floor, "PID {pid:#x} lead {lead}");
    }
}

/// Audio carries the PCR; a video stream beside it is judged against the
/// audio-driven clock and injects no PCR of its own.
#[test]
fn audio_pcr_pid_with_video_beside_it() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_audio(0x101, AudioCodec::Aac);
    prog.pcr_pid(0x101);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let (v, a) = (mux.video_handles()[0], mux.audio_handles()[0]);

    // One second in decode order: audio every 1 920 ticks, video every 3 000
    // with a one-frame reorder delay; audio first on a tie.
    let mut events: Vec<(i64, bool)> = (0..47).map(|j| (90_000 + j * 1_920, true)).collect();
    events.extend((0..30).map(|i| (90_000 + i * 3_000, false)));
    events.sort_by_key(|(t, is_audio)| (*t, !*is_audio));
    for (t, is_audio) in events {
        if is_audio {
            mux.push_audio_to(a, Pts90khz::new(t), &[0xA5; 200])
                .unwrap();
        } else {
            let key = t == 90_000;
            mux.push_video_to_with_dts(
                v,
                &h264(key),
                Pts90khz::new(t + 3_000),
                Pts90khz::new(t),
                key,
            )
            .unwrap();
        }
    }
    let ts = drain_all(&mut mux);
    let pkts = parse(&ts);
    let pcrs: Vec<&Pkt> = pkts.iter().filter(|p| p.pcr.is_some()).collect();
    assert!(pcrs.len() >= 20);
    for p in &pcrs {
        assert_eq!(p.pid, 0x101);
        assert!(p.has_payload, "no PCR-only packet while audio is live");
        assert_eq!(diff33(p.decode_ts.unwrap(), p.pcr.unwrap().1), LEAD);
    }
    assert_monotonic(&pcr_bases(&ts));
    let video: Vec<i64> = leads(&ts, 0x101)
        .into_iter()
        .filter(|(pid, _)| *pid == 0x100)
        .map(|(_, l)| l)
        .collect();
    assert_eq!(video.len(), 30);
    assert!(video.iter().all(|l| *l >= 0), "{video:?}");
}

// ── Reordered video ─────────────────────────────────────────────────────────

/// Twelve AUs in decode order, I P B B P B B …: (DTS, PTS). DTS is
/// monotonic; PTS zigzags.
fn b_frame_sequence() -> Vec<(i64, i64)> {
    (0..12i64)
        .map(|k| {
            let slot = match k {
                0 => 0,
                _ if (k - 1) % 3 == 0 => k + 2,
                _ => k - 1,
            };
            (90_000 + k * 3_000, 90_000 + (slot + 1) * 3_000)
        })
        .collect()
}

#[test]
fn b_frames_with_dts_keep_the_full_lead() {
    let mut mux = default_mux();
    let h = mux.video_stream_handle(0).unwrap();
    for (k, (dts, pts)) in b_frame_sequence().into_iter().enumerate() {
        mux.push_video_to_with_dts(
            h,
            &h264(k == 0),
            Pts90khz::new(pts),
            Pts90khz::new(dts),
            k == 0,
        )
        .unwrap();
    }
    let ts = drain_all(&mut mux);
    for p in parse(&ts).iter().filter(|p| p.pcr.is_some()) {
        assert_eq!(diff33(p.decode_ts.unwrap(), p.pcr.unwrap().1), LEAD);
    }
    assert_monotonic(&pcr_bases(&ts));
    assert!(leads(&ts, 0x1011).iter().all(|(_, l)| *l >= LEAD));
}

/// The same frames pushed without a DTS. The muxer cannot know the decode
/// order, so the clamp keeps the PCR from stepping back and a reordered
/// frame arrives with less than the configured lead: here 3 000 ticks, the
/// lead minus the two-frame reorder depth.
#[test]
fn b_frames_pts_only_are_clamped_and_lose_lead() {
    let mut mux = default_mux();
    for (k, (_, pts)) in b_frame_sequence().into_iter().enumerate() {
        mux.push_video(&h264(k == 0), Pts90khz::new(pts), k == 0)
            .unwrap();
    }
    let ts = drain_all(&mut mux);
    let pcrs = pcr_packets(&ts);
    assert_monotonic(&pcrs.iter().map(|p| p.2).collect::<Vec<_>>());
    assert!(pcrs.iter().all(|p| p.1 & 0x80 == 0));
    let min = leads(&ts, 0x1011)
        .into_iter()
        .map(|(_, l)| l)
        .min()
        .unwrap();
    assert_eq!(min, 3_000);
}

// ── 33-bit wrap ─────────────────────────────────────────────────────────────

fn psi_count(ts: &[u8]) -> usize {
    ts.chunks_exact(188).filter(|p| pid(p) == 0).count()
}

/// An ordinary 33-bit wrap is forward motion: no discontinuity, and PSI
/// keeps its cadence (pushes span 15 000 ticks, so PSI at 0 and 9 000).
#[test]
fn dts_wrap_is_not_a_restart() {
    let mut mux = default_mux();
    let start = MOD33 - 6_000;
    for i in 0..6i64 {
        mux.push_video(&h264(i == 0), Pts90khz::new(start + i * 3_000), i == 0)
            .unwrap();
    }
    let ts = drain_all(&mut mux);
    assert!(pcr_packets(&ts).iter().all(|p| p.1 & 0x80 == 0));
    assert_eq!(psi_count(&ts), 2);
}

#[test]
fn lead_holds_across_a_wrap_with_dts_and_pts_on_opposite_sides() {
    let mut mux = default_mux();
    let h = mux.video_stream_handle(0).unwrap();
    for i in 0..6i64 {
        let dts = MOD33 - 1_500 + i * 3_000;
        mux.push_video_to_with_dts(
            h,
            &h264(i == 0),
            Pts90khz::new(dts + 3_000),
            Pts90khz::new(dts),
            i == 0,
        )
        .unwrap();
    }
    let ts = drain_all(&mut mux);
    let pcrs = pcr_packets(&ts);
    assert_eq!(pcrs[0].2, m33(-1_500 - LEAD));
    assert!(pcrs.iter().all(|p| p.1 & 0x80 == 0));
    assert_monotonic(&pcrs.iter().map(|p| p.2).collect::<Vec<_>>());
    let leads = leads(&ts, 0x1011);
    assert_eq!(leads.len(), 6);
    assert!(leads.iter().all(|(_, l)| *l >= LEAD), "{leads:?}");
}

// ── Timeline restarts ───────────────────────────────────────────────────────

/// H.222.0 §2.4.3.5: the packet with the first PCR of a new time base has
/// discontinuity_indicator = 1. PSI is re-sent on that push so a receiver
/// acquires the restarted stream at once.
#[test]
fn restart_sets_discontinuity_once_and_reemits_psi() {
    let mut mux = default_mux();
    for i in 0..10i64 {
        mux.push_video(&h264(i == 0), Pts90khz::new(5_400_000 + i * 3_000), i == 0)
            .unwrap();
    }
    let _ = drain_all(&mut mux);
    mux.push_video(&h264(true), Pts90khz::new(0), true).unwrap();
    let ts = drain_all(&mut mux);
    let pkts: Vec<&[u8]> = ts.chunks_exact(188).collect();
    assert_eq!(pid(pkts[0]), 0x0000, "PAT first");
    assert_eq!(pid(pkts[1]), 0x1000, "PMT second");
    let pcrs = pcr_packets(&ts);
    assert_eq!(pcrs.len(), 1);
    assert_eq!(pcrs[0].1, 0x80 | 0x40 | 0x10);
    assert_eq!(pcrs[0].2, m33(-LEAD));
    mux.push_video(&h264(false), Pts90khz::new(3_000), false)
        .unwrap();
    mux.push_video(&h264(false), Pts90khz::new(6_000), false)
        .unwrap();
    assert!(
        pcr_packets(&drain_all(&mut mux))
            .iter()
            .all(|p| p.1 & 0x80 == 0)
    );
}

fn restart_twice_quickly(mux: &mut Muxer) {
    for i in 0..5i64 {
        mux.push_video(&h264(true), Pts90khz::new(900_000 + i * 3_600), true)
            .unwrap();
    }
    mux.push_video(&h264(true), Pts90khz::new(0), true).unwrap(); // restart
    mux.push_video(&h264(true), Pts90khz::new(-200_000), true)
        .unwrap(); // one PCR on the new base: too soon
}

/// H.222.0 §2.7.2: at least two PCRs between time-base discontinuities.
#[test]
fn second_restart_waits_for_two_pcrs() {
    let mut mux = default_mux();
    restart_twice_quickly(&mut mux);
    let pcrs = pcr_packets(&drain_all(&mut mux));
    assert_eq!(pcrs.iter().filter(|p| p.1 & 0x80 != 0).count(), 1);
    assert_eq!(
        pcrs[pcrs.len() - 1].2,
        m33(-LEAD),
        "the refused push repeats the clock"
    );
}

/// A restart that came too soon is not forgotten: the next push on the PCR
/// PID starts the new time base, so the clock never stays frozen behind a
/// timeline the producer has left.
#[test]
fn refused_restart_is_honoured_on_the_next_push() {
    let mut mux = default_mux();
    restart_twice_quickly(&mut mux);
    let _ = drain_all(&mut mux);
    mux.push_video(&h264(false), Pts90khz::new(-196_400), false)
        .unwrap();
    let ts = drain_all(&mut mux);
    assert_eq!(pid(&ts[..188]), 0x0000, "PSI first");
    let pcrs = pcr_packets(&ts);
    assert_eq!(pcrs.len(), 1);
    assert_eq!(pcrs[0].1, 0x80 | 0x10);
    assert_eq!(pcrs[0].2, m33(-196_400 - LEAD));
}

/// A backward step of under a second is a reorder, not a restart: the PCR
/// holds its value and nothing is signalled.
#[test]
fn small_backward_step_is_clamped_not_flagged() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_video(&h264(true), Pts90khz::new(60_000), true)
        .unwrap();
    let pcrs = pcr_packets(&drain_all(&mut mux));
    assert_eq!(
        pcrs.iter().map(|p| p.2).collect::<Vec<_>>(),
        vec![81_000, 81_000]
    );
    assert!(pcrs.iter().all(|p| p.1 & 0x80 == 0));
}

// ── Late metadata ───────────────────────────────────────────────────────────

/// Metadata should reach a receiver before its PTS. A unit pushed after the
/// PCR has passed its PTS is not dropped: it is muxed and counted.
#[test]
fn late_klv_is_sent_and_counted() {
    let mut mux = Muxer::new(five_stream_config()).unwrap();
    let k = mux.klv_handles()[0];
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap(); // PCR 81 000
    let _ = drain_all(&mut mux);
    mux.push_klv_to(k, &klv(), Pts90khz::new(36_000), 0)
        .unwrap();
    let ts = drain_all(&mut mux);
    assert!(ts.chunks_exact(188).any(|p| pid(p) == 0x102), "still sent");
    assert_eq!(mux.stats().late_metadata_pushes, 1);
    mux.push_klv_to(k, &klv(), Pts90khz::new(81_000), 0)
        .unwrap();
    assert_eq!(
        mux.stats().late_metadata_pushes,
        1,
        "a unit at the PCR is on time"
    );
}

/// The default KLV stream's PES carries no PTS, so nothing is stamped for a
/// time that could have passed.
#[test]
fn pts_less_klv_is_never_late() {
    let mut mux = default_mux();
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_klv(&klv(), Pts90khz::new(36_000), 0).unwrap();
    assert_eq!(mux.stats().late_metadata_pushes, 0);
}

#[test]
fn late_audio_data_and_subtitle_are_counted_and_reset_clears_it() {
    let mut mux = Muxer::new(five_stream_config()).unwrap();
    let (a, d, s) = (
        mux.audio_handles()[0],
        mux.data_handles()[0],
        mux.subtitle_handles()[0],
    );
    mux.push_video(&h264(true), Pts90khz::new(90_000), true)
        .unwrap();
    let late = Pts90khz::new(80_999);
    mux.push_audio_to(a, late, &[0xA5; 64]).unwrap();
    mux.push_data_to(d, &[0xA5; 64], late).unwrap();
    mux.push_subtitle_to(s, late, b"WEBVTT\n").unwrap();
    assert_eq!(mux.stats().late_metadata_pushes, 3);
    // The PCR PID's own pushes are never counted, however far back.
    mux.push_video(&h264(false), Pts90khz::new(60_000), false)
        .unwrap();
    assert_eq!(mux.stats().late_metadata_pushes, 3);
    mux.reset_stats();
    assert_eq!(mux.stats().late_metadata_pushes, 0);
}

// ── A failed push ───────────────────────────────────────────────────────────

/// `BufferFull` on any push kind leaves the clock where it was: the next
/// pushes produce the bytes of a muxer that never saw the failed one.
#[test]
fn failed_push_leaves_the_clock_unchanged() {
    for kind in 0..5 {
        let twin = || {
            let mut cfg = five_stream_config();
            cfg.buffer_packets = 10;
            let mut mux = Muxer::new(cfg).unwrap();
            mux.push_video(&h264(true), Pts90khz::new(90_000), true)
                .unwrap();
            let head = drain_all(&mut mux);
            (mux, head)
        };
        let (mut a, head_a) = twin();
        let (mut b, head_b) = twin();
        assert_eq!(head_a, head_b);

        let late = Pts90khz::new(135_000);
        let big = vec![0xA5u8; 4_000];
        let err = match kind {
            0 => a.push_video(&synthetic_nal::h264_au(4_000, true), late, true),
            1 => a.push_audio_to(a.audio_handles()[0], late, &big),
            2 => a.push_klv_to(a.klv_handles()[0], &big, late, 0),
            3 => a.push_data_to(a.data_handles()[0], &big, late),
            _ => a.push_subtitle_to(a.subtitle_handles()[0], late, &big),
        };
        assert!(
            matches!(err, Err(MuxError::BufferFull { .. })),
            "kind {kind}: {err:?}"
        );

        for mux in [&mut a, &mut b] {
            mux.push_video(&h264(false), Pts90khz::new(93_600), false)
                .unwrap();
            mux.push_klv_to(mux.klv_handles()[0], &klv(), Pts90khz::new(93_600), 0)
                .unwrap();
        }
        assert_eq!(drain_all(&mut a), drain_all(&mut b), "kind {kind}");
    }
}

// ── Property: the PCR never steps back ──────────────────────────────────────

/// Two programs: 1 = video 0x100 (PCR), audio 0x101, KLV 0x102, data 0x103;
/// 2 = video 0x200 (PCR), KLV 0x202.
fn two_program_config() -> MuxerConfig {
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x100, VideoCodec::H264);
    p1.add_audio(0x101, AudioCodec::Aac);
    p1.add_klv(0x102, KlvStreamType::PrivateData, true);
    p1.add_data(0x103, 0xF0, true);
    let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
    p2.add_video(0x200, VideoCodec::H264);
    p2.add_klv(0x202, KlvStreamType::PrivateData, true);
    let mut b = MuxerConfig::builder();
    b.add_program(p1.build()).add_program(p2.build());
    b.buffer_packets(50_000);
    b.build().unwrap()
}

#[derive(Debug, Clone)]
struct Step {
    stream: usize,
    forward: i64,
    /// A backward step of under one second instead of the forward one.
    back: Option<i64>,
    key: bool,
}

fn step_strategy() -> impl Strategy<Value = Step> {
    (
        0usize..6,
        0i64..=6_000,
        prop_oneof![9 => Just(None), 1 => (1i64..80_000).prop_map(Some)],
        any::<bool>(),
    )
        .prop_map(|(stream, forward, back, key)| Step {
            stream,
            forward,
            back,
            key,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever order the streams push in and however their timelines drift
    /// (each stream stepping forward, now and then back by under a second),
    /// each program's PCR never decreases and no discontinuity is signalled.
    #[test]
    fn pcr_is_monotonic_for_interleaved_pushes(
        steps in proptest::collection::vec(step_strategy(), 1..200)
    ) {
        let mut mux = Muxer::new(two_program_config()).unwrap();
        let (v, a, k, d) = (mux.video_handles(), mux.audio_handles(), mux.klv_handles(), mux.data_handles());
        let mut now = [90_000i64; 6];
        let body = [0xA5u8; 64];
        for s in &steps {
            now[s.stream] += s.forward;
            if let Some(back) = s.back {
                now[s.stream] -= back + s.forward;
            }
            let t = Pts90khz::new(now[s.stream]);
            let r = match s.stream {
                0 => mux.push_video_to(v[0], &h264(s.key), t, s.key),
                1 => mux.push_audio_to(a[0], t, &body),
                2 => mux.push_klv_to(k[0], &body, t, 0),
                3 => mux.push_data_to(d[0], &body, t),
                4 => mux.push_video_to(v[1], &h264(s.key), t, s.key),
                _ => mux.push_klv_to(k[1], &body, t, 0),
            };
            prop_assert!(r.is_ok(), "{r:?}");
        }
        let pcrs = pcr_packets(&drain_all(&mut mux));
        for pcr_pid in [0x100u16, 0x200] {
            let bases: Vec<u64> = pcrs.iter().filter(|p| p.0 == pcr_pid).map(|p| p.2).collect();
            for w in bases.windows(2) {
                prop_assert!(diff33(w[1], w[0]) >= 0, "PID {pcr_pid:#x}: {bases:?}");
            }
        }
        prop_assert!(pcrs.iter().all(|p| p.1 & 0x80 == 0));
        prop_assert!(pcrs.iter().all(|p| p.0 == 0x100 || p.0 == 0x200));
    }
}
