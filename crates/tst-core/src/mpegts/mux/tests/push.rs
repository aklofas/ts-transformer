//! Push-path behavior tests: `push_video`, `push_klv`, `push_audio`, AU cell
//! auto-wrap, PCR emission, buffer-full atomicity, PES structure checks.

use super::*;
use crate::mpegts::common::Pts90khz;

// ── Helper ────────────────────────────────────────────────────────────────

/// Reassemble the PES payload bytes for a single PID across the TS packets
/// emitted in `buf[..n]`. Strips PES header. Used by AU cell auto-wrap tests.
fn reassemble_pes_payload_for_pid(buf: &[u8], n: usize, target_pid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    for pkt in buf[..n].chunks_exact(188) {
        let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pid != target_pid {
            continue;
        }
        let payload_unit_start = (pkt[1] & 0x40) != 0;
        let adaptation_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if adaptation_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        if payload_unit_start && idx + 9 <= 188 {
            // Standard PES: start_code(3) + stream_id(1) + length(2) +
            // flags(2) + PES_header_data_length(1) + N PTS bytes.
            let pes_header_data_length = pkt[idx + 8] as usize;
            idx += 9 + pes_header_data_length;
        }
        if idx < 188 {
            payload.extend_from_slice(&pkt[idx..188]);
        }
    }
    payload
}

// ── Pull / queue ──────────────────────────────────────────────────────────

#[test]
fn pull_returns_zero_on_empty_queue() {
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let mut buf = [0u8; 1316];
    assert_eq!(mux.pull(&mut buf), 0);
}

#[test]
fn pull_returns_zero_on_short_buffer() {
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    mux.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let mut buf = [0u8; 100];
    assert_eq!(mux.pull(&mut buf), 0);
}

// ── push_video ────────────────────────────────────────────────────────────

#[test]
fn push_video_rejects_non_annex_b() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = [0x00u8, 0x00, 0x00, 0x02]; // not an Annex-B start code
    assert!(matches!(
        m.push_video(&bad, Pts90khz::new(0), false),
        Err(MuxError::InvalidNal)
    ));
}

#[test]
fn push_video_accepts_3byte_start_code() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = [0x00u8, 0x00, 0x01, 0x67]; // 3-byte start code
    assert!(m.push_video(&nal, Pts90khz::new(0), false).is_ok());
}

#[test]
fn first_pull_includes_pat_pmt() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = [0x00u8, 0x00, 0x01, 0x67];
    m.push_video(&nal, Pts90khz::new(0), false).unwrap();
    let mut buf = [0u8; 188 * 16];
    let n = m.pull(&mut buf);
    assert!(n >= 188 * 3, "need at least PAT + PMT + video packets");
    // First packet is PAT at PID 0x0000.
    assert_eq!(buf[0], 0x47, "sync byte");
    let pat_pid = ((buf[1] as u16 & 0x1F) << 8) | buf[2] as u16;
    assert_eq!(pat_pid, 0x0000, "first packet should be PAT (PID 0)");
}

/// True if any TS packet in `buf[..n]` is a PAT (PID 0).
fn contains_pat(buf: &[u8], n: usize) -> bool {
    buf[..n].chunks_exact(188).any(|pkt| {
        let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        pid == 0x0000
    })
}

#[test]
fn request_psi_forces_pat_on_next_push() {
    // Default psi_interval_ms = 100 → 9000 ticks. Push at PTS 0 (PSI emitted
    // on the first push because slots start None), drain. A second push well
    // within the interval emits NO PAT. After request_psi(), the very next
    // push re-emits the PAT so the segment opens PAT → PMT → IDR.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = [0x00u8, 0x00, 0x01, 0x67];
    let mut buf = [0u8; 188 * 32];

    // First push: PSI due (slot None) → PAT present.
    m.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let n = m.pull(&mut buf);
    assert!(contains_pat(&buf, n), "first push must emit PAT");

    // Second push inside the interval → no PAT.
    m.push_video(&nal, Pts90khz::new(1000), false).unwrap();
    let n = m.pull(&mut buf);
    assert!(
        !contains_pat(&buf, n),
        "push inside psi_interval must NOT re-emit PAT"
    );

    // Force PSI, then push (still inside the interval) → PAT re-emitted, and
    // it precedes the video PES payload for this push.
    m.request_psi();
    m.push_video(&nal, Pts90khz::new(2000), true).unwrap();
    let n = m.pull(&mut buf);
    assert!(
        n >= 188,
        "expected packets after request_psi + push, got {n} bytes"
    );
    let first_pid = ((buf[1] as u16 & 0x1F) << 8) | buf[2] as u16;
    assert_eq!(
        first_pid, 0x0000,
        "after request_psi the next push must begin with a PAT (PID 0)"
    );
}

// ── Buffer-full atomicity ─────────────────────────────────────────────────

