//! Integration tests for multi-program TS muxing.
//!
//! Verifies that PAT carries N program entries and that one PMT is emitted per
//! program per PSI tick.

use crate::common::mux_demux::drain_mux as drain_all;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfig, MuxerProgramConfigBuilder, StreamSpec,
    VideoCodec,
};
use tst_test_helpers::synthetic_nal;

/// Two-program config:
///   prog 1 (H.264 + KLV) at PMT=0x1000, video=0x1011, klv=0x1031
///   prog 2 (H.265 + KLV) at PMT=0x1100, video=0x1111, klv=0x1131
///
/// All PIDs are in the valid user range 0x0010..=0x1FFE.
fn two_program_config() -> MuxerConfig {
    let mut prog1 = MuxerProgramConfig::new(1, 0x1000);
    prog1.streams = vec![
        StreamSpec::Video {
            pid: 0x1011,
            codec: VideoCodec::H264,
        },
        StreamSpec::Klv {
            pid: 0x1031,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    prog1.stream_descriptors = vec![Vec::new(), Vec::new()];
    let mut prog2 = MuxerProgramConfig::new(2, 0x1100);
    prog2.streams = vec![
        StreamSpec::Video {
            pid: 0x1111,
            codec: VideoCodec::H265,
        },
        StreamSpec::Klv {
            pid: 0x1131,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    prog2.stream_descriptors = vec![Vec::new(), Vec::new()];
    let mut cfg = MuxerConfig::default();
    cfg.programs = vec![prog1, prog2];
    cfg
}

/// Trigger PSI emission for program 0 by pushing a video frame (PTS=0 forces
/// the first-ever PSI emit regardless of interval).
fn trigger_psi(mux: &mut Muxer) -> Vec<u8> {
    let nal = synthetic_nal::h264_au(200, true);
    // Using prog 0 video stream with handle pack(0,0) via push_video_to.
    // For two-program muxers, push_video is ambiguous (2 video streams), so
    // we use the handle-based variant.
    use tst_core::mpegts::mux::VideoStreamHandle;
    let handle = VideoStreamHandle::pack(0, 0);
    mux.push_video_to(handle, &nal, Pts90khz::new(0), true)
        .unwrap();
    drain_all(mux)
}

#[test]
fn pat_carries_two_program_entries() {
    let mut muxer = Muxer::new(two_program_config()).unwrap();

    // Push a video frame on prog 0 — psi_last[0] == None so PSI is due immediately.
    let out = trigger_psi(&mut muxer);

    let pat_packet = out
        .chunks_exact(188)
        .find(|p| {
            let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
            pid == 0x0000
        })
        .expect("PAT packet must be emitted");

    // TS header = bytes 0..3 (4 bytes)
    // pointer field = byte 4 (= 0x00, so section starts at byte 5)
    // PAT section layout from byte 5:
    //   table_id(1)=5, section_syntax+length(2)=6..7,
    //   tsid(2)=8..9, ver/cni(1)=10, sect_no(1)=11, last_sect(1)=12
    //   program loop starts at byte 13
    let prog_loop_start = 13;
    let prog1_num =
        u16::from_be_bytes([pat_packet[prog_loop_start], pat_packet[prog_loop_start + 1]]);
    let prog1_pid = u16::from_be_bytes([
        pat_packet[prog_loop_start + 2] & 0x1F,
        pat_packet[prog_loop_start + 3],
    ]);
    let prog2_num = u16::from_be_bytes([
        pat_packet[prog_loop_start + 4],
        pat_packet[prog_loop_start + 5],
    ]);
    let prog2_pid = u16::from_be_bytes([
        pat_packet[prog_loop_start + 6] & 0x1F,
        pat_packet[prog_loop_start + 7],
    ]);

    assert_eq!(prog1_num, 1, "first program_number must be 1");
    assert_eq!(prog1_pid, 0x1000, "first pmt_pid must be 0x1000");
    assert_eq!(prog2_num, 2, "second program_number must be 2");
    assert_eq!(prog2_pid, 0x1100, "second pmt_pid must be 0x1100");
}

#[test]
fn both_pmts_emitted_per_psi_tick() {
    let mut muxer = Muxer::new(two_program_config()).unwrap();

    let out = trigger_psi(&mut muxer);

    let pmt1_count = out
        .chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1000)
        .count();
    let pmt2_count = out
        .chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1100)
        .count();

    assert_eq!(
        pmt1_count, 1,
        "PMT for program 1 (PID 0x1000) should be emitted once per tick"
    );
    assert_eq!(
        pmt2_count, 1,
        "PMT for program 2 (PID 0x1100) should be emitted once per tick"
    );
}

#[test]
fn pmt2_carries_correct_program_number() {
    // Verify that the PMT emitted on PID 0x2000 encodes program_number=2 in
    // its section header (bytes 8..9 of the PMT section body, which starts at
    // payload[1] = packet[5]).
    let mut muxer = Muxer::new(two_program_config()).unwrap();
    let out = trigger_psi(&mut muxer);

    let pmt2_packet = out
        .chunks_exact(188)
        .find(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1100)
        .expect("PMT for program 2 (PID 0x1100) must be emitted");

    // PMT section starts at pkt[5] (4-byte TS header + 1-byte pointer field).
    // section layout: table_id(1)=pkt[5], section_syntax+length(2)=pkt[6..7],
    //   program_number(2)=pkt[8..9]
    let program_number = u16::from_be_bytes([pmt2_packet[8], pmt2_packet[9]]);
    assert_eq!(
        program_number, 2,
        "PMT on PID 0x2000 must encode program_number=2"
    );
}

#[test]
fn single_program_pat_unchanged() {
    // Single-program config must produce the same PAT byte layout as before:
    // one program entry at bytes 13..16 of the PAT packet.
    let mut muxer = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = synthetic_nal::h264_au(200, true);
    muxer.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let out = drain_all(&mut muxer);

    let pat_packet = out
        .chunks_exact(188)
        .find(|p| ((p[1] as u16 & 0x1F) << 8) | p[2] as u16 == 0x0000)
        .expect("PAT must be emitted for single-program config");

    // Single program entry at bytes 13..16.
    let prog_num = u16::from_be_bytes([pat_packet[13], pat_packet[14]]);
    let pmt_pid = u16::from_be_bytes([pat_packet[15] & 0x1F, pat_packet[16]]);
    assert_eq!(prog_num, 1, "single-program PAT: program_number must be 1");
    assert_eq!(
        pmt_pid, 0x1000,
        "single-program PAT: pmt_pid must be 0x1000"
    );

    // Bytes after the one program entry + CRC should be 0xFF padding.
    // single program: 1 entry (4 bytes) + CRC (4 bytes) = 8 bytes after the 8-byte PSI header.
    // Padding starts at byte 13 + 8 = 21.
    assert_eq!(
        pat_packet[21], 0xFF,
        "single-program PAT must have 0xFF padding after the one entry + CRC"
    );
}

#[test]
fn config_builder_emits_multi_program_config() {
    let config = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog0.add_video(0x1011, VideoCodec::H264);
        prog0.add_klv(0x1031, KlvStreamType::PrivateData, false);
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x1100);
        prog1.add_video(0x1111, VideoCodec::H265);
        prog1.add_klv(0x1131, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build().unwrap()
    };

    assert_eq!(config.programs.len(), 2);
    assert_eq!(config.programs[0].program_number, 1);
    assert_eq!(config.programs[0].pmt_pid, 0x1000);
    assert_eq!(config.programs[0].streams.len(), 2);
    assert_eq!(config.programs[1].program_number, 2);
    assert_eq!(config.programs[1].pmt_pid, 0x1100);
}

#[test]
fn push_video_to_routes_to_correct_program_and_pid() {
    let mut muxer = Muxer::new(two_program_config()).unwrap();
    let prog1_video = muxer.video_handles_for_program(1).unwrap();
    let prog2_video = muxer.video_handles_for_program(2).unwrap();
    assert_eq!(prog1_video.len(), 1);
    assert_eq!(prog2_video.len(), 1);

    // Annex B IDR NAL — valid for both H.264 (prog 1) and H.265 (prog 2):
    // validate_annex_b only checks for the start code, not the codec.
    let nal = synthetic_nal::h264_au(64, true);
    muxer
        .push_video_to(prog1_video[0], &nal, Pts90khz::new(90_000), true)
        .unwrap();
    muxer
        .push_video_to(prog2_video[0], &nal, Pts90khz::new(90_000), true)
        .unwrap();

    let mut out = vec![0u8; 64 * 188];
    let n = muxer.pull(&mut out);
    let out = &out[..n];

    let prog1_pid_count = out
        .chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1011)
        .count();
    let prog2_pid_count = out
        .chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1111)
        .count();
    assert!(
        prog1_pid_count > 0,
        "video on program 1 PID 0x1011 must be emitted"
    );
    assert!(
        prog2_pid_count > 0,
        "video on program 2 PID 0x1111 must be emitted"
    );
}

