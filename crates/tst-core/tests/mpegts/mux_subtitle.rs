//! Integration tests for subtitle / caption sender side
//! (`mpegts::mux`).

use crate::common::mux_demux::drain_mux as drain_all;
use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec, VideoCodec,
};

/// One WebVTT document holding a single 0–5 s cue of `text`. The cue
/// timing is fixed; the presentation time rides the PES header.
fn webvtt_cue(text: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"WEBVTT\n\n");
    buf.extend_from_slice(
        format!(
            "{:02}:{:02}:{:02}.{:03} --> {:02}:{:02}:{:02}.{:03}\n",
            0, 0, 0, 0, 0, 0, 5, 0
        )
        .as_bytes(),
    );
    buf.extend_from_slice(text.as_bytes());
    buf.push(b'\n');
    buf
}

#[test]
fn mux_webvtt_in_single_program() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), &webvtt_cue("POI #1"))
        .unwrap();
    mux.push_subtitle_to(h, Pts90khz::new(90_000 * 5), &webvtt_cue("POI #2"))
        .unwrap();
    let out = drain_all(&mut mux);
    // PSI + 2 subtitle PES at minimum.
    assert!(out.len() > 188 * 3);
    // Subtitle PID must appear in TS packets.
    assert!(
        out.chunks_exact(188)
            .any(|p| { ((((p[1] & 0x1F) as u16) << 8) | (p[2] as u16)) == 0x200 })
    );
}

#[test]
fn mux_dvb_subtitling_multiple_languages() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        prog.add_subtitle(
            0x201,
            SubtitleCodec::DvbSubtitling {
                language: *b"spa",
                subtitling_type: 0x10,
                composition_page_id: 2,
                ancillary_page_id: 2,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    assert_eq!(mux.subtitle_handles().len(), 2);
    let handles = mux.subtitle_handles();
    for (i, h) in handles.iter().enumerate() {
        mux.push_subtitle_to(
            *h,
            Pts90khz::new(90_000 * (i as i64 + 1)),
            b"DVBSUB segment",
        )
        .unwrap();
    }
    let out = drain_all(&mut mux);
    assert!(!out.is_empty());
}

#[test]
fn mux_subtitle_with_klv_same_program_no_classification_collision() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_klv(
            0x300,
            KlvStreamType::PrivateData,
            /* carries_pts = */ false,
        );
        prog.add_subtitle(0x400, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_klv(b"\x06\x0E\x2B\x34short", Pts90khz::new(90_000), 0x00)
        .unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n")
        .unwrap();
    let out = drain_all(&mut mux);
    // Both PIDs distinct in TS packet stream.
    let pids: std::collections::HashSet<u16> = out
        .chunks_exact(188)
        .map(|p| (((p[1] & 0x1F) as u16) << 8) | (p[2] as u16))
        .collect();
    assert!(pids.contains(&0x300));
    assert!(pids.contains(&0x400));
}

#[test]
fn mux_multi_program_webvtt_and_klv() {
    let cfg = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x100);
        prog0.add_video(0x101, VideoCodec::H264);
        prog0.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x300);
        prog1.add_video(0x301, VideoCodec::H265);
        prog1.add_klv(0x400, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n")
        .unwrap();
    let out = drain_all(&mut mux);
    assert!(!out.is_empty());
}

#[test]
fn mux_push_subtitle_too_large_rejected() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let too_big = vec![0u8; 70_000];
    let err = mux.push_subtitle(Pts90khz::new(0), &too_big).unwrap_err();
    assert!(matches!(err, MuxError::SubtitleTooLarge { .. }));
}