#[test]
fn buffer_full_returned_when_overcommitted() {
    let cfg = MuxerConfig {
        buffer_packets: 10,
        ..MuxerConfig::default()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    // A 50KB IDR is much larger than 10 packets can hold.
    let big_nal = {
        let mut v = vec![0u8; 50_000];
        v[0] = 0;
        v[1] = 0;
        v[2] = 0;
        v[3] = 1;
        v[4] = 0x65; // IDR slice NAL type
        v
    };
    let res = mux.push_video(&big_nal, Pts90khz::new(0), true);
    assert!(matches!(
        res,
        Err(MuxError::BufferFull {
            capacity_packets: 10
        })
    ));
}

#[test]
fn buffer_full_does_not_modify_state() {
    let cfg = MuxerConfig {
        buffer_packets: 10,
        ..MuxerConfig::default()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let nal = vec![0u8; 50_000];
    let nal = {
        let mut v = nal;
        v[..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        v
    };
    let _ = mux.push_video(&nal, Pts90khz::new(0), true);
    // Queue should be empty (push didn't commit).
    let mut buf = [0u8; 1316];
    assert_eq!(mux.pull(&mut buf), 0);
}

// ── PSI emission cadence ──────────────────────────────────────────────────

#[test]
fn psi_emission_survives_pts_rollover() {
    // Push a video AU just before 33-bit rollover, then another well past.
    // True modular delta is +9590 ticks (~106ms), greater than psi_interval
    // default of 9000 ticks (100ms), so PSI MUST re-emit. Buggy raw i64
    // subtraction yields a huge negative and wrongly suppresses PSI.
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x00];
    let just_before_wrap = (1i64 << 33) - 90;
    let well_past_wrap = 9_500;
    mux.push_video(&nal, Pts90khz::new(just_before_wrap), true)
        .unwrap();
    let mut buf = vec![0u8; 188 * 64];
    while mux.pull(&mut buf) > 0 {}
    mux.push_video(&nal, Pts90khz::new(well_past_wrap), false)
        .unwrap();
    let n = mux.pull(&mut buf);
    assert!(n > 0);
    // First packet should be PAT (PID 0x0000) since PSI is due.
    let first_pid = (((buf[1] as u16) & 0x1F) << 8) | buf[2] as u16;
    assert_eq!(
        first_pid, 0x0000,
        "PSI suppressed across rollover; got first PID 0x{:04X}",
        first_pid
    );
}

#[test]
fn psi_not_due_on_backward_pts() {
    // B-frame display-order: PTS may zigzag backward by a few frames. PSI
    // cadence must NOT trigger on a backward step (it would wrongly emit).
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x00];
    mux.push_video(&nal, Pts90khz::new(100_000), true).unwrap();
    let mut buf = vec![0u8; 188 * 64];
    while mux.pull(&mut buf) > 0 {}
    // Now push a backward PTS (display order earlier). Should NOT emit PSI.
    mux.push_video(&nal, Pts90khz::new(100_000 - 270), false)
        .unwrap(); // -3ms
    let n = mux.pull(&mut buf);
    assert!(n > 0);
    let first_pid = (((buf[1] as u16) & 0x1F) << 8) | buf[2] as u16;
    assert_eq!(
        first_pid, 0x1011,
        "PSI emitted on backward PTS, got first PID 0x{:04X}",
        first_pid
    );
}

#[test]
fn psi_due_after_threshold_forward() {
    // Sanity: forward by exactly psi_interval triggers PSI.
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x00];
    mux.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let mut buf = vec![0u8; 188 * 64];
    while mux.pull(&mut buf) > 0 {}
    // psi_interval default = 100ms = 9000 ticks at 90kHz.
    mux.push_video(&nal, Pts90khz::new(9_000), false).unwrap();
    let n = mux.pull(&mut buf);
    assert!(n > 0);
    // First packet should be PAT (PID 0x0000) since PSI was due.
    let first_pid = (((buf[1] as u16) & 0x1F) << 8) | buf[2] as u16;
    assert_eq!(first_pid, 0x0000, "expected PAT, got 0x{:04X}", first_pid);
}

// ── push_klv size checks ──────────────────────────────────────────────────

#[test]
fn push_klv_rejects_oversized_blob() {
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    // PES_packet_length is u16; with PTS off, max KLV payload = 65535 - 3 = 65532.
    let too_big = vec![0u8; 65_533];
    let err = mux.push_klv(&too_big, Pts90khz::new(0), 0x00).unwrap_err();
    match err {
        MuxError::KlvTooLarge { size, max } => {
            assert_eq!(size, 65_533);
            assert_eq!(max, 65_532);
        }
        other => panic!("expected MuxError::KlvTooLarge, got {:?}", other),
    }
}

#[test]
fn push_klv_accepts_largest_legal_blob() {
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    // 65532 with no PTS is the spec-imposed ceiling.
    let max_klv = vec![0xAB; 65_532];
    mux.push_klv(&max_klv, Pts90khz::new(0), 0x00)
        .expect("max-size KLV must succeed");
}