#[test]
fn bare_push_video_returns_ambiguous_target_with_two_programs() {
    use tst_core::error::MuxError;
    let mut muxer = Muxer::new(two_program_config()).unwrap();
    let nal = synthetic_nal::h264_au(64, true);
    let err = muxer
        .push_video(&nal, Pts90khz::new(90_000), true)
        .unwrap_err();
    assert!(
        matches!(err, MuxError::AmbiguousTarget { count: 2, .. }),
        "expected AmbiguousTarget {{ count: 2, .. }}, got {err:?}"
    );
}

#[test]
fn config_builder_descriptors_for_video_attaches_to_correct_program() {
    use tst_core::mpegts::descriptors as desc;
    let config = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog0.add_video(0x1011, VideoCodec::H264);
        prog0
            .stream_descriptors_for_video(
                0,
                vec![desc::user_private(b"EO 1080p").expect("within cap")],
            )
            .unwrap();
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x1100);
        prog1.add_video(0x1111, VideoCodec::H265);
        prog1
            .stream_descriptors_for_video(
                0,
                vec![desc::user_private(b"EO 4K").expect("within cap")],
            )
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build().unwrap()
    };

    assert_eq!(
        config.programs[0].stream_descriptors[0][0],
        desc::user_private(b"EO 1080p").expect("within cap")
    );
    assert_eq!(
        config.programs[1].stream_descriptors[0][0],
        desc::user_private(b"EO 4K").expect("within cap")
    );
}

