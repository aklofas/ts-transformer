//! `Muxer::reconfigure`: replacing a running muxer's program list in place,
//! checked on the wire and through the demuxer.

use crate::common::mux_demux::{collect_events, drain_mux};
use crate::common::synthetic::synthetic_klv_ls;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::mux::{
    KlvStreamHandle, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfig,
    MuxerProgramConfigBuilder, VideoCodec, VideoStreamHandle,
};
use tst_core::{MuxError, mpegts::stats::StreamCodecStats};
use tst_test_helpers::synthetic_nal::h264_au;

/// Program 1 (PMT 0x1000) = H.264 0x1011 + sync KLV 0x1031 (service id 0).
fn visible() -> MuxerProgramConfigBuilder {
    let mut p = MuxerProgramConfigBuilder::new(1, 0x1000);
    p.add_video(0x1011, VideoCodec::H264);
    p.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    p
}

/// Program 2 (PMT 0x1100) = H.264 0x1111 + sync KLV 0x1131 (service id 1).
fn thermal() -> MuxerProgramConfigBuilder {
    let mut p = MuxerProgramConfigBuilder::new(2, 0x1100);
    p.add_video(0x1111, VideoCodec::H264);
    p.add_klv(0x1131, KlvStreamType::SynchronousMetadata, true);
    p.stream_descriptors_for_klv(0, vec![tst_core::mpegts::descriptors::metadata_klva(1)])
        .unwrap();
    p
}

/// Assembled without `build()` so a refused config can reach `reconfigure`.
fn config_of(programs: Vec<MuxerProgramConfig>) -> MuxerConfig {
    let mut config = MuxerConfig::default();
    config.programs = programs;
    config
}

fn visible_thermal(with_thermal: bool) -> MuxerConfig {
    let mut programs = vec![visible().build()];
    if with_thermal {
        programs.push(thermal().build());
    }
    config_of(programs)
}

const STEP: i64 = 3600; // 40 ms at 90 kHz

fn pid_of(pkt: &[u8]) -> u16 {
    (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2])
}

fn cc_of(pkt: &[u8]) -> u8 {
    pkt[3] & 0x0F
}

fn has_payload(pkt: &[u8]) -> bool {
    pkt[3] & 0x10 != 0
}

/// PSI version of a section-start packet with no adaptation field.
fn psi_version(pkt: &[u8]) -> u8 {
    (pkt[10] >> 1) & 0x1F
}

/// `(program_number, pmt_pid)` entries of a single-packet PAT.
fn pat_programs(pkt: &[u8]) -> Vec<(u16, u16)> {
    let section_length = (usize::from(pkt[6] & 0x0F) << 8) | usize::from(pkt[7]);
    let end = 8 + section_length - 4;
    pkt[13..end]
        .chunks(4)
        .map(|e| {
            (
                u16::from_be_bytes([e[0], e[1]]),
                (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]),
            )
        })
        .collect()
}

fn packets_on(ts: &[u8], pid: u16) -> Vec<&[u8]> {
    ts.chunks(188).filter(|p| pid_of(p) == pid).collect()
}

fn versions_on(ts: &[u8], pid: u16) -> Vec<u8> {
    packets_on(ts, pid).into_iter().map(psi_version).collect()
}

fn handles(m: &Muxer, program: u16) -> (VideoStreamHandle, KlvStreamHandle) {
    (
        m.video_handles_for_program(program).unwrap()[0],
        m.klv_handles_for_program(program).unwrap()[0],
    )
}

/// One video AU and one sync KLV record per step for each `(handles,
/// service id)` target, at `n` steps starting at step `from`.
fn push_steps(
    m: &mut Muxer,
    targets: &[((VideoStreamHandle, KlvStreamHandle), u8)],
    from: i64,
    n: i64,
) {
    let klv = synthetic_klv_ls();
    for i in from..from + n {
        let pts = Pts90khz::new(i * STEP);
        for &((v, k), sid) in targets {
            m.push_video_to(v, &h264_au(200, i == from), pts, i == from)
                .unwrap();
            m.push_klv_to(k, &klv, pts, sid).unwrap();
        }
    }
}