#[test]
fn push_klv_with_pts_reduces_max() {
    // With klv_carries_pts=true, header_data_length=5, so max payload =
    // 65535 - 3 - 5 = 65527.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let too_big = vec![0u8; 65_528];
    let err = mux
        .push_klv(&too_big, Pts90khz::new(90_000), 0x00)
        .unwrap_err();
    match err {
        MuxError::KlvTooLarge { size, max } => {
            assert_eq!(size, 65_528);
            assert_eq!(max, 65_527);
        }
        other => panic!("expected MuxError::KlvTooLarge, got {:?}", other),
    }
}

// ── Routed push via handle ────────────────────────────────────────────────

#[test]
fn push_video_to_routes_to_correct_pid() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_video(0x101, VideoCodec::H265);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let handles = m.video_handles();
    assert_eq!(handles.len(), 2);
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x67];
    m.push_video_to(handles[1], &nal, Pts90khz::new(0), false)
        .unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let saw_0x101 = buf[..n]
        .chunks_exact(188)
        .any(|p| p[0] == 0x47 && (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x101);
    assert!(saw_0x101, "push_video_to handle[1] must emit on PID 0x101");
}

#[test]
fn push_klv_to_routes_to_correct_pid() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let handles = m.klv_handles();
    assert_eq!(handles.len(), 2);
    let klv = [0x06u8, 0x0E, 0x2B, 0x34, 0x00];
    m.push_klv_to(handles[1], &klv, Pts90khz::new(0), 0x00)
        .unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let saw_0x102 = buf[..n]
        .chunks_exact(188)
        .any(|p| p[0] == 0x47 && (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x102);
    assert!(saw_0x102, "push_klv_to handle[1] must emit on PID 0x102");
}

// ── PES flags: data_alignment_indicator ──────────────────────────────────

#[test]
fn klv_pes_sets_data_alignment_indicator_per_h2220_v9_2_12_4_1() {
    // H.222.0 V9 §2.12.4.1 requires data_alignment_indicator=1 for
    // Synchronous KLV (stream_type 0x15) carried on PIDs with
    // registration descriptor KLVA. This test uses PrivateData (0x06)
    // which uses the same PES header path and also sets alignment.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let klv: &[u8] = &[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF,
    ];
    m.push_klv(klv, Pts90khz::new(90_000), 0x00).unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    // Find the KLV PES start packet (PID 0x101, PUSI=1).
    let pkt = buf[..n]
        .chunks_exact(188)
        .find(|p| {
            p[0] == 0x47
                && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x101
                && (p[1] & 0x40) != 0
        })
        .expect("KLV PES start packet present");
    // Locate PES payload start.
    let afc = (pkt[3] >> 4) & 0b11;
    let payload_start = if afc == 0b11 { 5 + pkt[4] as usize } else { 4 };
    let pes = &pkt[payload_start..];
    // PES flags byte 1 is at offset 6; data_alignment_indicator = bit 2.
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "PES start code");
    let flags1 = pes[6];
    assert!(
        (flags1 & 0x04) != 0,
        "data_alignment_indicator must be set for KLV (flags1 = {flags1:#04x})"
    );
}

#[test]
fn av1_pes_sets_data_alignment_indicator_per_av1_binding_3_4() {
    // AV1-in-MPEG-2-TS binding §3.4 requires data_alignment_indicator=1.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    // AV1 OBU bitstream (low-overhead); no Annex-B start code check.
    let obu = [0x12u8, 0x00]; // temporal_unit_delimiter OBU (minimal)
    m.push_video_to(
        VideoStreamHandle::pack(0, 0),
        &obu,
        Pts90khz::new(90_000),
        false,
    )
    .unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let pkt = buf[..n]
        .chunks_exact(188)
        .find(|p| {
            p[0] == 0x47
                && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x101
                && (p[1] & 0x40) != 0
        })
        .expect("AV1 PES start packet present");
    let afc = (pkt[3] >> 4) & 0b11;
    let payload_start = if afc == 0b11 { 5 + pkt[4] as usize } else { 4 };
    let pes = &pkt[payload_start..];
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "PES start code");
    let flags1 = pes[6];
    assert!(
        (flags1 & 0x04) != 0,
        "data_alignment_indicator must be set for AV1 (flags1 = {flags1:#04x})"
    );
}

#[test]
fn h264_pes_does_not_set_data_alignment_indicator() {
    // H.222.0 leaves data_alignment_indicator codec-defined for
    // H.264/H.265/H.266 — library leaves it unset.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x67, 0xBB];
    m.push_video_to(
        VideoStreamHandle::pack(0, 0),
        &nal,
        Pts90khz::new(90_000),
        false,
    )
    .unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let pkt = buf[..n]
        .chunks_exact(188)
        .find(|p| {
            p[0] == 0x47
                && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x101
                && (p[1] & 0x40) != 0
        })
        .expect("H.264 PES start packet present");
    let afc = (pkt[3] >> 4) & 0b11;
    let payload_start = if afc == 0b11 { 5 + pkt[4] as usize } else { 4 };
    let pes = &pkt[payload_start..];
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "PES start code");
    let flags1 = pes[6];
    assert!(
        (flags1 & 0x04) == 0,
        "data_alignment_indicator must NOT be set for H.264 (flags1 = {flags1:#04x})"
    );
}

