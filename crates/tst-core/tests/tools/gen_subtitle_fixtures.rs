//! Generate synthetic subtitle / caption MPEG-TS fixtures for tests.
//!
//! Hand-rolls codec-specific payload shapes (DVB-sub
//! `subtitle_data_segment` per ETSI EN 300 743; DVB-teletext
//! `teletext_data_unit` per ETSI EN 300 706; CEA-708 `cc_data_pkt`
//! per CEA-708-D; WebVTT cue per Apple HLS WebVTT-in-TS draft) and
//! uses our own `Muxer` to wrap them in MPEG-TS. Bootstrap cycle —
//! the muxer must work to emit these, and the resulting fixtures
//! then guard against future regressions in either side (mux + demux).
//!
//! Run: `cargo run -p tst-core --bin gen-subtitle-fixtures -- <output-dir>`.
//! `regen.sh` invokes this with the fixtures dir as argv[1].
//!
//! Payloads are intentionally synthetic — none of these contain real
//! caption content. The shapes match each codec's segment / data-unit
//! / cue framing so the demuxer's classification + descriptor parsing
//! gets exercised, but the contents won't decode to real captions.

use std::fs;
use std::path::PathBuf;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec,
    VideoCodec,
};

/// Drain every queued packet from the muxer into a single `Vec<u8>`.
///
/// Mirrors the helper in `tests/mpegts_mux_subtitle.rs` — no public
/// `drain_output` on `Muxer`, so we pull in chunks until exhausted.
fn drain_all(mux: &mut Muxer) -> Vec<u8> {
    let mut all = Vec::new();
    let mut chunk = vec![0u8; 188 * 256];
    loop {
        let n = mux.pull(&mut chunk);
        if n == 0 {
            break;
        }
        all.extend_from_slice(&chunk[..n]);
    }
    all
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    fs::create_dir_all(&out_dir)?;

    write(&out_dir, "dvb_subtitling_eng.ts", build_dvb_sub_eng_only());
    write(
        &out_dir,
        "dvb_subtitling_multi_lang.ts",
        build_dvb_sub_multi_lang(),
    );
    write(&out_dir, "dvb_teletext_eng.ts", build_dvb_teletext_eng());
    write(&out_dir, "cea708_standalone.ts", build_cea708_standalone());
    write(&out_dir, "webvtt_in_ts_simple.ts", build_webvtt_simple());
    write(
        &out_dir,
        "webvtt_in_ts_multi_cue.ts",
        build_webvtt_multi_cue(),
    );
    write(
        &out_dir,
        "subtitle_with_klv_same_program.ts",
        build_subtitle_with_klv(),
    );
    write(
        &out_dir,
        "webvtt_multi_program_with_klv.ts",
        build_webvtt_multi_program(),
    );
    write(
        &out_dir,
        "non_conformant_subtitle_missing_descriptor.ts",
        build_non_conformant_missing_descriptor(),
    );

    println!("wrote 9 fixtures to {}", out_dir.display());
    Ok(())
}

fn write(dir: &std::path::Path, name: &str, bytes: Vec<u8>) {
    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write fixture");
    println!(
        "  {} ({} bytes)",
        path.file_name().unwrap().to_string_lossy(),
        bytes.len()
    );
}

/// Single English DVB subtitling stream alongside an H.264 video.
///
/// Smallest realistic DVB-sub fixture — exercises the
/// `subtitling_descriptor` emission path with one entry, and the
/// `extract_user_label` "DVB sub eng" arm in the demuxer.
fn build_dvb_sub_eng_only() -> Vec<u8> {
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
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    // Synthetic page-composition-segment per ETSI EN 300 743 §7.2.2
    // Table 9: sync_byte=0x0F + segment_type=0x10 (page composition) +
    // page_id BE u16 + segment_length BE u16 + body. segment_length=2
    // means zero regions (page_time_out byte + packed page_version /
    // page_state byte only). The muxer auto-prepends the §6.2
    // PES_data_field envelope (0x20 + 0x00 + segments + 0xFF), so the
    // caller passes raw segment bytes.
    for i in 0..3 {
        let pts = 90_000 * (i as i64 + 1);
        mux.push_subtitle_to(
            h,
            Pts90khz::new(pts),
            &[0x0F, 0x10, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10],
        )
        .unwrap();
    }
    drain_all(&mut mux)
}

