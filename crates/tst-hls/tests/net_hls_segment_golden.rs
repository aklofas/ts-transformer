//! `MuxPublisher<HlsPublisher>` writes byte-stable segments: a fixed feed
//! must reproduce the committed `segment_*.ts` files and the playlist's
//! structure exactly. This is the HLS-layer mirror of
//! `tst-pipeline`'s `mux_sender_golden`: it pins PSI content, continuity
//! counters and PCR across the segmenting transport, so a change to the
//! drain, cut or PSI order surfaces as a byte diff.
//!
//! Set `TST_REGEN_HLS_GOLDEN=1` to write the fixtures instead of comparing.

#![cfg(feature = "serve")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hls-golden")
}

fn idr() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend((0u8..200).map(|i| 0xA5 ^ i));
    v
}

fn p_frame() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x30];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x41]);
    v.extend((0u8..120).map(|i| 0x5A ^ i));
    v
}

/// A 16-byte universal-key prefix plus a short value, enough for the muxer's
/// KLV path; the content is arbitrary but fixed.
fn klv(seed: u8) -> Vec<u8> {
    let mut v = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x04,
    ];
    v.extend((0u8..4).map(|i| seed.wrapping_add(i)));
    v
}

#[test]
fn hls_segments_match_the_committed_golden() {
    let out = std::env::temp_dir().join(format!("hls-golden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&out)
        .segment_duration(Duration::from_secs(10))
        .playlist_window(6)
        .mode(HlsMode::Vod)
        .build()
        .unwrap();

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };

    let shell = MuxPublisher::with_config(publisher, cfg).unwrap();
    shell.push_video(&idr(), Pts90khz::new(0), true).unwrap();
    shell.push_klv(&klv(1), Pts90khz::new(0), 0).unwrap();
    shell
        .push_video(&p_frame(), Pts90khz::new(3000), false)
        .unwrap();
    shell.push_klv(&klv(2), Pts90khz::new(3000), 0).unwrap();
    shell
        .push_video(&idr(), Pts90khz::new(270_000), true)
        .unwrap();
    shell.cut_segment().unwrap();
    shell
        .push_video(&p_frame(), Pts90khz::new(279_000), false)
        .unwrap();
    let publisher = shell.finish().unwrap();
    publisher.finish().unwrap();

    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("segment_") && n.ends_with(".ts"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no segments were written");

    // Segments cut by the wall clock carry a wall-clock `#EXTINF`; every
    // other playlist line is a function of the feed alone. The first segment
    // spans 3 s of PTS (0 -> 270 000), so its duration is pinned exactly.
    let playlist = std::fs::read_to_string(out.join("playlist.m3u8")).unwrap();
    assert!(
        playlist.contains("#EXTINF:3.000,\nsegment_00000.ts"),
        "the first segment's PTS-derived duration drifted:\n{playlist}"
    );
    let mut stable = String::new();
    for l in playlist.lines().filter(|l| !l.starts_with("#EXTINF:")) {
        stable.push_str(l);
        stable.push('\n');
    }

    let dir = fixture_dir();
    if std::env::var_os("TST_REGEN_HLS_GOLDEN").is_some() {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in &names {
            std::fs::copy(out.join(n), dir.join(n)).unwrap();
        }
        std::fs::write(dir.join("playlist.stable.m3u8"), &stable).unwrap();
        return;
    }

    let mut golden: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("segment_"))
        .collect();
    golden.sort();
    assert_eq!(names, golden, "segment set diverged from the golden");
    for n in &names {
        assert_eq!(
            std::fs::read(out.join(n)).unwrap(),
            std::fs::read(dir.join(n)).unwrap(),
            "{n} diverged from the committed golden"
        );
    }
    // A Windows checkout may turn the committed playlist's line endings into
    // CRLF; the publisher writes LF, so compare on normalised text.
    let golden_playlist = std::fs::read_to_string(dir.join("playlist.stable.m3u8"))
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(
        stable, golden_playlist,
        "playlist structure diverged from the committed golden"
    );
    let _ = std::fs::remove_dir_all(&out);
}