// ── Invalid handle rejection ──────────────────────────────────────────────

#[test]
fn push_video_to_invalid_handle_rejects() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = VideoStreamHandle::pack(5, 3); // way out of range
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x67];
    assert!(matches!(
        m.push_video_to(bad, &nal, Pts90khz::new(0), false),
        Err(MuxError::InvalidStreamHandle {
            kind: StreamKind::Video,
            ..
        })
    ));
}

#[test]
fn push_klv_to_invalid_handle_rejects() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = KlvStreamHandle::pack(5, 3);
    let klv = [0x06u8, 0x0E];
    assert!(matches!(
        m.push_klv_to(bad, &klv, Pts90khz::new(0), 0x00),
        Err(MuxError::InvalidStreamHandle {
            kind: StreamKind::Klv,
            ..
        })
    ));
}

// ── Bare push rejections for multi-stream and no-stream configs ───────────

#[test]
fn push_video_rejects_when_multiple_video_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_video(0x101, VideoCodec::H265);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x67];
    assert!(matches!(
        m.push_video(&nal, Pts90khz::new(0), false),
        Err(MuxError::AmbiguousTarget {
            kind: StreamKind::Video,
            count: 2
        })
    ));
}

#[test]
fn push_klv_rejects_when_multiple_klv_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let klv = [0x06u8, 0x0E];
    assert!(matches!(
        m.push_klv(&klv, Pts90khz::new(0), 0x00),
        Err(MuxError::AmbiguousTarget {
            kind: StreamKind::Klv,
            count: 2
        })
    ));
}

#[test]
fn push_video_rejects_when_no_video_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_audio(0x300, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x67];
    assert!(matches!(
        m.push_video(&nal, Pts90khz::new(0), false),
        Err(MuxError::AmbiguousTarget {
            kind: StreamKind::Video,
            count: 0
        })
    ));
}

#[test]
fn push_klv_rejects_when_no_klv_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let klv = [0x06u8, 0x0E];
    let err = m.push_klv(&klv, Pts90khz::new(0), 0x00).unwrap_err();
    assert!(
        matches!(err, MuxError::NoKlvStreamsConfigured),
        "expected NoKlvStreamsConfigured, got {err:?}",
    );
}

#[test]
fn push_subtitle_without_streams_returns_no_streams_configured() {
    // Single video, no subtitles configured; push_subtitle shorthand must
    // surface NoSubtitleStreamsConfigured (was misleading AmbiguousTarget{count:0}).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let err = mux.push_subtitle(Pts90khz::new(0), &[]).unwrap_err();
    assert!(
        matches!(err, MuxError::NoSubtitleStreamsConfigured),
        "expected NoSubtitleStreamsConfigured, got {err:?}",
    );
}

#[test]
fn push_audio_without_streams_returns_no_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let err = mux.push_audio(&[], Pts90khz::new(0)).unwrap_err();
    assert!(
        matches!(err, MuxError::NoAudioStreamsConfigured),
        "expected NoAudioStreamsConfigured, got {err:?}",
    );
}

#[test]
fn push_klv_without_streams_returns_no_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let err = mux.push_klv(&[], Pts90khz::new(0), 0x00).unwrap_err();
    assert!(
        matches!(err, MuxError::NoKlvStreamsConfigured),
        "expected NoKlvStreamsConfigured, got {err:?}",
    );
}

// ── Audio push behavior ───────────────────────────────────────────────────

#[test]
fn push_audio_to_writes_pes_with_pts_only_header() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x300, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let handles = muxer.audio_handles();
    assert_eq!(handles.len(), 1);

    // Push 100 bytes of synthetic audio data with PTS = 90000 (1 second).
    let frames: Vec<u8> = (0..100).map(|i| i as u8).collect();
    muxer
        .push_audio_to(handles[0], Pts90khz::new(90_000), &frames)
        .unwrap();

    // Pull the resulting TS bytes; locate the PES start packet for PID 0x300.
    let mut buf = vec![0u8; 188 * 64];
    let n = muxer.pull(&mut buf);
    assert!(n > 0);

    // Find the audio PES packet — first TS packet for PID 0x300 with PUSI=1.
    let packet = buf[..n]
        .chunks_exact(188)
        .find(|p| {
            p[0] == 0x47
                && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x300
                && (p[1] & 0x40) != 0 // payload_unit_start_indicator
        })
        .expect("audio PES start packet present");

    // Locate the PES payload start. The adaptation_field_control bits
    // (bits 5-4 of byte 3) determine whether an adaptation field is
    // present. When set to 0b11 the adaptation field comes first, and
    // byte 4 holds its length — skip past it to reach the payload.
    let afc = (packet[3] >> 4) & 0b11;
    let payload_start = if afc == 0b11 {
        5 + packet[4] as usize // 4 (TS hdr) + 1 (af_length byte) + af_length
    } else {
        4 // payload-only (afc == 0b01): payload starts right after TS header
    };

    let pes = &packet[payload_start..];
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01], "PES start code");
    assert_eq!(pes[3], 0xC0, "stream_id = first audio (0xC0)");
    // flags2 byte at PES offset 7 — high two bits are PTS_DTS_flags
    assert_eq!(pes[7] >> 6, 0b10, "PTS only (no DTS)");
}