/// Two DVB subtitling streams (eng + spa) on two PIDs.
///
/// Forces `subtitling_descriptor` to carry two entries — exercises
/// the multi-language descriptor write/parse path. The PMT must list
/// both subtitle PIDs and each must have its own
/// `subtitling_descriptor` (one entry per PID is the conformant
/// shape; some encoders pack both in one descriptor — we emit
/// per-PID and the demuxer should accept both forms).
fn build_dvb_sub_multi_lang() -> Vec<u8> {
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
    for h in mux.subtitle_handles() {
        // segment_length=2: zero regions per EN 300 743 §7.2.2 Table 9.
        // Muxer auto-wraps the §6.2 PES_data_field envelope around this.
        mux.push_subtitle_to(
            h,
            Pts90khz::new(90_000),
            &[0x0F, 0x10, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10],
        )
        .unwrap();
    }
    drain_all(&mut mux)
}

/// DVB teletext, English, magazine 8 page 88 — the canonical
/// subtitle-via-teletext convention (some EBU broadcasters still use
/// this in 2026).
///
/// Payload: a `teletext_data_unit` per ETSI EN 300 706 §11.2 wrapped
/// in EBU teletext PES (data_identifier=0x10). The data-unit-id 0x02
/// is "EBU teletext non-subtitle data" — the exact value isn't
/// meaningful for our test (we just need a valid framing shape).
fn build_dvb_teletext_eng() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                // 3-bit field; 0 = magazine 8 per the canonical EBU
                // subtitle convention (magazine 8 page 88).
                magazine_number: 0,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    // Teletext PES payload: data_identifier(0x10) +
    // (data_unit_id, data_unit_length, line bytes).
    // Length 0x2C = 44 bytes is the standard EBU teletext line size.
    for i in 0..3 {
        let pts = 90_000 * (i as i64 + 1);
        let mut payload = vec![0x10]; // data_identifier
        payload.extend_from_slice(&[0x02, 0x2C]); // data_unit_id=0x02, length=0x2C
        payload.extend(std::iter::repeat(0x00).take(0x2C));
        mux.push_subtitle_to(h, Pts90khz::new(pts), &payload)
            .unwrap();
    }
    drain_all(&mut mux)
}

/// CEA-708 standalone caption stream (carried as a separate elementary
/// stream rather than embedded in H.264/H.265 SEI).
///
/// Payload: synthetic `cc_data_pkt`s. cc_valid|cc_type byte = 0xFC
/// (cc_valid=1, cc_type=0b00 = CEA-608 line 21 field 1) followed by
/// two cc_data bytes (0x80 0x80 = NUL/NUL). Two such packets per
/// frame is the standard 60-Hz cadence; we just need a recognizable
/// shape, not real caption content.
fn build_cea708_standalone() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::Cea708Standalone);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    for i in 0..3 {
        mux.push_subtitle_to(
            h,
            Pts90khz::new(90_000 * (i as i64 + 1)),
            &[0xFC, 0x80, 0x80, 0xFC, 0x80, 0x80],
        )
        .unwrap();
    }
    drain_all(&mut mux)
}

/// WebVTT-in-MPEG-TS, single cue. Smallest realistic shape per Apple's
/// HLS WebVTT-in-TS draft — header line, blank line, timing line,
/// payload, trailing newline.
fn build_webvtt_simple() -> Vec<u8> {
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
    let cue = b"WEBVTT\n\n00:00:01.000 --> 00:00:05.000\nhello world\n";
    mux.push_subtitle_to(h, Pts90khz::new(90_000), cue).unwrap();
    drain_all(&mut mux)
}