fn discontinuities_on(events: &[DemuxEvent], pids: &[u16]) -> usize {
    events
        .iter()
        .filter(
            |e| matches!(e, DemuxEvent::Discontinuity { stream, .. } if pids.contains(&stream.pid)),
        )
        .count()
}

/// Payload-bearing packets on `pid` carry consecutive continuity counters.
fn assert_cc_continuous(ts: &[u8], pid: u16) {
    let ccs: Vec<u8> = packets_on(ts, pid)
        .into_iter()
        .filter(|p| has_payload(p))
        .map(cc_of)
        .collect();
    for w in ccs.windows(2) {
        assert_eq!(w[1], (w[0] + 1) & 0x0F, "pid 0x{pid:04X} CCs {ccs:?}");
    }
}

#[test]
fn drop_program_keeps_survivor_continuous() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let p1 = handles(&m, 1);
    let p2 = handles(&m, 2);
    push_steps(&mut m, &[(p1, 0), (p2, 1)], 0, 5);
    let pre = drain_mux(&mut m);

    m.reconfigure(visible_thermal(false)).unwrap();
    push_steps(&mut m, &[(p1, 0)], 5, 5);
    let post = drain_mux(&mut m);

    let mut d = Demuxer::new();
    d.feed(&pre).unwrap();
    let mut events = collect_events(&mut d);
    let pmt_versions_before = d.stats().pmt_versions_seen;
    d.feed(&post).unwrap();
    events.extend(collect_events(&mut d));
    assert_eq!(discontinuities_on(&events, &[0x1011, 0x1031]), 0);
    assert_eq!(d.stats().pmt_versions_seen, pmt_versions_before);

    assert!(versions_on(&pre, 0).iter().all(|&v| v == 0));
    let post_pats = packets_on(&post, 0);
    assert!(!post_pats.is_empty());
    assert!(post_pats.iter().all(|p| psi_version(p) == 1));
    assert_eq!(pat_programs(post_pats.last().unwrap()), vec![(1, 0x1000)]);
    assert!(
        versions_on(&pre, 0x1000)
            .iter()
            .chain(&versions_on(&post, 0x1000))
            .all(|&v| v == 0)
    );
    for pid in [0x1100, 0x1111, 0x1131] {
        assert!(
            packets_on(&post, pid).is_empty(),
            "pid 0x{pid:04X} after removal"
        );
    }
    let mut both = pre.clone();
    both.extend_from_slice(&post);
    assert_cc_continuous(&both, 0x1011);
    assert_cc_continuous(&both, 0x1031);
}

#[test]
fn add_program_mid_stream_is_announced_then_decoded() {
    let mut m = Muxer::new(visible_thermal(false)).unwrap();
    let p1 = handles(&m, 1);
    push_steps(&mut m, &[(p1, 0)], 0, 5);
    let pre = drain_mux(&mut m);

    m.reconfigure(visible_thermal(true)).unwrap();
    let p2 = handles(&m, 2);
    push_steps(&mut m, &[(p1, 0), (p2, 1)], 5, 5);
    let post = drain_mux(&mut m);

    let post_pats = packets_on(&post, 0);
    assert!(post_pats.iter().all(|p| psi_version(p) == 1));
    assert_eq!(pat_programs(post_pats[0]), vec![(1, 0x1000), (2, 0x1100)]);
    let first = |pid: u16| post.chunks(188).position(|p| pid_of(p) == pid).unwrap();
    assert!(first(0x1100) < first(0x1111));
    assert_eq!(versions_on(&post, 0x1100)[0], 0);
    assert!(
        versions_on(&pre, 0x1000)
            .iter()
            .chain(&versions_on(&post, 0x1000))
            .all(|&v| v == 0)
    );

    let mut d = Demuxer::new();
    d.feed(&pre).unwrap();
    let mut events = collect_events(&mut d);
    d.feed(&post).unwrap();
    events.extend(collect_events(&mut d));
    assert_eq!(discontinuities_on(&events, &[0x1011, 0x1031]), 0);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x1111))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Metadata { stream, .. } if stream.pid == 0x1131))
    );
    let mut both = pre.clone();
    both.extend_from_slice(&post);
    assert_cc_continuous(&both, 0x1011);
    assert_cc_continuous(&both, 0x1031);
}