#[test]
fn bare_push_audio_rejects_when_two_audio_streams_configured() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.add_audio(0x301, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let err = muxer
        .push_audio(b"frame", Pts90khz::new(90_000))
        .unwrap_err();
    assert!(
        matches!(
            err,
            MuxError::AmbiguousTarget {
                kind: StreamKind::Audio,
                count: 2
            }
        ),
        "expected AmbiguousTarget {{ audio, 2 }}, got {err:?}",
    );
}

#[test]
fn audio_handles_lists_in_declaration_order() {
    let cfg = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog0.add_audio(0x300, AudioCodec::Aac);
        prog0.add_audio(0x301, AudioCodec::Mp2);
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x1100);
        prog1.add_audio(0x400, AudioCodec::Ac3);
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let handles = muxer.audio_handles();
    assert_eq!(handles.len(), 3);
    assert_eq!(handles[0].unpack(), (0, 0));
    assert_eq!(handles[1].unpack(), (0, 1));
    assert_eq!(handles[2].unpack(), (1, 0));
}

#[test]
fn audio_handles_for_program_filters_correctly() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(7, 0x1000);
        prog.add_audio(0x300, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let handles = muxer.audio_handles_for_program(7).unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].unpack(), (0, 0));

    // Unknown program number rejects.
    assert!(muxer.audio_handles_for_program(99).is_err());
}

#[test]
fn stream_descriptors_for_audio_attaches_at_build_time() {
    use crate::mpegts::descriptors::iso_639_language;
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.stream_descriptors_for_audio(0, vec![iso_639_language(*b"eng", 0)])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    // The descriptor list reaches the per-program stream_descriptors slot.
    let prog = &cfg.programs[0];
    let audio_idx = prog
        .streams
        .iter()
        .position(|s| matches!(s, StreamSpec::Audio { .. }))
        .unwrap();
    assert_eq!(prog.stream_descriptors[audio_idx].len(), 1);
}

// ── KLV AU cell auto-wrap ─────────────────────────────────────────────────

#[test]
fn sync_klv_push_auto_wraps_with_5_byte_au_cell_header() {
    use crate::mpegts::au_cell::{CellFragmentIndication, read_metadata_au_cell};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Push first sync-KLV blob — synthetic ST 0601-shaped LS.
    let mut inner_klv = Vec::new();
    inner_klv.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    inner_klv.push(0x04);
    inner_klv.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    mux.push_klv(&inner_klv, Pts90khz::new(90_000), 0x00)
        .unwrap();

    let mut buf = vec![0u8; 188 * 32];
    let n = mux.pull(&mut buf);
    let pes_payload = reassemble_pes_payload_for_pid(&buf, n, 0x1031);
    assert!(
        !pes_payload.is_empty(),
        "expected at least one TS packet on KLV PID 0x1031"
    );

    // PES payload must start with the 5-byte AU cell header followed by
    // the inner KLV bytes verbatim.
    let (hdr, body) = read_metadata_au_cell(&pes_payload).expect("valid AU cell header");
    assert_eq!(hdr.metadata_service_id, 0x00, "ST 1402.2 App. B default");
    assert_eq!(hdr.sequence_number, 0, "first push starts at seq 0");
    assert_eq!(
        hdr.cell_fragment_indication,
        CellFragmentIndication::Complete
    );
    assert!(!hdr.decoder_config_flag);
    assert!(hdr.random_access_indicator);
    assert_eq!(body, &inner_klv[..]);

    // Push second blob; sequence_number must increment.
    mux.push_klv(&inner_klv, Pts90khz::new(90_000 * 2), 0x00)
        .unwrap();
    let n2 = mux.pull(&mut buf);
    let pes2 = reassemble_pes_payload_for_pid(&buf, n2, 0x1031);
    let (hdr2, _) = read_metadata_au_cell(&pes2).expect("valid AU cell header");
    assert_eq!(
        hdr2.sequence_number, 1,
        "sequence_number must increment per push"
    );
}

#[test]
fn private_data_klv_does_not_auto_wrap() {
    // PrivateData streams must pass payload through as-is; the muxer
    // must NOT prepend an AU cell header.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let mut inner_klv = Vec::new();
    inner_klv.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    inner_klv.push(0x00);
    mux.push_klv(&inner_klv, Pts90khz::new(0), 0x00).unwrap();

    let mut buf = vec![0u8; 188 * 32];
    let n = mux.pull(&mut buf);
    let pes_payload = reassemble_pes_payload_for_pid(&buf, n, 0x1031);
    assert_eq!(
        &pes_payload[..inner_klv.len()],
        &inner_klv[..],
        "PrivateData payload must pass through unchanged"
    );
}

// ── push_video_to_with_dts (B-frame reorder) ─────────────────────────────