/// WebVTT-in-MPEG-TS, five back-to-back cues with stepping PTS. Each
/// cue is a complete (self-contained) WebVTT chunk — Apple's HLS
/// authoring spec carries one cue per PES, not the WebVTT-file shape
/// of one header + many cues.
fn build_webvtt_multi_cue() -> Vec<u8> {
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
    for i in 0..5 {
        let cue = format!(
            "WEBVTT\n\n00:00:0{}.000 --> 00:00:0{}.000\nPOI #{}\n",
            i,
            i + 1,
            i,
        );
        mux.push_subtitle_to(h, Pts90khz::new(90_000 * (i as i64 + 1)), cue.as_bytes())
            .unwrap();
    }
    drain_all(&mut mux)
}

/// One program carrying H.264 video + KLV (async, ST 1402-style) +
/// WebVTT subtitles. Cascade exercise — ensures the demuxer routes
/// each PID to the right `SamplePayload` variant when all three
/// kinds are present. KLV uses `PrivateData` (0x06) which collides
/// with DVB subtitle's stream_type byte; the demuxer disambiguates
/// via the registration / VTTC / subtitling descriptors.
fn build_subtitle_with_klv() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_klv(0x300, KlvStreamType::PrivateData, false);
        prog.add_subtitle(0x400, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    // Dummy KLV payload: the full 16-byte SMPTE 336M UL header (which
    // `classify_klv` requires for the bare-LS path) plus a trailing
    // length+value stub. Not a real ST 0601 packet, but enough for the
    // demuxer to surface as a KLV `Metadata` event (the 16-byte UL
    // minimum is enforced inside `classify_klv`).
    mux.push_klv(
        b"\x06\x0E\x2B\x34\x02\x0B\x01\x01\x0E\x01\x03\x01\x01\x00\x00\x00\x02\xAB\xCD",
        Pts90khz::new(90_000),
        0x00, // spec default; ST 1402.2 App. B Table 2
    )
    .unwrap();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n")
        .unwrap();
    drain_all(&mut mux)
}

/// Two programs in one TS:
/// - Program 1: H.264 + WebVTT subtitles
/// - Program 2: H.265 + KLV (no subtitles)
///
/// Forces `subtitle_handles_for_program` to disambiguate which
/// program owns the WebVTT stream — the demuxer must route program-2
/// PIDs without spuriously emitting a subtitle event for them.
fn build_webvtt_multi_program() -> Vec<u8> {
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
    let h = mux.subtitle_handles_for_program(1).unwrap()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n")
        .unwrap();
    drain_all(&mut mux)
}

/// Non-conformant fixture for the `treat_as` override path.
///
/// Routes WebVTT-shaped bytes through the AUDIO config path with
/// `AudioCodec::Mp2` (stream_type 0x03). Result: PMT declares PID
/// 0x200 as MP2 audio with NO subtitle descriptor of any kind —
/// non-conformance is structural-by-construction (we don't have to
/// rewrite PMT bytes after the fact).
///
/// Default demux behavior: classify PID 0x200 as Audio(Mp2), emit
/// `SamplePayload::Audio` carrying the WebVTT bytes. The codec /
/// payload mismatch is the caller's problem in that path.
///
/// With `DemuxerConfig::treat_as.insert(0x200, StreamKind::Subtitle(WebVttInTs))`:
/// demuxer reroutes PID 0x200 to the subtitle dispatch path, emits
/// `SamplePayload::Subtitle`, AND emits
/// `NonConformantIssue::SubtitleMissingDescriptor` because no subtitle
/// descriptor is present in the PMT. The `treat_as` test uses exactly this
/// fixture to verify both halves.
fn build_non_conformant_missing_descriptor() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x200, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.audio_handles()[0];
    // WebVTT-shaped bytes (intentionally codec-mismatched against the
    // declared MP2 stream_type — that mismatch IS the non-conformance).
    let webvtt = b"WEBVTT\n\n00:00:01.000 --> 00:00:05.000\nhello\n";
    mux.push_audio_to(h, Pts90khz::new(90_000), webvtt).unwrap();
    drain_all(&mut mux)
}
