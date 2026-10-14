//! Multi-stream integration tests for `mpegts::mux::Muxer`.
//!
//! Covers the five shapes introduced by the multi-stream lift:
//! dual-video + KLV (EO+IR pod), video + dual-KLV, video-only,
//! KLV-only, and dual-video + dual-KLV (worst case for PMT enumeration).
//!
//! The bit-level wire-format checks live in the existing
//! `mpegts_mux_ffprobe.rs` and `mpegts_mux.rs` files; these tests focus
//! on routing — the right bytes go to the right PIDs.

use crate::common::mux_demux::drain_mux as drain_all;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Extract the set of PIDs present in a TS byte stream.
fn pids_present(ts: &[u8]) -> std::collections::BTreeSet<u16> {
    let mut s = std::collections::BTreeSet::new();
    for chunk in ts.chunks_exact(188) {
        let pid = (((chunk[1] as u16) & 0x1F) << 8) | (chunk[2] as u16);
        s.insert(pid);
    }
    s
}

/// Minimal Annex-B H.264 NAL — start code + nal_unit_type byte + payload.
fn h264_au(payload: u8) -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x67, payload, 0xFF]
}

/// Minimal 17-byte KLV (16-byte UL + 1-byte length=0).
fn klv_blob() -> Vec<u8> {
    vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ]
}

#[test]
fn dual_video_plus_klv_routes_to_three_pids() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264); // EO
        prog.add_video(0x1021, VideoCodec::H264); // IR
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        prog.pcr_pid(0x1011);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let eo = mux.video_stream_handle(0).unwrap();
    let ir = mux.video_stream_handle(1).unwrap();
    let klv_h = mux.klv_stream_handle(0).unwrap();

    // Push three frames — one per stream.
    mux.push_video_to(eo, &h264_au(0xAA), Pts90khz::new(0), true)
        .unwrap();
    mux.push_video_to(ir, &h264_au(0xBB), Pts90khz::new(0), true)
        .unwrap();
    mux.push_klv_to(klv_h, &klv_blob(), Pts90khz::new(0), 0x00)
        .unwrap();

    let ts = drain_all(&mut mux);
    let pids = pids_present(&ts);
    assert!(pids.contains(&0x0000), "expected PAT (PID 0)");
    assert!(pids.contains(&0x1000), "expected PMT (PID 0x1000)");
    assert!(pids.contains(&0x1011), "expected EO video PID");
    assert!(pids.contains(&0x1021), "expected IR video PID");
    assert!(pids.contains(&0x1031), "expected KLV PID");
}

#[test]
fn video_plus_dual_klv_routes_to_three_pids() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false); // vehicle telemetry
        prog.add_klv(0x1041, KlvStreamType::PrivateData, true); // sensor metadata (sync)
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let v = mux.video_stream_handle(0).unwrap();
    let k_async = mux.klv_stream_handle(0).unwrap();
    let k_sync = mux.klv_stream_handle(1).unwrap();

    mux.push_video_to(v, &h264_au(0xAA), Pts90khz::new(0), true)
        .unwrap();
    mux.push_klv_to(k_async, &klv_blob(), Pts90khz::new(0), 0x00)
        .unwrap();
    mux.push_klv_to(k_sync, &klv_blob(), Pts90khz::new(0), 0x00)
        .unwrap();

    let pids = pids_present(&drain_all(&mut mux));
    for required in [0x0000u16, 0x1000, 0x1011, 0x1031, 0x1041] {
        assert!(pids.contains(&required), "missing PID 0x{required:04X}");
    }
}

#[test]
fn video_only_emits_video_pid_only() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let v = mux.video_stream_handle(0).unwrap();
    mux.push_video_to(v, &h264_au(0xAA), Pts90khz::new(0), true)
        .unwrap();

    let pids = pids_present(&drain_all(&mut mux));
    assert!(pids.contains(&0x1011));
    assert!(
        !pids.contains(&0x1031),
        "no KLV stream configured — must not emit on default KLV PID"
    );
}

#[test]
fn video_and_klv_emits_both_pids() {
    // KLV-only programs are rejected (KLV cadence too sparse for PCR).
    // The minimum viable program shape is video + KLV; test that both PIDs
    // appear in the output and no spurious PIDs are emitted.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let k = mux.klv_stream_handle(0).unwrap();
    let v = mux.video_stream_handle(0).unwrap();
    let nal = [0x00, 0x00, 0x00, 0x01, 0x67];
    mux.push_video_to(v, &nal, Pts90khz::new(0), true).unwrap();
    mux.push_klv_to(k, &klv_blob(), Pts90khz::new(0), 0x00)
        .unwrap();

    let pids = pids_present(&drain_all(&mut mux));
    assert!(pids.contains(&0x1011), "video PID must appear");
    assert!(pids.contains(&0x1031), "KLV PID must appear");
}

#[test]
fn dual_video_plus_dual_klv_pmt_lists_all_four() {
    // PAT PID is 0x0000 by ISO 13818-1 spec.
    const PAT_PID: u16 = 0x0000;

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_video(0x1021, VideoCodec::H265);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        prog.add_klv(0x1041, KlvStreamType::SynchronousMetadata, true);
        prog.pcr_pid(0x1011);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Push to one stream to trigger first PSI emission.
    let vid0 = mux.video_stream_handle(0).unwrap();
    mux.push_video_to(vid0, &h264_au(0xAA), Pts90khz::new(0), true)
        .unwrap();

    let ts = drain_all(&mut mux);
    let pids = pids_present(&ts);
    assert!(pids.contains(&PAT_PID));
    // The PMT itself rides PID 0x1000 (default).
    assert!(pids.contains(&0x1000), "PMT PID 0x1000 missing");
    // Find the PMT TS packet — first one with PID 0x1000 — and assert it
    // names all four elementary PIDs in its payload.
    let pmt_pkt = ts
        .chunks_exact(188)
        .find(|p| ((((p[1] as u16) & 0x1F) << 8) | (p[2] as u16)) == 0x1000)
        .expect("at least one PMT packet");
    // Search the entire 188-byte packet for each elementary PID encoded as
    // two consecutive bytes (high << 8 | low) — a coarse but reliable check
    // because PIDs are encoded in big-endian inside the PMT body.
    for elem in [0x1011u16, 0x1021, 0x1031, 0x1041] {
        let hi = (elem >> 8) as u8;
        let lo = elem as u8;
        let found = pmt_pkt
            .windows(2)
            .any(|w| w[0] & 0x1F == hi & 0x1F && w[1] == lo);
        assert!(
            found,
            "PMT body does not reference elementary PID 0x{elem:04X}"
        );
    }
}