/// Reassemble the FULL TS payload (including PES header) for `target_pid`
/// across the TS packets in `buf[..n]`. Used to inspect PES header bytes —
/// the standard `reassemble_pes_payload_for_pid` helper strips the PES
/// header, which is needed to verify PTS_DTS_flags encoding.
fn reassemble_full_ts_payload_for_pid(buf: &[u8], n: usize, target_pid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    for pkt in buf[..n].chunks_exact(188) {
        let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pid != target_pid {
            continue;
        }
        let adaptation_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if adaptation_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        if idx < 188 {
            payload.extend_from_slice(&pkt[idx..188]);
        }
    }
    payload
}

/// Locate the PES_start packet for `target_pid` in `buf[..n]` and return
/// the parsed PES `stream_id` byte plus the PES payload bytes (after the
/// PES header) reassembled across continuation packets.
fn reassemble_pes_stream_id_and_payload(buf: &[u8], n: usize, target_pid: u16) -> (u8, Vec<u8>) {
    let mut stream_id = 0u8;
    let mut payload = Vec::new();
    let mut saw_start = false;
    for pkt in buf[..n].chunks_exact(188) {
        let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pid != target_pid {
            continue;
        }
        let payload_unit_start = (pkt[1] & 0x40) != 0;
        let adaptation_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if adaptation_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        if payload_unit_start {
            saw_start = true;
            // PES: start_code(3) + stream_id(1) + length(2) + flags(2)
            //      + PES_header_data_length(1) + N PTS bytes.
            assert_eq!(&pkt[idx..idx + 3], &[0x00, 0x00, 0x01]);
            stream_id = pkt[idx + 3];
            let pes_header_data_length = pkt[idx + 8] as usize;
            idx += 9 + pes_header_data_length;
        }
        if idx < 188 {
            payload.extend_from_slice(&pkt[idx..188]);
        }
    }
    assert!(
        saw_start,
        "no PES_start packet found for PID {target_pid:#06x}"
    );
    (stream_id, payload)
}

#[test]
fn push_video_to_with_dts_emits_pts_dts_flags_11() {
    // Reordered video (B-frames) must emit
    // PTS_DTS_flags='11' with separate PTS and DTS bytes per
    // ISO/IEC 13818-1 §2.4.3.6.
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let handle = mux.video_stream_handle(0).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
    mux.push_video_to_with_dts(
        handle,
        &nal,
        /*pts=*/ Pts90khz::new(200_000),
        /*dts=*/ Pts90khz::new(100_000),
        /*key_frame=*/ true,
    )
    .expect("push_video_to_with_dts must succeed");

    let mut buf = vec![0u8; 188 * 64];
    let n = mux.pull(&mut buf);
    // Find the video PID's PES bytes (default video PID is 0x1011).
    let pes = reassemble_full_ts_payload_for_pid(&buf, n, 0x1011);
    assert!(pes.len() >= 19, "PES must include 19-byte PTS+DTS header");
    // start_code + stream_id (video PES uses 0xE0).
    assert_eq!(&pes[0..3], &[0x00, 0x00, 0x01]);
    assert_eq!(pes[3], 0xE0);
    // flags2 byte: PTS_DTS_flags = '11' in bits 7..6.
    assert_eq!(
        (pes[7] >> 6) & 0b11,
        0b11,
        "PTS_DTS_flags must be '11' for PtsAndDts"
    );
    // PES_header_data_length = 10 (5 PTS + 5 DTS).
    assert_eq!(pes[8], 10);
    // PTS marker_high in byte 9 high nibble = 0b0011.
    assert_eq!(pes[9] >> 4, 0b0011);
    // DTS marker_high in byte 14 high nibble = 0b0001.
    assert_eq!(pes[14] >> 4, 0b0001);
    // PTS round-trip: decode the 5-byte PTS at pes[9..14].
    let decoded_pts = read_5byte_pts(&pes[9..14]);
    assert_eq!(decoded_pts, 200_000);
    let decoded_dts = read_5byte_pts(&pes[14..19]);
    assert_eq!(decoded_dts, 100_000);
}

#[test]
fn push_video_to_keeps_pts_only_encoding() {
    // Regression guard: the PTS-only path must remain unchanged after
    // adding PtsAndDts (the variant addition is additive).
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let handle = mux.video_stream_handle(0).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAA];
    mux.push_video_to(handle, &nal, Pts90khz::new(45_000), true)
        .unwrap();
    let mut buf = vec![0u8; 188 * 16];
    let n = mux.pull(&mut buf);
    let pes = reassemble_full_ts_payload_for_pid(&buf, n, 0x1011);
    assert!(pes.len() >= 14);
    // PTS_DTS_flags = '10' (PTS only).
    assert_eq!((pes[7] >> 6) & 0b11, 0b10);
    // PES_header_data_length = 5 (PTS only).
    assert_eq!(pes[8], 5);
}