// ── PCR pacing tests ─────────────────────────────────────────────────────────

/// Return true if any 188-byte TS packet in `data` with the given PID carries
/// a PCR: adaptation field present, AF length ≥ 7, and PCR_flag set.
fn has_pcr_on_pid(data: &[u8], pid: u16) -> bool {
    data.chunks_exact(188).any(|p| {
        let p_pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        p_pid == pid
            && (p[3] & 0x20) != 0  // adaptation_field_control has AF
            && p[4] >= 7           // AF length covers at least 1 + 6 PCR bytes
            && (p[5] & 0x10) != 0 // PCR_flag set
    })
}

#[test]
fn per_program_pcr_pids_resolved_independently() {
    // Program 1 pins PCR to its video PID (0x1011) explicitly. Program 2
    // leaves pcr_pid = None, so it auto-falls back to its first video PID
    // (0x1111). We verify the actual byte output: PCR-bearing packets must
    // appear on 0x1011 for program 1 and on 0x1111 for program 2, and NOT
    // on each program's KLV streams. KLV-as-PCR is rejected at validate
    // time (ETSI TR 101 290 §5.6.1 requires ≤100 ms between PCRs).
    let mut config = two_program_config();
    config.programs[0].pcr_pid = Some(0x1011); // program 1 PCR → video PID (explicit)

    let mut muxer = Muxer::new(config).unwrap();
    let p1_video = muxer.video_handles_for_program(1).unwrap()[0];
    let p1_klv = muxer.klv_handles_for_program(1).unwrap()[0];
    let p2_video = muxer.video_handles_for_program(2).unwrap()[0];
    let p2_klv = muxer.klv_handles_for_program(2).unwrap()[0];

    let nal = synthetic_nal::h264_au(64, true);
    let klv = synthetic_nal::klv_blob(32);

    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 188];
    for tick in 0..30i64 {
        let pts = tick * 3_003;
        muxer
            .push_video_to(p1_video, &nal, Pts90khz::new(pts), tick == 0)
            .unwrap();
        muxer
            .push_klv_to(p1_klv, &klv, Pts90khz::new(pts), 0x00)
            .unwrap();
        muxer
            .push_video_to(p2_video, &nal, Pts90khz::new(pts), tick == 0)
            .unwrap();
        muxer
            .push_klv_to(p2_klv, &klv, Pts90khz::new(pts), 0x00)
            .unwrap();
        loop {
            let n = muxer.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    // Program 1: PCR must appear on the pinned video PID, not the KLV PID.
    assert!(
        has_pcr_on_pid(&out, 0x1011),
        "program 1 PCR PID 0x1011 (video, pinned) must carry PCR-bearing packets"
    );
    assert!(
        !has_pcr_on_pid(&out, 0x1031),
        "program 1 KLV PID 0x1031 must NOT carry PCR when PCR is pinned to video"
    );

    // Program 2: PCR must appear on the auto-fallback video PID.
    assert!(
        has_pcr_on_pid(&out, 0x1111),
        "program 2 PCR PID 0x1111 (video, auto-fallback) must carry PCR-bearing packets"
    );
    assert!(
        !has_pcr_on_pid(&out, 0x1131),
        "program 2 KLV PID 0x1131 must NOT carry PCR when PCR is on video"
    );
}

#[test]
fn per_program_pcr_emitted_on_each_program_pid() {
    // Default two-program config: both programs leave pcr_pid = None, so
    // each falls back to its first video PID (0x1011 and 0x1111 respectively).
    // Push enough frames (~1 second of 30 fps) to span several PCR intervals
    // (default interval is 40 ms) and confirm PCR-bearing packets land on
    // both video PIDs independently.
    let mut muxer = Muxer::new(two_program_config()).unwrap();
    let p1_video = muxer.video_handles_for_program(1).unwrap()[0];
    let p2_video = muxer.video_handles_for_program(2).unwrap()[0];

    let nal = synthetic_nal::h264_au(64, true);

    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 188];
    for tick in 0..30i64 {
        let pts = tick * 3_003; // ~33 ms per frame at 90 kHz
        muxer
            .push_video_to(p1_video, &nal, Pts90khz::new(pts), tick == 0)
            .unwrap();
        muxer
            .push_video_to(p2_video, &nal, Pts90khz::new(pts), tick == 0)
            .unwrap();
        loop {
            let n = muxer.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    assert!(
        has_pcr_on_pid(&out, 0x1011),
        "program 1 PCR PID 0x1011 must carry at least one PCR-bearing TS packet"
    );
    assert!(
        has_pcr_on_pid(&out, 0x1111),
        "program 2 PCR PID 0x1111 must carry at least one PCR-bearing TS packet"
    );
}

// ── Stats tests ───────────────────────────────────────────────────────────────

#[test]
fn muxer_stats_per_stream_carries_program_number() {
    let muxer = Muxer::new(two_program_config()).unwrap();
    let stats = muxer.stats();
    assert_eq!(stats.programs_configured, 2);

    let prog1_pids: Vec<u16> = stats
        .per_stream
        .values()
        .filter(|s| s.program_number == 1)
        .map(|s| s.pid)
        .collect();
    assert_eq!(prog1_pids.len(), 2, "program 1 should have 2 streams");
    assert!(
        prog1_pids.contains(&0x1011),
        "program 1 video PID 0x1011 missing"
    );
    assert!(
        prog1_pids.contains(&0x1031),
        "program 1 KLV PID 0x1031 missing"
    );

    let prog2_pids: Vec<u16> = stats
        .per_stream
        .values()
        .filter(|s| s.program_number == 2)
        .map(|s| s.pid)
        .collect();
    assert_eq!(prog2_pids.len(), 2, "program 2 should have 2 streams");
    assert!(
        prog2_pids.contains(&0x1111),
        "program 2 video PID 0x1111 missing"
    );
    assert!(
        prog2_pids.contains(&0x1131),
        "program 2 KLV PID 0x1131 missing"
    );
}

#[test]
fn single_program_muxer_stats_programs_configured_is_one() {
    let muxer = Muxer::new(tst_core::mpegts::mux::MuxerConfig::default()).unwrap();
    let stats = muxer.stats();
    assert_eq!(
        stats.programs_configured, 1,
        "single-program default config must report programs_configured=1"
    );
    for s in stats.per_stream.values() {
        assert_eq!(
            s.program_number, 1,
            "default-config streams must carry program_number=1"
        );
    }
}

// ── PSI multi-program backpressure ───────────────────────────────────────────

/// Build a 4-program H.264 config used by the backpressure-reservation tests
/// below. Each program has a single video stream so PSI tick = 1 PAT + 4 PMTs
/// = 5 packets — enough to discriminate a `1 + N` reservation from the
/// historical hardcoded `2`.
fn four_program_video_config() -> MuxerConfig {
    let mut b = MuxerConfig::builder();
    for i in 0..4u16 {
        let mut prog = MuxerProgramConfigBuilder::new(i + 1, 0x1000 + i * 0x100);
        prog.add_video(0x1011 + i * 0x100, VideoCodec::H264);
        b.add_program(prog.build());
    }
    b.buffer_packets(10); // minimum allowed; tight enough to surface a 1+N vs 2 reservation gap
    b.build().unwrap()
}

#[test]
fn psi_reservation_accounts_for_one_pat_plus_n_pmts() {
    // Defect: each push path reserved a hardcoded 2 PSI
    // packets, but `maybe_emit_psi` emits 1 PAT + N PMTs. With N ≥ 3 programs
    // and a tight `buffer_packets`, a push that the bug accepted on the basis
    // of "2 reserved" would actually overflow the queue past
    // `buffer_packets`. The fix reserves `1 + programs.len()` and surfaces
    // `BufferFull` BEFORE the over-emit can happen.
    //
    // Setup: 4 programs, `buffer_packets = 10`, PSI tick = 5 packets, payload
    // = 1 packet. First push uses 6 packets of capacity. Second push at the
    // next PSI interval needs 5 PSI + 1 video = 6 more (queue would land at
    // 12, but capacity is 10) → MUST be rejected as BufferFull. With the bug
    // (reservation=2), check `6 + 2 + 1 = 9 ≤ 10` passes silently and the
    // queue overflows.
    use tst_core::error::MuxError;
    use tst_core::mpegts::mux::VideoStreamHandle;

    let mut muxer = Muxer::new(four_program_video_config()).unwrap();
    let nal = synthetic_nal::h264_au(8, true); // tiny payload → 1 TS packet

    // First push on program 0 — PSI is due (first ever). 5 PSI + 1 video → queue=6.
    muxer
        .push_video_to(VideoStreamHandle::pack(0, 0), &nal, Pts90khz::new(0), true)
        .expect("first push must fit: 5 PSI + 1 video = 6 ≤ buffer_packets=10");

    // Second push on program 0 past one PSI interval (default 100 ms = 9000
    // 90 kHz ticks). PSI fires again; with the fix, reservation = 5 + 1 = 6,
    // and 6 (queue) + 6 (reservation) > 10 → BufferFull. With the bug
    // (reservation = 2), 6 + 2 + 1 = 9 ≤ 10 would pass, allowing the over-emit.
    let err = muxer
        .push_video_to(
            VideoStreamHandle::pack(0, 0),
            &nal,
            Pts90khz::new(9_000),
            false,
        )
        .expect_err("second push must surface BufferFull when 1+N PSI packets would overflow");
    assert!(
        matches!(
            err,
            MuxError::BufferFull {
                capacity_packets: 10
            }
        ),
        "expected MuxError::BufferFull {{ capacity_packets: 10 }}, got {err:?}"
    );
}

#[test]
fn psi_emit_updates_psi_last_for_all_programs() {
    // Defect: `maybe_emit_psi` only wrote
    // `self.psi_last[prog_idx] = Some(masked_pts)` for the triggering program.
    // With other programs' `psi_last[i]` still `None`, a subsequent push on
    // program `i` inside the same PSI window re-fired the entire PAT+PMTs set
    // because `psi_due(i, ..)` returns true for any None entry.
    //
    // The fix writes the masked timestamp to every entry of `self.psi_last`
    // on emit, so a push on a different program inside the interval window
    // does NOT re-fire PSI.
    //
    // Setup: 2-program config. Push on program 0 at PTS=0 → PSI fires.
    // Drain. Push on program 1 at PTS=1000 (well inside the 9000-tick PSI
    // interval). Drain again. Count PAT packets emitted on the second push;
    // with the fix it must be 0, with the bug it would be 1.
    use tst_core::mpegts::mux::VideoStreamHandle;

    let mut muxer = Muxer::new(two_program_config()).unwrap();
    let nal = synthetic_nal::h264_au(64, true);

    // First push on program 0 — fires PSI for the first time.
    muxer
        .push_video_to(VideoStreamHandle::pack(0, 0), &nal, Pts90khz::new(0), true)
        .unwrap();
    let _ = drain_all(&mut muxer);

    // Second push on program 1 (different program) at PTS well inside the
    // 9000-tick PSI interval. The fix must mark psi_last[1] = masked(0)
    // during the first push, so psi_due(1, 1000) returns false here.
    muxer
        .push_video_to(
            VideoStreamHandle::pack(1, 0),
            &nal,
            Pts90khz::new(1_000),
            true,
        )
        .unwrap();
    let out = drain_all(&mut muxer);

    let pat_count = out
        .chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x0000)
        .count();
    assert_eq!(
        pat_count, 0,
        "PSI must NOT re-fire on program 1 inside the same interval window: \
         psi_last must be updated for all programs on emit (got {pat_count} PATs)"
    );
}

// ---------------------------------------------------------------------------
// PSI on each program's own clock. The schedules below run on one time line;
// every program's stamps follow it at a fixed offset (its epoch), so a gap on
// the schedule is also a gap on the stamps of a program that kept pushing.
// ---------------------------------------------------------------------------

const MS: i64 = 90;
const SEC: i64 = 90_000;
/// 25 access units a second.
const FRAME: i64 = 40 * MS;
const E0: i64 = 20 * SEC;

fn pmt_pid_of(prog: usize) -> u16 {
    0x1000 + 0x100 * prog as u16
}

/// `n` programs of one H.264 stream each (program `i`: number `i + 1`, PMT
/// PID `0x1000 + 0x100 * i`, video PID `0x1011 + 0x100 * i`); program 0 also
/// carries synchronous KLV on `0x1031` when `klv_on_first` is set.
fn video_programs(n: usize, psi_interval_ms: u32, klv_on_first: bool) -> MuxerConfig {
    let mut b = MuxerConfig::builder();
    b.psi_interval_ms(psi_interval_ms);
    for i in 0..n {
        let mut prog = MuxerProgramConfigBuilder::new(i as u16 + 1, pmt_pid_of(i));
        prog.add_video(pmt_pid_of(i) + 0x11, VideoCodec::H264);
        if klv_on_first && i == 0 {
            prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
            prog.stream_descriptors_for_klv(
                0,
                vec![tst_core::mpegts::descriptors::metadata_klva(0)],
            )
            .unwrap();
        }
        b.add_program(prog.build());
    }
    b.build().unwrap()
}

#[derive(Clone, Copy)]
enum Act {
    Video(usize, i64),
    Klv(usize, i64),
    Tick(usize, i64),
}

/// One action at schedule time `at`; at equal times the lower `order` runs
/// first.
#[derive(Clone, Copy)]
struct Ev {
    at: i64,
    order: i64,
    act: Act,
}

/// Video pushes of `prog` at schedule times `from, from + step, …` below
/// `to`, stamped `at + epoch` modulo 2^33.
fn video_every(prog: usize, epoch: i64, step: i64, from: i64, to: i64, order: i64) -> Vec<Ev> {
    (0..)
        .map(|i| from + i * step)
        .take_while(|at| *at < to)
        .map(|at| Ev {
            at,
            order,
            act: Act::Video(prog, (at + epoch).rem_euclid(1 << 33)),
        })
        .collect()
}

/// Run the actions in schedule order; per action, its time and the bytes it
/// put out.
fn run_schedule(config: MuxerConfig, mut evs: Vec<Ev>) -> Vec<(i64, Vec<u8>)> {
    evs.sort_by_key(|e| (e.at, e.order));
    let mut mux = Muxer::new(config).unwrap();
    let mut first_video = std::collections::HashSet::new();
    evs.iter()
        .map(|e| {
            match e.act {
                Act::Video(p, ts) => {
                    let key = first_video.insert(p);
                    let h = mux.video_handles_for_program(p as u16 + 1).unwrap()[0];
                    let au = synthetic_nal::h264_au(400, key);
                    mux.push_video_to(h, &au, Pts90khz::new(ts), key).unwrap();
                }
                Act::Klv(p, ts) => {
                    let h = mux.klv_handles_for_program(p as u16 + 1).unwrap()[0];
                    let record: Vec<u8> = (0..60u8).collect();
                    mux.push_klv_to(h, &record, Pts90khz::new(ts), 0).unwrap();
                }
                Act::Tick(p, now) => mux.tick(p as u16 + 1, Pts90khz::new(now)).unwrap(),
            }
            (e.at, drain_all(&mut mux))
        })
        .collect()
}

fn carries_pid(bytes: &[u8], pid: u16) -> bool {
    bytes
        .chunks_exact(188)
        .any(|p| ((u16::from(p[1]) & 0x1F) << 8 | u16::from(p[2])) == pid)
}

/// The stretches, in milliseconds of the schedule, between two successive
/// actions that put out a packet on `pmt_pid`, and from the last of them to
/// the last action.
fn psi_gaps(out: &[(i64, Vec<u8>)], pmt_pid: u16) -> Vec<i64> {
    let at: Vec<i64> = out
        .iter()
        .filter(|(_, bytes)| carries_pid(bytes, pmt_pid))
        .map(|(t, _)| *t)
        .collect();
    let end = out.last().map_or(0, |(t, _)| *t);
    let mut gaps: Vec<i64> = at.windows(2).map(|w| (w[1] - w[0]) / MS).collect();
    gaps.extend(at.last().map(|l| (end - l) / MS));
    gaps
}

fn longest(gaps: &[i64]) -> i64 {
    gaps.iter().copied().max().unwrap_or(i64::MAX)
}

#[test]
fn a_stopped_program_ahead_does_not_stall_the_others_psi() {
    // Program 2's stamps are five seconds above program 1's; it stops at 3 s.
    let evs = [
        video_every(0, E0, FRAME, 0, 10 * SEC, 0),
        video_every(1, E0 + 5 * SEC, FRAME, 0, 3 * SEC, 1),
    ]
    .concat();
    let out = run_schedule(video_programs(2, 100, false), evs);
    let gap = longest(&psi_gaps(&out, pmt_pid_of(0)));
    assert!(gap <= 120, "program 1 went {gap} ms without its PMT");
}

#[test]
fn a_slow_program_ahead_does_not_slow_the_others_psi() {
    // Program 2 pushes at a tenth of program 1's rate, five seconds above it.
    let evs = [
        video_every(0, E0, FRAME, 0, 10 * SEC, 0),
        video_every(1, E0 + 5 * SEC, 10 * FRAME, 0, 10 * SEC, 1),
    ]
    .concat();
    let out = run_schedule(video_programs(2, 100, false), evs);
    let gap = longest(&psi_gaps(&out, pmt_pid_of(0)));
    assert!(gap <= 120, "program 1 went {gap} ms without its PMT");
}

#[test]
fn a_held_clock_on_one_program_does_not_hold_the_others_psi() {
    // Program 1, five seconds above program 2, steps back 900 ms at 3 s: its
    // clock is held (a step under a second is not a new time base).
    let evs = [
        video_every(0, E0 + 5 * SEC, FRAME, 0, 3 * SEC, 0),
        video_every(0, E0 + 5 * SEC - 900 * MS, FRAME, 3 * SEC, 10 * SEC, 0),
        video_every(1, E0, FRAME, 0, 10 * SEC, 1),
    ]
    .concat();
    let out = run_schedule(video_programs(2, 100, false), evs);
    let gap = longest(&psi_gaps(&out, pmt_pid_of(1)));
    assert!(gap <= 120, "program 2 went {gap} ms without its PMT");
}

#[test]
fn a_tick_on_any_program_keeps_psi_repeating() {
    // As the slow program ahead above, with ticks on the program BEHIND
    // between its pushes.
    let mut evs = [
        video_every(0, E0, FRAME, 0, 10 * SEC, 0),
        video_every(1, E0 + 5 * SEC, 10 * FRAME, 0, 10 * SEC, 1),
    ]
    .concat();
    evs.extend((0..250).map(|i| {
        let at = 20 * MS + i * FRAME;
        Ev {
            at,
            order: 2,
            act: Act::Tick(0, at + E0),
        }
    }));
    let out = run_schedule(video_programs(2, 100, false), evs);
    let gap = longest(&psi_gaps(&out, pmt_pid_of(0)));
    assert!(gap <= 100, "program 1 went {gap} ms without its PMT");
}

#[test]
fn a_second_program_ahead_sends_no_second_set() {
    use tst_core::mpegts::mux::VideoStreamHandle;
    let config = video_programs(2, 100, false);
    let mut mux = Muxer::new(config.clone()).unwrap();
    let au = synthetic_nal::h264_au(64, true);
    let round = |mux: &mut Muxer, at: i64| -> (bool, bool) {
        mux.push_video_to(
            VideoStreamHandle::pack(0, 0),
            &au,
            Pts90khz::new(E0 + at),
            true,
        )
        .unwrap();
        let first = carries_pid(&drain_all(mux), 0);
        // Program 2's stamps are five seconds above program 1's.
        let ahead = E0 + 5 * SEC + at;
        mux.push_video_to(
            VideoStreamHandle::pack(1, 0),
            &au,
            Pts90khz::new(ahead),
            true,
        )
        .unwrap();
        (first, carries_pid(&drain_all(mux), 0))
    };
    assert_eq!(round(&mut mux, 0), (true, false), "after Muxer::new");
    mux.request_psi();
    assert_eq!(round(&mut mux, 3_600), (true, false), "after request_psi");
    mux.reconfigure(config).unwrap();
    assert_eq!(round(&mut mux, 7_200), (true, false), "after reconfigure");
}

#[test]
fn a_second_program_pushed_after_the_first_ended_still_gets_psi() {
    // One input muxed twice, one program after the other with the same
    // stamps: all of program 1 (5 s), then all of program 2.
    let evs = [
        video_every(0, E0, FRAME, 0, 5 * SEC, 0),
        video_every(1, E0 - 5 * SEC, FRAME, 5 * SEC, 10 * SEC, 1),
    ]
    .concat();
    let out = run_schedule(video_programs(2, 100, false), evs);
    let second_half_sets = out
        .iter()
        .filter(|(at, bytes)| *at >= 5 * SEC && carries_pid(bytes, 0))
        .count();
    assert!(
        second_half_sets > 0,
        "{second_half_sets} sets in the second half"
    );
    let gap = longest(&psi_gaps(&out, pmt_pid_of(1)));
    assert!(gap <= 200, "program 2 went {gap} ms without its PMT");
}

#[test]
fn psi_gap_at_a_hand_over_is_at_most_the_interval_in_whole_pushes_plus_one_push() {
    let bound =
        |interval_ms: i64, step_ms: i64| (interval_ms + step_ms - 1) / step_ms * step_ms + step_ms;
    // Program 1 (25 units a second) stops at 3 s; program 2, on the same
    // time line, pushes every 400 ms and takes over.
    let evs = [
        video_every(0, E0, FRAME, 0, 3 * SEC, 0),
        video_every(1, E0, 10 * FRAME, 0, 10 * SEC, 1),
    ]
    .concat();
    let gap = longest(&psi_gaps(
        &run_schedule(video_programs(2, 100, false), evs),
        0x1000,
    ));
    assert!(gap <= bound(100, 400));
    assert_eq!(gap, 720);
    // The same with program 2 at one unit a second.
    let evs = [
        video_every(0, E0, FRAME, 0, 3 * SEC, 0),
        video_every(1, E0, SEC, 0, 10 * SEC, 1),
    ]
    .concat();
    let gap = longest(&psi_gaps(
        &run_schedule(video_programs(2, 100, false), evs),
        0x1000,
    ));
    assert!(gap <= bound(100, 1_000));
    assert_eq!(gap, 1_120);
    // Program 1 (video and synchronous KLV, 25 units a second, its stamps
    // 2^32 + 2 s above program 2's) stops at about 6 s; program 2 pushes at
    // one unit a second and goes first at equal times. The bound is tight.
    let epoch_1 = 4_296_949_276;
    let mut evs = video_every(0, epoch_1, FRAME, 0, 540_630, 1);
    evs.extend(
        video_every(0, epoch_1, FRAME, 0, 540_630, 11)
            .into_iter()
            .map(|e| match e.act {
                Act::Video(p, ts) => Ev {
                    act: Act::Klv(p, ts),
                    ..e
                },
                _ => e,
            }),
    );
    evs.extend(video_every(1, 1_800_000, SEC, 0, 10 * SEC, 0));
    let gap = longest(&psi_gaps(
        &run_schedule(video_programs(2, 100, true), evs),
        0x1000,
    ));
    assert!(gap <= bound(100, 1_000));
    assert_eq!(gap, 2_000);
}

#[test]
fn a_hand_over_onto_a_held_clock_waits_for_the_stamps_to_catch_up() {
    // Program 1, far ahead, stops at 1 s; at that moment program 2's stamps
    // step back by less than a second, which holds its clock, and its PSI
    // with it, until the stamps catch up.
    let gap_for = |back: i64| {
        let evs = [
            video_every(0, 900_000_000, FRAME, 0, SEC, 0),
            video_every(1, SEC + 20 * MS, FRAME, 0, SEC, 1),
            video_every(1, SEC + 20 * MS - back, FRAME, SEC, 6 * SEC, 1),
        ]
        .concat();
        longest(&psi_gaps(
            &run_schedule(video_programs(2, 100, false), evs),
            0x1000,
        ))
    };
    assert_eq!(gap_for(0), 120);
    assert_eq!(gap_for(300 * MS), 400);
    assert_eq!(gap_for(900 * MS), 1_000);
}

#[test]
fn three_programs_that_each_pass_the_interval_at_every_push_send_one_set_a_round() {
    let interval = 100 * MS;
    let run = |epochs: [i64; 3]| {
        let mut mux = Muxer::new(video_programs(3, 100, false)).unwrap();
        let au = synthetic_nal::h264_au(64, false);
        let mut rounds = Vec::new();
        for round in 0..100i64 {
            let mut senders = Vec::new();
            let mut pmts = 0;
            for (p, epoch) in epochs.iter().enumerate() {
                let h = mux.video_handles_for_program(p as u16 + 1).unwrap()[0];
                let ts = (epoch + round * interval).rem_euclid(1 << 33);
                mux.push_video_to(h, &au, Pts90khz::new(ts), false).unwrap();
                let bytes = drain_all(&mut mux);
                let pats = bytes
                    .chunks_exact(188)
                    .filter(|k| k[1] & 0x1F == 0 && k[2] == 0)
                    .count();
                if pats > 0 {
                    assert_eq!(pats, 1);
                    senders.push(p);
                    pmts += (0..3)
                        .filter(|q| carries_pid(&bytes, pmt_pid_of(*q)))
                        .count();
                }
            }
            rounds.push((senders, pmts));
        }
        rounds
    };
    for epochs in [
        [E0, E0 + 5 * SEC, E0 + 10 * SEC],
        [E0, E0 + (1 << 32), E0 + (1 << 32) + 5 * SEC],
    ] {
        let rounds = run(epochs);
        for (round, (senders, pmts)) in rounds.iter().enumerate() {
            assert_eq!(
                senders,
                &vec![0],
                "round {round}: sets sent by programs {senders:?}"
            );
            assert_eq!(*pmts, 3, "round {round}");
        }
    }
}