#[test]
fn remove_stream_from_surviving_program_stops_its_pid() {
    let mut m = Muxer::new(visible_thermal(false)).unwrap();
    let p1 = handles(&m, 1);
    push_steps(&mut m, &[(p1, 0)], 0, 3);
    let pre = drain_mux(&mut m);
    assert!(!packets_on(&pre, 0x1031).is_empty());

    let mut video_only = MuxerProgramConfigBuilder::new(1, 0x1000);
    video_only.add_video(0x1011, VideoCodec::H264);
    m.reconfigure(config_of(vec![video_only.build()])).unwrap();
    let pts = Pts90khz::new(3 * STEP);
    assert!(matches!(
        m.push_klv_to(p1.1, &synthetic_klv_ls(), pts, 0),
        Err(MuxError::InvalidStreamHandle { .. })
    ));
    for i in 3..6 {
        m.push_video_to(p1.0, &h264_au(200, false), Pts90khz::new(i * STEP), false)
            .unwrap();
    }
    let post = drain_mux(&mut m);

    assert!(packets_on(&post, 0x1031).is_empty());
    let pmt = versions_on(&post, 0x1000);
    assert!(!pmt.is_empty());
    assert!(pmt.iter().all(|&v| v == 1), "{pmt:?}");
    let mut both = pre.clone();
    both.extend_from_slice(&post);
    assert_cc_continuous(&both, 0x1011);
}

#[test]
fn former_pmt_pid_reused_as_es_pid_starts_at_cc_0() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let p1 = handles(&m, 1);
    let p2 = handles(&m, 2);
    push_steps(&mut m, &[(p1, 0), (p2, 1)], 0, 10);
    let pre = drain_mux(&mut m);
    let last_pmt_cc = cc_of(packets_on(&pre, 0x1100).last().unwrap());
    assert_ne!(
        last_pmt_cc, 0x0F,
        "the PMT PID's counter must not be about to wrap to 0"
    );

    m.reconfigure(visible_thermal(false)).unwrap();
    push_steps(&mut m, &[(p1, 0)], 10, 2);
    drain_mux(&mut m);

    let mut grown = visible();
    grown.add_klv(0x1100, KlvStreamType::PrivateData, false);
    m.reconfigure(config_of(vec![grown.build()])).unwrap();
    let added = m.klv_handles_for_program(1).unwrap()[1];
    m.push_klv_to(added, &synthetic_klv_ls(), Pts90khz::new(12 * STEP), 0)
        .unwrap();
    let post = drain_mux(&mut m);
    assert_eq!(cc_of(packets_on(&post, 0x1100)[0]), 0);
}

#[test]
fn refused_reconfigure_leaves_output_identical_to_control() {
    let mut control = Muxer::new(visible_thermal(true)).unwrap();
    let mut refused = Muxer::new(visible_thermal(true)).unwrap();
    let targets = [(handles(&control, 1), 0), (handles(&control, 2), 1)];
    push_steps(&mut control, &targets, 0, 3);
    push_steps(&mut refused, &targets, 0, 3);
    let mut a = drain_mux(&mut control);
    let mut b = drain_mux(&mut refused);

    let mut changed = MuxerProgramConfigBuilder::new(1, 0x1000);
    changed.add_video(0x1011, VideoCodec::H265);
    changed.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    let err = refused
        .reconfigure(config_of(vec![changed.build(), thermal().build()]))
        .unwrap_err();
    assert!(matches!(err, MuxError::ConfigInvalid { .. }), "{err:?}");

    push_steps(&mut control, &targets, 3, 3);
    push_steps(&mut refused, &targets, 3, 3);
    a.extend(drain_mux(&mut control));
    b.extend(drain_mux(&mut refused));
    assert_eq!(a, b);
}

/// Push one AU on program 1 at step 0, drain, reconfigure to program 1
/// alone, optionally `request_psi`, push one AU at step 1 (inside the PSI
/// interval) and return what that push queued.
fn after_drop(request_psi: bool) -> Vec<u8> {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let (v, _) = handles(&m, 1);
    m.push_video_to(v, &h264_au(100, true), Pts90khz::new(0), true)
        .unwrap();
    drain_mux(&mut m);
    m.reconfigure(visible_thermal(false)).unwrap();
    if request_psi {
        m.request_psi();
    }
    m.push_video_to(v, &h264_au(100, false), Pts90khz::new(STEP), false)
        .unwrap();
    drain_mux(&mut m)
}