/// Decode a 5-byte ISO/IEC 13818-1 §2.4.3.6 PTS field. Test-only helper.
fn read_5byte_pts(buf: &[u8]) -> u64 {
    let b0 = buf[0] as u64;
    let b1 = buf[1] as u64;
    let b2 = buf[2] as u64;
    let b3 = buf[3] as u64;
    let b4 = buf[4] as u64;
    (((b0 >> 1) & 0x07) << 30)
        | (b1 << 22)
        | (((b2 >> 1) & 0x7F) << 15)
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

// ── structural Annex-B validation ────────────────────────────────────────

#[test]
fn push_video_rejects_start_code_only_no_nal_body() {
    // An input consisting solely of a start code with no NAL body must
    // be rejected.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = [0x00u8, 0x00, 0x00, 0x01]; // just the start code, no NAL byte
    assert!(matches!(
        m.push_video(&bad, Pts90khz::new(0), false),
        Err(MuxError::InvalidNal)
    ));
}

#[test]
fn push_video_rejects_3byte_start_code_only() {
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = [0x00u8, 0x00, 0x01]; // 3-byte start code with no body
    assert!(matches!(
        m.push_video(&bad, Pts90khz::new(0), false),
        Err(MuxError::InvalidNal)
    ));
}

#[test]
fn push_video_rejects_adjacent_start_codes_no_body_between() {
    // Two start codes with no NAL bytes between them
    // (empty NAL unit) — forbidden by H.264/H.265/H.266.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = [
        0x00u8, 0x00, 0x00, 0x01, // first start code
        0x00, 0x00, 0x00, 0x01, // immediately another start code (zero body)
        0x09, // body of second NAL
    ];
    assert!(matches!(
        m.push_video(&bad, Pts90khz::new(0), false),
        Err(MuxError::InvalidNal)
    ));
}

#[test]
fn push_video_rejects_trailing_start_code() {
    // The buffer ends with a start code (no body after) —
    // the final NAL would be empty.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let bad = [
        0x00u8, 0x00, 0x00, 0x01, 0x09, 0xAA, // first NAL has a body
        0x00, 0x00, 0x00, 0x01, // trailing start code, no NAL body
    ];
    assert!(matches!(
        m.push_video(&bad, Pts90khz::new(0), false),
        Err(MuxError::InvalidNal)
    ));
}

#[test]
fn push_video_accepts_multi_nal_au() {
    // A well-formed multi-NAL AU must pass.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let good = [
        0x00u8, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1F, // SPS NAL
        0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // PPS NAL
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, // IDR NAL
    ];
    assert!(m.push_video(&good, Pts90khz::new(0), true).is_ok());
}

#[test]
fn push_video_accepts_single_nal_unchanged() {
    // Backward compat: the simple single-NAL case must still pass.
    let mut m = Muxer::new(MuxerConfig::default()).unwrap();
    let nal = [0x00u8, 0x00, 0x01, 0x67]; // 3-byte start code + NAL body
    assert!(m.push_video(&nal, Pts90khz::new(0), false).is_ok());
}

// ─────────────────────────────────────────────────────────────────────────
// AV1-in-MPEG-2-TS binding-conformant carriage
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn av1_default_uses_binding_mode_with_stream_id_bd_and_obu_framing() {
    // Default AV1 carriage MUST be binding-conformant per the binding
    // §3.4 (stream_id=0xBD) + §3.2 (ts_open_bitstream_unit framing).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    // No carriage call — default must be Mpeg2TsBinding.
    assert_eq!(cfg.av1_carriage, Av1CarriageMode::Mpeg2TsBinding);

    let mut m = Muxer::new(cfg).unwrap();
    // Two OBUs: temporal_unit_delimiter (type 2) + a sequence-header-like
    // OBU (type 1) with no body. The exact OBU types don't matter — we
    // only care about the wire framing.
    let obu_payload = vec![0x12u8, 0x00, 0x0A, 0x00];
    m.push_video_to(
        VideoStreamHandle::pack(0, 0),
        &obu_payload,
        Pts90khz::new(90_000),
        true,
    )
    .unwrap();

    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let (stream_id, pes_payload) = reassemble_pes_stream_id_and_payload(&buf, n, 0x101);

    // §3.4 — PES stream_id MUST be 0xBD (private_stream_1).
    assert_eq!(stream_id, 0xBD, "binding §3.4 mandates stream_id=0xBD");
    // §3.2 — PES payload MUST begin with the 3-byte ts_open_bitstream_unit
    // start code 0x00 0x00 0x01 (obu_start_code is uimsbf(24) = 0x000001
    // per the binding syntax table). The production is applied ONCE PER
    // OBU, so 2 OBUs → 2 start codes.
    assert_eq!(
        &pes_payload[..3],
        &[0x00, 0x00, 0x01],
        "binding §3.2 mandates ts_open_bitstream_unit start code 0x000001"
    );
    // Expected on-wire layout: start code + OBU1 + start code + OBU2.
    // Neither OBU body needs emulation-prevention escapes.
    let expected: &[u8] = &[
        0x00, 0x00, 0x01, // start code 1
        0x12, 0x00, // OBU1: TD header + size=0
        0x00, 0x00, 0x01, // start code 2
        0x0A, 0x00, // OBU2: SeqHeader-like header + size=0
    ];
    assert_eq!(
        &pes_payload[..expected.len()],
        expected,
        "binding §3.2 emits one start code per OBU"
    );
}

