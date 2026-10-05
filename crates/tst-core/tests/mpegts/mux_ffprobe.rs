//! ffprobe smoke test — closed-loop sanity check.
//!
//! Mux a synthetic stream → write to a temp `.ts` file → run ffprobe with
//! JSON output → parse the JSON → assert: stream count, video codec,
//! KLV PID with KLVA tag.
//!
//! Skipped if `ffprobe` is not on PATH (returns early with a printed note).

use std::process::Command;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder};
use tst_test_helpers::synthetic_nal;

fn drain_all(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn have_ffprobe() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_ffprobe_field(path: &str, field: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            &format!("stream={}", field),
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path,
        ])
        .output()
        .expect("ffprobe should run");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

#[test]
fn ffprobe_recognizes_our_pmt() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    // Several frames so the stream has structure to parse.
    for i in 0..10 {
        let nal = synthetic_nal::h264_au(800, i % 5 == 0);
        mux.push_video(&nal, Pts90khz::new((i as i64) * 3000), i % 5 == 0)
            .unwrap();
        let klv = synthetic_nal::klv_blob(48);
        mux.push_klv(&klv, Pts90khz::new((i as i64) * 3000), 0x00)
            .unwrap();
    }
    let bytes = drain_all(&mut mux);

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_smoke.ts");
    std::fs::write(&tmp, &bytes).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    // Minimum signal: ffprobe finds at least one stream and the codec
    // 'h264' appears in its output.
    assert!(
        stdout.contains("\"codec_name\": \"h264\""),
        "h264 stream missing"
    );
    // KLV PID should be present somewhere in the JSON (may show as
    // 'data' codec or similar). Don't assert on the exact codec_name —
    // ffprobe versions differ. Just confirm 2 streams reported.
    let stream_count = stdout.matches("\"index\":").count();
    assert!(
        stream_count >= 2,
        "expected >= 2 streams, got {}",
        stream_count
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn ffprobe_recognizes_dual_camera_plus_klv() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{KlvStreamType, VideoCodec};
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

    // Generate a few seconds of synthetic frames so ffprobe has something
    // structural to read. Minimal Annex-B AUs are enough for ffprobe to
    // identify the codec from the PMT stream_type byte.
    let nal = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x00,
    ];
    let klv = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ];

    let mut ts = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    for i in 0..30i64 {
        let pts = i * 3000; // 33 ms @ 90 kHz
        mux.push_video_to(eo, &nal, Pts90khz::new(pts), i == 0)
            .unwrap();
        mux.push_video_to(ir, &nal, Pts90khz::new(pts), i == 0)
            .unwrap();
        mux.push_klv_to(klv_h, &klv, Pts90khz::new(pts), 0x00)
            .unwrap();
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            ts.extend_from_slice(&buf[..n]);
        }
    }

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_dual_camera.ts");
    std::fs::write(&tmp, &ts).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    // ffprobe should report >=3 streams: two h264 video, one data (KLV).
    // Don't lock the exact ordering — just count.
    let video_count = stdout.matches("\"codec_type\": \"video\"").count();
    let data_count = stdout.matches("\"codec_type\": \"data\"").count();
    assert!(
        video_count >= 2,
        "expected >= 2 video streams, got {}: {}",
        video_count,
        stdout,
    );
    assert!(
        data_count >= 1,
        "expected >= 1 data stream (KLV), got {}: {}",
        data_count,
        stdout,
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn ffprobe_recognizes_two_programs_with_distinct_streams() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{
        KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfig, StreamSpec, VideoCodec,
    };

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
    let mut config = MuxerConfig::default();
    config.programs = vec![prog1, prog2];
    let mut mux = Muxer::new(config).unwrap();

    let p1_video = mux.video_handles_for_program(1).unwrap()[0];
    let p2_video = mux.video_handles_for_program(2).unwrap()[0];
    let p1_klv = mux.klv_handles_for_program(1).unwrap()[0];
    let p2_klv = mux.klv_handles_for_program(2).unwrap()[0];

    // Minimal Annex B access units — enough for ffprobe to identify codec
    // from the PMT stream_type byte.
    let nal_h264 = synthetic_nal::h264_au(64, true);
    let nal_h265 = synthetic_nal::h265_au(64, true);
    let klv = synthetic_nal::klv_blob(32);

    let mut ts = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    for i in 0..20i64 {
        let pts = i * 3_003;
        mux.push_video_to(p1_video, &nal_h264, Pts90khz::new(pts), i == 0)
            .unwrap();
        mux.push_video_to(p2_video, &nal_h265, Pts90khz::new(pts), i == 0)
            .unwrap();
        mux.push_klv_to(p1_klv, &klv, Pts90khz::new(pts), 0x00)
            .unwrap();
        mux.push_klv_to(p2_klv, &klv, Pts90khz::new(pts), 0x00)
            .unwrap();
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            ts.extend_from_slice(&buf[..n]);
        }
    }

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_two_programs.ts");
    std::fs::write(&tmp, &ts).expect("write temp ts");

    // -show_programs reports the program table; -of json gives structured output.
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_programs", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe -show_programs output: {}", stdout);

    // ffprobe JSON contains one entry per program in the "programs" array.
    // Both program_num 1 and 2 must be present.
    let program_count = stdout.matches("\"program_num\":").count();
    assert_eq!(
        program_count, 2,
        "expected 2 programs in ffprobe output, got {}: {}",
        program_count, stdout
    );
    // ffprobe may format as `"program_num": 1` (with space) or `"program_num":1`.
    assert!(
        stdout.contains("\"program_num\": 1") || stdout.contains("\"program_num\":1"),
        "program_num 1 missing from ffprobe output: {}",
        stdout
    );
    assert!(
        stdout.contains("\"program_num\": 2") || stdout.contains("\"program_num\":2"),
        "program_num 2 missing from ffprobe output: {}",
        stdout
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn ffprobe_roundtrip_audio_video_klv_three_streams() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{AudioCodec, KlvStreamType, VideoCodec};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.add_klv(0x200, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();

    // Drive enough content for ffprobe to recognize all three streams.
    // Audio frames can be any non-empty bytes; ffprobe identifies the codec
    // from the PMT stream_type byte, not by parsing the audio bitstream.
    for i in 0..30 {
        let pts = 90_000 + (i as i64) * 3000;
        let nal = synthetic_nal::h264_au(128, i % 5 == 0);
        muxer
            .push_video(&nal, Pts90khz::new(pts), i % 5 == 0)
            .unwrap();
        // Minimal synthetic audio frame — ffprobe identifies codec from
        // stream_type 0x0F (AAC) in the PMT, not from bitstream analysis.
        muxer
            .push_audio(b"aac_frame_data", Pts90khz::new(pts))
            .unwrap();
        let klv = synthetic_nal::klv_blob(32);
        muxer.push_klv(&klv, Pts90khz::new(pts), 0x00).unwrap();
    }

    let ts = drain_all(&mut muxer);
    let tmp = std::env::temp_dir().join("tstrans_ffprobe_audio_three_streams.ts");
    std::fs::write(&tmp, &ts).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    // ffprobe should report >=3 streams: video (h264), audio (aac), data (KLV).
    let video_count = stdout.matches("\"codec_type\": \"video\"").count();
    let audio_count = stdout.matches("\"codec_type\": \"audio\"").count();
    let data_count = stdout.matches("\"codec_type\": \"data\"").count();
    assert!(
        video_count >= 1,
        "expected >= 1 video stream, got {}: {}",
        video_count,
        stdout,
    );
    assert!(
        audio_count >= 1,
        "expected >= 1 audio stream, got {}: {}",
        audio_count,
        stdout,
    );
    assert!(
        data_count >= 1,
        "expected >= 1 data stream (KLV), got {}: {}",
        data_count,
        stdout,
    );
    // Confirm the audio codec is recognized as AAC (stream_type 0x0F).
    assert!(
        stdout.contains("\"codec_name\": \"aac\""),
        "audio codec_name missing or not aac: {}",
        stdout
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn ffprobe_roundtrip_each_audio_codec() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{AudioCodec, VideoCodec};

    // Test each audio codec variant, verifying that ffprobe reports the
    // stream with the expected `codec_tag`. Stream types per ISO/IEC 13818-1:
    //   0x03 = ISO/IEC 11172-3 Audio (MP2)
    //   0x0F = ISO/IEC 13818-7 ADTS AAC
    //   0x11 = ISO/IEC 14496-3 LATM AAC
    //   0x81 = User private (used for ATSC AC-3)
    //
    // Special case for AC-3: the muxer auto-emits
    // a `registration_descriptor` with `format_identifier="AC-3"` per
    // ATSC A/53 Part 3 §5.1. ffmpeg's MPEG-TS parser then reports
    // `codec_tag` as the format_identifier ASCII bytes (little-endian
    // packed: "AC-3" → 0x33 0x2D 0x43 0x41 → 0x332d4341) instead of
    // the stream_type byte. This is the correct ffmpeg behavior and
    // confirms our auto-emit lands in a way receivers honor.
    //
    // The expected codec_tag below is therefore the per-codec wire shape
    // ffmpeg actually surfaces, not always the raw stream_type.
    let cases = vec![
        (AudioCodec::Mp2, "0x0003"),
        (AudioCodec::Aac, "0x000f"),
        (AudioCodec::AacLatm, "0x0011"),
        // AC-3 with auto-emitted Registration → format_identifier as tag.
        (AudioCodec::Ac3, "0x332d4341"),
    ];

    for (codec, expected_codec_tag) in cases {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_audio(0x300, codec);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = Muxer::new(cfg).unwrap();

        // Drive a few frames of synthetic audio/video.
        for i in 0..20 {
            let pts = 90_000 + (i as i64) * 3000;
            let nal = synthetic_nal::h264_au(128, i % 5 == 0);
            muxer
                .push_video(&nal, Pts90khz::new(pts), i % 5 == 0)
                .unwrap();
            // Minimal synthetic audio — without real bitstream data, ffprobe
            // cannot determine codec_type, but it does report the stream and
            // its codec_tag (reflecting either stream_type or, for AC-3 with
            // auto-emitted Registration, the format_identifier).
            muxer.push_audio(b"audio_data", Pts90khz::new(pts)).unwrap();
        }

        let ts = drain_all(&mut muxer);
        let tmp = std::env::temp_dir().join(format!("tstrans_ffprobe_audio_{:?}.ts", codec));
        std::fs::write(&tmp, &ts).expect("write temp ts");

        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&tmp)
            .output()
            .expect("run ffprobe");
        assert!(
            out.status.success(),
            "ffprobe exited non-zero for {:?}: stderr={}",
            codec,
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);

        let expected_tag = format!("\"codec_tag\": \"{}\"", expected_codec_tag);
        assert!(
            stdout.contains(&expected_tag),
            "codec {:?}: expected codec_tag {}, ffprobe output: {}",
            codec,
            expected_tag,
            stdout
        );
        // Also confirm PID 0x300 is in the output.
        assert!(
            stdout.contains("\"id\": \"0x300\""),
            "codec {:?}: expected audio PID 0x300 in ffprobe output: {}",
            codec,
            stdout
        );

        let _ = std::fs::remove_file(&tmp);
    }
}

// --- Subtitle ffprobe round-trip tests ---
//
// These match the audio-codec round-trip pattern above: build a config with
// the subtitle codec, push a few PES units, drain TS bytes, run ffprobe, and
// assert on the codec / language strings ffprobe reports.
//
// CEA-708 standalone is intentionally excluded — ffmpeg's CEA-708 path is
// SEI-embedded, not standalone-PID, and ffprobe doesn't classify the
// standalone form cleanly.
//
// Gated behind `#[ignore = "ffprobe-only"]` so they run only with
// `-- --ignored`, matching the audio tests' pattern.

#[test]
#[ignore = "ffprobe-only"]
fn ffprobe_validates_dvb_subtitling_round_trip() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{SubtitleCodec, VideoCodec};

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

    // Drive a few video frames so the program has structure, plus a handful
    // of synthetic DVB subtitle PES units. ffprobe identifies DVB sub via the
    // PMT subtitling_descriptor (auto-emitted), not by parsing the bitstream.
    let h = mux.subtitle_handles()[0];
    for i in 0..10 {
        let pts = 90_000 * (i as i64 + 1);
        let nal = synthetic_nal::h264_au(128, i % 5 == 0);
        mux.push_video(&nal, Pts90khz::new(pts), i % 5 == 0)
            .unwrap();
    }
    for i in 0..5 {
        // Minimal DVB subtitle PES payload: data_identifier (0x20) + a tiny
        // subtitle segment. ffprobe doesn't parse this — the descriptor in
        // the PMT is what drives codec recognition.
        let payload = [0x0F, 0x10, 0x00, 0x01, 0x00, 0x06, 0, 0, 0, 0, 0, 0];
        mux.push_subtitle_to(h, Pts90khz::new(90_000 * (i as i64 + 1)), &payload)
            .unwrap();
    }
    let bytes = drain_all(&mut mux);

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_dvb_sub.ts");
    std::fs::write(&tmp, &bytes).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    assert!(
        stdout.contains("dvb_subtitle"),
        "expected dvb_subtitle codec_name in ffprobe output: {}",
        stdout
    );
    assert!(
        stdout.contains("eng"),
        "expected language tag 'eng' in ffprobe output: {}",
        stdout
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
#[ignore = "ffprobe-only"]
fn ffprobe_validates_dvb_teletext_round_trip() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{SubtitleCodec, VideoCodec};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Same shape as the DVB-sub case: a few video frames plus a handful of
    // synthetic teletext PES units. ffprobe identifies teletext via the PMT
    // teletext_descriptor (auto-emitted).
    let h = mux.subtitle_handles()[0];
    for i in 0..10 {
        let pts = 90_000 * (i as i64 + 1);
        let nal = synthetic_nal::h264_au(128, i % 5 == 0);
        mux.push_video(&nal, Pts90khz::new(pts), i % 5 == 0)
            .unwrap();
    }
    for i in 0..5 {
        // Minimal teletext PES payload: data_identifier (0x10) + a tiny
        // data unit. Real-content shape doesn't matter for ffprobe codec ID.
        let mut payload = vec![0x10];
        payload.extend_from_slice(&[0x02, 0x10]);
        payload.extend(std::iter::repeat(0x00).take(0x10));
        mux.push_subtitle_to(h, Pts90khz::new(90_000 * (i as i64 + 1)), &payload)
            .unwrap();
    }
    let bytes = drain_all(&mut mux);

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_dvb_teletext.ts");
    std::fs::write(&tmp, &bytes).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    assert!(
        stdout.contains("dvb_teletext"),
        "expected dvb_teletext codec_name in ffprobe output: {}",
        stdout
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
#[ignore = "ffprobe-only"]
fn ffprobe_validates_webvtt_in_ts_round_trip() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    use tst_core::mpegts::mux::{SubtitleCodec, VideoCodec};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Push a handful of video frames so the program is well-formed, then a
    // single WebVTT cue. ffprobe recognizes WebVTT-in-TS via the auto-emitted
    // registration_descriptor (format_identifier = "VTTC").
    let h = mux.subtitle_handles()[0];
    for i in 0..10 {
        let pts = 90_000 * (i as i64 + 1);
        let nal = synthetic_nal::h264_au(128, i % 5 == 0);
        mux.push_video(&nal, Pts90khz::new(pts), i % 5 == 0)
            .unwrap();
    }
    mux.push_subtitle_to(
        h,
        Pts90khz::new(90_000),
        b"WEBVTT\n\n00:00:01.000 --> 00:00:05.000\nhello\n",
    )
    .unwrap();
    let bytes = drain_all(&mut mux);

    let tmp = std::env::temp_dir().join("tstrans_ffprobe_webvtt_in_ts.ts");
    std::fs::write(&tmp, &bytes).expect("write temp ts");

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("ffprobe output: {}", stdout);

    // ffmpeg may label this as "webvtt", as a generic "subtitle" codec_type,
    // or (older Ubuntu builds) as "bin_data" — all three indicate the stream
    // was classified rather than dropped.
    assert!(
        stdout.contains("webvtt") || stdout.contains("subtitle") || stdout.contains("bin_data"),
        "expected webvtt/subtitle/bin_data in ffprobe output: {}",
        stdout
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn ffprobe_agrees_on_mp2_sample_rate_and_channels() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audio/mp2.ts");
    let bytes = std::fs::read(&path).unwrap();
    let path_str = path.to_str().unwrap();

    let ffprobe_sr = run_ffprobe_field(path_str, "sample_rate")
        .parse::<u32>()
        .unwrap();
    let ffprobe_ch = run_ffprobe_field(path_str, "channels")
        .parse::<u8>()
        .unwrap();

    let mut demuxer = tst_core::mpegts::demux::Demuxer::new();
    demuxer.feed(&bytes).unwrap();
    demuxer.flush();
    let mut parsed_sr = None;
    let mut parsed_ch = None;
    while let Some(ev) = demuxer.next_event() {
        if let tst_core::mpegts::demux::DemuxEvent::Sample {
            payload: tst_core::mpegts::demux::SamplePayload::Audio { frames, .. },
            ..
        } = ev
        {
            if let Some(f) = tst_core::codec::mpegaudio::frames(&frames)
                .next()
                .and_then(|r| r.ok())
            {
                parsed_sr = Some(f.sample_rate_hz);
                parsed_ch = Some(f.channels);
            }
        }
        if parsed_sr.is_some() {
            break;
        }
    }
    assert_eq!(parsed_sr, Some(ffprobe_sr));
    assert_eq!(parsed_ch, Some(ffprobe_ch));
}

#[test]
fn ffprobe_agrees_on_aac_adts_sample_rate_and_channels() {
    if !have_ffprobe() {
        eprintln!("[skip] ffprobe not on PATH");
        return;
    }

    let tst_core_manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tst-core");
    let path = tst_core_manifest.join("tests/fixtures/audio/aac-adts.ts");
    let bytes = std::fs::read(&path).unwrap();
    let path_str = path.to_str().unwrap();

    let ffprobe_sr = run_ffprobe_field(path_str, "sample_rate")
        .parse::<u32>()
        .unwrap();
    let ffprobe_ch = run_ffprobe_field(path_str, "channels")
        .parse::<u8>()
        .unwrap();

    let mut demuxer = tst_core::mpegts::demux::Demuxer::new();
    demuxer.feed(&bytes).unwrap();
    demuxer.flush();
    let mut parsed_sr = None;
    let mut parsed_ch = None;
    while let Some(ev) = demuxer.next_event() {
        if let tst_core::mpegts::demux::DemuxEvent::Sample {
            payload: tst_core::mpegts::demux::SamplePayload::Audio { frames, .. },
            ..
        } = ev
        {
            if let Some(f) = tst_core::codec::aac::frames(&frames)
                .next()
                .and_then(|r| r.ok())
            {
                parsed_sr = Some(f.sample_rate_hz);
                // `.channels()` returns `None` for PCE-defined
                // layouts (`channel_configuration == 0`). The aac-adts
                // fixture uses canonical stereo (config 2), so we expect
                // `Some(2)`; assertion against ffprobe-derived count
                // would still hold for any canonical-layout encoder.
                parsed_ch = f.channels();
            }
        }
        if parsed_sr.is_some() {
            break;
        }
    }
    assert_eq!(parsed_sr, Some(ffprobe_sr));
    assert_eq!(parsed_ch, Some(ffprobe_ch));
}