fn assert_new_tables_first(ts: &[u8]) {
    let pkts: Vec<&[u8]> = ts.chunks(188).collect();
    assert_eq!(pid_of(pkts[0]), 0);
    assert_eq!(psi_version(pkts[0]), 1);
    assert_eq!(pat_programs(pkts[0]), vec![(1, 0x1000)]);
    assert_eq!(pid_of(pkts[1]), 0x1000);
    assert_eq!(pid_of(pkts[2]), 0x1011);
    assert!(pkts[2..].iter().all(|p| pid_of(p) == 0x1011));
}

#[test]
fn request_psi_after_reconfigure_yields_new_tables_first() {
    assert_new_tables_first(&after_drop(true));
}

#[test]
fn reconfigure_alone_forces_psi_before_next_payload() {
    assert_new_tables_first(&after_drop(false));
}

#[test]
fn duplicate_metadata_service_id_refused_at_reconfigure() {
    // Program 2's sync KLV without its own service id collides with
    // program 1's default service id 0.
    let mut clash = MuxerProgramConfigBuilder::new(2, 0x1100);
    clash.add_video(0x1111, VideoCodec::H264);
    clash.add_klv(0x1131, KlvStreamType::SynchronousMetadata, true);
    let dup = config_of(vec![visible().build(), clash.build()]);
    let at_new = Muxer::new(dup.clone()).err().expect("new refuses");
    let mut m = Muxer::new(visible_thermal(false)).unwrap();
    let at_reconfigure = m.reconfigure(dup).unwrap_err();
    assert_eq!(at_reconfigure.to_string(), at_new.to_string());
}

#[test]
fn stream_added_to_surviving_program_bumps_only_its_pmt() {
    let mut m = Muxer::new(visible_thermal(false)).unwrap();
    let p1 = handles(&m, 1);
    push_steps(&mut m, &[(p1, 0)], 0, 3);
    let pre = drain_mux(&mut m);

    let mut grown = visible();
    grown.add_klv(0x1032, KlvStreamType::PrivateData, false);
    m.reconfigure(config_of(vec![grown.build()])).unwrap();
    let added = m.klv_handles_for_program(1).unwrap()[1];
    push_steps(&mut m, &[(p1, 0)], 3, 3);
    m.push_klv_to(added, &synthetic_klv_ls(), Pts90khz::new(6 * STEP), 0)
        .unwrap();
    let post = drain_mux(&mut m);

    assert!(versions_on(&post, 0x1000).iter().all(|&v| v == 1));
    assert!(!versions_on(&post, 0x1000).is_empty());
    assert!(versions_on(&post, 0).iter().all(|&v| v == 0));
    let mut both = pre.clone();
    both.extend_from_slice(&post);
    assert_cc_continuous(&both, 0x1011);
    assert_eq!(cc_of(packets_on(&post, 0x1032)[0]), 0);
}

#[test]
fn removed_handle_fails_until_slot_revived() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let p2 = handles(&m, 2);
    push_steps(&mut m, &[(p2, 1)], 0, 3);
    drain_mux(&mut m);

    m.reconfigure(visible_thermal(false)).unwrap();
    let pts = Pts90khz::new(3 * STEP);
    assert!(matches!(
        m.push_video_to(p2.0, &h264_au(100, true), pts, true),
        Err(MuxError::InvalidStreamHandle { .. })
    ));
    assert!(matches!(
        m.video_handles_for_program(2),
        Err(MuxError::ProgramNotFound { program_number: 2 })
    ));
    drain_mux(&mut m);

    m.reconfigure(visible_thermal(true)).unwrap();
    m.push_video_to(p2.0, &h264_au(100, true), pts, true)
        .unwrap();
    let post = drain_mux(&mut m);
    assert_eq!(cc_of(packets_on(&post, 0x1111)[0]), 0);
    let pmt = versions_on(&post, 0x1100);
    assert!(!pmt.is_empty());
    assert!(pmt.iter().all(|&v| v == 1), "{pmt:?}");
}