#[test]
fn av1_interop_mode_preserves_existing_carriage() {
    // InteropRawObu mode preserves stream_id=0xE0 + raw OBU payload, for
    // ffmpeg / libaom / hls.js loopback. The escape hatch when binding
    // carriage breaks interop.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(Av1CarriageMode::InteropRawObu);
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let obu_payload = vec![0x12u8, 0x00];
    m.push_video_to(
        VideoStreamHandle::pack(0, 0),
        &obu_payload,
        Pts90khz::new(90_000),
        false,
    )
    .unwrap();

    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let (stream_id, pes_payload) = reassemble_pes_stream_id_and_payload(&buf, n, 0x101);

    assert_eq!(stream_id, 0xE0, "interop mode uses stream_id=0xE0");
    // No ts_open_bitstream_unit start code in interop mode — raw OBUs.
    assert_eq!(&pes_payload[..obu_payload.len()], &obu_payload[..]);
}

#[test]
fn av1_binding_mode_inserts_emulation_prevention_bytes() {
    // Per-OBU emulation prevention: a single OBU
    // whose body contains 0x00 0x00 0x01 MUST have a 0x03 inserted to
    // prevent the body bytes from being misread as a NEW start code.
    let cfg = MuxerConfig::default();
    // Default config is single H.264 program, no AV1. Build a custom AV1 cfg.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.pcr_interval_ms(cfg.pcr_interval_ms);
        b.psi_interval_ms(cfg.psi_interval_ms);
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    // One AV1 low-overhead OBU: Frame Header (obu_type=3), extension_flag=0,
    // has_size_field=1, reserved=0 → header byte `(3 << 3) | 0b010 = 0x1A`.
    // Body deliberately contains 0x00 0x00 0x01 to exercise the escape rule.
    let obu_payload = vec![
        0x1A, // OBU header: obu_type=3 (Frame Header), has_size_field=1
        0x04, // LEB128 size = 4
        0x00, 0x00, 0x01, 0xAA, // body: triggers emulation prevention
    ];
    m.push_video_to(
        VideoStreamHandle::pack(0, 0),
        &obu_payload,
        Pts90khz::new(90_000),
        false,
    )
    .unwrap();

    let mut buf = vec![0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let (_, pes_payload) = reassemble_pes_stream_id_and_payload(&buf, n, 0x101);

    // Expected on the wire:
    //   start code   : 00 00 01           (3-byte obu_start_code per §3.2)
    //   OBU header   : 1A
    //   LEB128 size  : 04
    //   body w/escape: 00 00 03 01 AA     (0x03 inserted between 00 00 and 01)
    let expected: &[u8] = &[
        0x00, 0x00, 0x01, // ts_open_bitstream_unit start code (3 bytes)
        0x1A, 0x04, 0x00, 0x00, 0x03, 0x01, 0xAA,
    ];
    assert_eq!(
        &pes_payload[..expected.len()],
        expected,
        "binding §3.2 emulation prevention must escape 0x00 0x00 0x0X (X<=3)"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// AV01 registration descriptor MUST be FIRST in PMT ES descriptor loop
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn pmt_av1_with_caller_supplied_av01_after_other_descriptor_reorders_to_first() {
    // Caller supplies [OtherDescriptor, AV01RegDescriptor]; the muxer MUST
    // reorder AV01 to position 0 per binding §2.1 ("MUST be FIRST").
    let other_desc: Vec<u8> = vec![
        0x52, // stream_identifier_descriptor tag
        0x01, // length
        0x07, // component_tag value
    ];
    let av01_desc: Vec<u8> = vec![0x05, 0x04, b'A', b'V', b'0', b'1'];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        prog.stream_descriptors_for_video(0, vec![other_desc.clone(), av01_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let entry = &muxer.pmt_descriptor_caches[0][0];

    // AV01 must come first (positions 0..6), followed by other_desc (3 bytes).
    assert_eq!(
        &entry[..6],
        &av01_desc[..],
        "AV01 reg must be reordered to position 0 per binding §2.1"
    );
    assert_eq!(
        &entry[6..6 + other_desc.len()],
        &other_desc[..],
        "non-AV01 descriptor must follow the reordered AV01"
    );
    assert_eq!(entry.len(), av01_desc.len() + other_desc.len());
}

#[test]
fn pmt_av1_without_av01_passes_caller_descriptors_through() {
    // Caller supplies a non-Registration descriptor only; auto-emit fires
    // and prepends AV01 (existing behavior — sanity that the reorder
    // logic doesn't disturb the auto-emit path).
    let other_desc: Vec<u8> = vec![0x52, 0x01, 0x07];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        prog.stream_descriptors_for_video(0, vec![other_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let entry = &muxer.pmt_descriptor_caches[0][0];

    // First 6 bytes auto-emitted AV01; then caller's other_desc bytes.
    assert_eq!(&entry[..6], &[0x05, 0x04, b'A', b'V', b'0', b'1']);
    assert_eq!(&entry[6..6 + other_desc.len()], &other_desc[..]);
}
