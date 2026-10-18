//! Real-corpus integration tests.
//!
//! Looks for `.ts` files in `tests/fixtures/local/` (gitignored). For each
//! file: extracts video AUs and KLV blobs, re-muxes them through our
//! `Muxer`, parses the output, asserts structural agreement.
//!
//! Skipped silently when `local/` is absent or empty — matches the existing
//! `klv` local-fixtures pattern.

use std::fs;
use std::path::Path;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_test_helpers::rawts;

const FIXTURES: &str = "tests/fixtures/local";

fn list_ts_files() -> Vec<std::path::PathBuf> {
    match fs::read_dir(FIXTURES) {
        Ok(rd) => rd
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ts"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn corpus_replay_structural_match() {
    let files = list_ts_files();
    if files.is_empty() {
        eprintln!("[skip] no fixtures in {}", FIXTURES);
        return;
    }
    for path in files {
        process_one(&path);
    }
}

fn process_one(path: &Path) {
    eprintln!("processing {}", path.display());
    let data = fs::read(path).expect("read fixture");

    // Use the existing parser to find what's in the original.
    let original = rawts::parsed(&data);
    let video_stream = match original
        .streams
        .iter()
        .find(|s| s.stream_type == 0x1B || s.stream_type == 0x24)
    {
        Some(s) => s,
        None => {
            eprintln!("  no video stream — skipping");
            return;
        }
    };
    let klv_stream = original.streams.iter().find(|s| s.klva);

    let codec = if video_stream.stream_type == 0x1B {
        VideoCodec::H264
    } else {
        VideoCodec::H265
    };

    // Re-mux with our Muxer.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, codec);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.buffer_packets(200_000);
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Interleave video + KLV pushes by PTS, draining incrementally to keep
    // the muxer queue bounded for real-corpus-sized inputs (>100 MB).
    let mut video_iter = original
        .pes_by_pid
        .get(&video_stream.pid)
        .map(|v| v.iter().enumerate())
        .into_iter()
        .flatten()
        .peekable();
    let mut klv_iter = klv_stream
        .and_then(|ks| original.pes_by_pid.get(&ks.pid))
        .map(|v| v.iter())
        .into_iter()
        .flatten()
        .peekable();

    let mut bytes: Vec<u8> = Vec::new();
    let mut drain_buf = vec![0u8; 188 * 4096];
    let mut skipped_video = 0usize;

    loop {
        let v_pts = video_iter.peek().map(|(_, (p, _))| p.unwrap_or(0));
        let k_pts = klv_iter.peek().map(|(p, _)| p.unwrap_or(0));
        match (v_pts, k_pts) {
            (None, None) => break,
            (Some(_), None) => {
                let (i, (pts, body)) = video_iter.next().unwrap();
                if !body.starts_with(&[0x00, 0x00, 0x00, 0x01])
                    && !body.starts_with(&[0x00, 0x00, 0x01])
                {
                    skipped_video += 1;
                    continue;
                }
                let key = i % 30 == 0;
                mux.push_video(body, Pts90khz::new(pts.unwrap_or(0) as i64), key)
                    .expect("push_video");
            }
            (None, Some(_)) => {
                let (pts, body) = klv_iter.next().unwrap();
                mux.push_klv(body, Pts90khz::new(pts.unwrap_or(0) as i64), 0x00)
                    .expect("push_klv");
            }
            (Some(vp), Some(kp)) => {
                if vp <= kp {
                    let (i, (pts, body)) = video_iter.next().unwrap();
                    if !body.starts_with(&[0x00, 0x00, 0x00, 0x01])
                        && !body.starts_with(&[0x00, 0x00, 0x01])
                    {
                        skipped_video += 1;
                        continue;
                    }
                    let key = i % 30 == 0;
                    mux.push_video(body, Pts90khz::new(pts.unwrap_or(0) as i64), key)
                        .expect("push_video");
                } else {
                    let (pts, body) = klv_iter.next().unwrap();
                    mux.push_klv(body, Pts90khz::new(pts.unwrap_or(0) as i64), 0x00)
                        .expect("push_klv");
                }
            }
        }
        // Drain after each push to keep the queue small.
        loop {
            let n = mux.pull(&mut drain_buf);
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&drain_buf[..n]);
        }
    }

    if skipped_video > 0 {
        eprintln!("  skipped {} non-Annex-B video AUs", skipped_video);
    }

    let recovered = rawts::parsed(&bytes);

    // Structural assertions:
    assert!(recovered.pmt_pid.is_some(), "PMT not present in output");
    assert!(
        recovered
            .streams
            .iter()
            .any(|s| s.stream_type == video_stream.stream_type),
        "video stream_type mismatch: orig {:#x}",
        video_stream.stream_type
    );
    if let Some(ks) = klv_stream {
        let our_klv = recovered.streams.iter().find(|s| s.klva);
        assert!(our_klv.is_some(), "KLVA descriptor missing in our output");
        let orig_count = original
            .pes_by_pid
            .get(&ks.pid)
            .map(|v| v.len())
            .unwrap_or(0);
        let our_count = recovered
            .pes_by_pid
            .get(&our_klv.unwrap().pid)
            .map(|v| v.len())
            .unwrap_or(0);
        assert_eq!(orig_count, our_count, "KLV PES count differs");
    }
}