#[test]
fn identical_config_keeps_versions_and_reemits_psi() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let (v, _) = handles(&m, 1);
    m.push_video_to(v, &h264_au(100, true), Pts90khz::new(0), true)
        .unwrap();
    drain_mux(&mut m);
    m.reconfigure(visible_thermal(true)).unwrap();
    m.push_video_to(v, &h264_au(100, false), Pts90khz::new(STEP), false)
        .unwrap();
    let post = drain_mux(&mut m);
    let pkts: Vec<&[u8]> = post.chunks(188).collect();
    let head: Vec<(u16, u8)> = pkts[..3]
        .iter()
        .map(|p| (pid_of(p), psi_version(p)))
        .collect();
    assert_eq!(head, vec![(0, 0), (0x1000, 0), (0x1100, 0)]);
}

#[test]
fn shorthand_push_skips_tombstoned_programs() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let au = h264_au(100, true);
    assert!(matches!(
        m.push_video(&au, Pts90khz::new(0), true),
        Err(MuxError::AmbiguousTarget { .. })
    ));
    m.reconfigure(config_of(vec![thermal().build()])).unwrap();
    m.push_video(&au, Pts90khz::new(0), true).unwrap();
    let out = drain_mux(&mut m);
    assert!(!packets_on(&out, 0x1111).is_empty());
    assert!(packets_on(&out, 0x1011).is_empty());
}

#[test]
fn pmt_version_wraps_mod_32() {
    let mut m = Muxer::new(visible_thermal(false)).unwrap();
    let (v, _) = handles(&m, 1);
    let mut seen = Vec::new();
    for i in 1..=32_i64 {
        let mut p = visible();
        if i % 2 == 1 {
            p.program_descriptors(vec![vec![0xF0, 1, i as u8]]);
        }
        m.reconfigure(config_of(vec![p.build()])).unwrap();
        m.push_video_to(v, &h264_au(100, true), Pts90khz::new(i * STEP), true)
            .unwrap();
        let out = drain_mux(&mut m);
        seen.push(versions_on(&out, 0x1000)[0]);
    }
    let expected: Vec<u8> = (1..=32_u8).map(|n| n & 0x1F).collect();
    assert_eq!(seen, expected);
    assert_eq!(&seen[30..], &[31, 0]);
}

#[test]
fn forced_psi_under_buffer_full_is_consistent() {
    let small = |with_thermal: bool| {
        let mut b = MuxerConfig::builder();
        b.add_program(visible().build());
        if with_thermal {
            b.add_program(thermal().build());
        }
        b.buffer_packets(10);
        b.build().unwrap()
    };
    let mut m = Muxer::new(small(true)).unwrap();
    let (v, _) = handles(&m, 1);
    // 10 ms steps keep every push inside the PSI interval after the first.
    let mut pts = 0;
    while m.pending_packets() < 8 {
        m.push_video_to(v, &h264_au(20, pts == 0), Pts90khz::new(pts), pts == 0)
            .unwrap();
        pts += 900;
    }
    assert_eq!(m.pending_packets(), 8);
    m.reconfigure(small(false)).unwrap();
    let au = h264_au(20, false);
    assert!(matches!(
        m.push_video_to(v, &au, Pts90khz::new(pts), false),
        Err(MuxError::BufferFull { .. })
    ));
    assert_eq!(m.pending_packets(), 8);
    let before = drain_mux(&mut m);
    assert!(versions_on(&before, 0).iter().all(|&v| v == 0));
    m.push_video_to(v, &au, Pts90khz::new(pts), false).unwrap();
    let after = drain_mux(&mut m);
    let pkts: Vec<&[u8]> = after.chunks(188).collect();
    assert_eq!((pid_of(pkts[0]), psi_version(pkts[0])), (0, 1));
    assert_eq!(pid_of(pkts[1]), 0x1000);
    assert!(packets_on(&after, 0x1100).is_empty());
}

#[test]
fn stats_lifetime_and_frozen_removed_pid() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    let p1 = handles(&m, 1);
    let p2 = handles(&m, 2);
    push_steps(&mut m, &[(p1, 0), (p2, 1)], 0, 3);
    drain_mux(&mut m);
    let s1 = m.stats();
    assert_eq!(s1.programs_configured, 2);
    let frozen = s1.per_stream[&0x1111].clone();
    assert!(frozen.items > 0);

    m.reconfigure(visible_thermal(false)).unwrap();
    let s2 = m.stats();
    assert_eq!(s2.programs_configured, 1);
    assert_eq!(s2.ts_packets_emitted, s1.ts_packets_emitted);
    assert_eq!(s2.per_stream[&0x1111], frozen);
    assert_eq!(
        m.stream_codec_stats(0x1111),
        Some(StreamCodecStats::Unknown)
    );

    push_steps(&mut m, &[(p1, 0)], 3, 3);
    drain_mux(&mut m);
    let s3 = m.stats();
    assert!(s3.ts_packets_emitted > s2.ts_packets_emitted);
    assert_eq!(s3.per_stream[&0x1111], frozen);
    assert!(s3.per_stream[&0x1011].items > s1.per_stream[&0x1011].items);
}

/// Program 1 = H.264 0x1011 + KLV on each of `klv_pids` in that declaration
/// order; 0x1031 is sync KLV, every other PID async.
fn video_with_klv(klv_pids: &[u16]) -> MuxerConfig {
    let mut p = MuxerProgramConfigBuilder::new(1, 0x1000);
    p.add_video(0x1011, VideoCodec::H264);
    for &pid in klv_pids {
        let stream_type = if pid == 0x1031 {
            KlvStreamType::SynchronousMetadata
        } else {
            KlvStreamType::PrivateData
        };
        p.add_klv(pid, stream_type, true);
    }
    config_of(vec![p.build()])
}

/// The ES PIDs that carry payload in `ts`, excluding PSI and the video PID.
fn klv_pids_in(ts: &[u8]) -> Vec<u16> {
    let mut pids: Vec<u16> = ts
        .chunks(188)
        .filter(|p| has_payload(p))
        .map(pid_of)
        .filter(|&pid| pid != 0 && pid != 0x1000 && pid != 0x1011)
        .collect();
    pids.dedup();
    pids
}

#[test]
fn handles_follow_declaration_order_after_stream_reuse() {
    let mut m = Muxer::new(video_with_klv(&[0x1031, 0x1032])).unwrap();
    m.reconfigure(video_with_klv(&[0x1032, 0x1033])).unwrap();
    // Flush the forced PSI so the next drains hold only the KLV payload.
    let v = m.video_handles_for_program(1).unwrap()[0];
    m.push_video_to(v, &h264_au(200, true), Pts90khz::new(0), true)
        .unwrap();
    drain_mux(&mut m);

    let klv = m.klv_handles_for_program(1).unwrap();
    assert_eq!(klv.len(), 2);
    for (i, (&handle, expected)) in klv.iter().zip([0x1032, 0x1033]).enumerate() {
        m.push_klv_to(handle, &synthetic_klv_ls(), Pts90khz::new(STEP), 0)
            .unwrap();
        assert_eq!(klv_pids_in(&drain_mux(&mut m)), [expected], "handle {i}");
    }
    assert_eq!(m.klv_handles(), klv);
    assert_eq!(m.klv_stream_handle(0), Some(klv[0]));
    assert_eq!(m.klv_stream_handle(1), Some(klv[1]));
}

#[test]
fn first_program_handle_counts_live_streams_after_removal() {
    let mut m = Muxer::new(video_with_klv(&[0x1031, 0x1032])).unwrap();
    m.reconfigure(video_with_klv(&[0x1032])).unwrap();
    assert_eq!(m.klv_handles().len(), 1);
    let handle = m.klv_stream_handle(0).expect("one KLV stream is live");
    assert_eq!(m.klv_stream_handle(1), None);

    let v = m.video_stream_handle(0).unwrap();
    m.push_video_to(v, &h264_au(200, true), Pts90khz::new(0), true)
        .unwrap();
    drain_mux(&mut m);
    m.push_klv_to(handle, &synthetic_klv_ls(), Pts90khz::new(STEP), 0)
        .unwrap();
    assert_eq!(klv_pids_in(&drain_mux(&mut m)), [0x1032]);
}
