//! Regression tests for multi-program shorthand routing: `push_video()`
//! and `push_klv()` must not use `pack(0, 0)`, which misroutes when the
//! lone stream of that kind sits in program-index >= 1.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfig, StreamSpec, VideoCodec,
};
use tst_test_helpers::synthetic_nal;

/// Two-program config where the lone video stream lives in program-index 1:
/// - Program 1 (pmt_pid 0x1000): audio only (Mp2) at PID 0x1011 — no video, no KLV.
/// - Program 2 (pmt_pid 0x1100): one H.264 video at PID 0x1111.
///
/// Forces `total_video == 1` globally (so `push_video` doesn't short-circuit
/// on `AmbiguousTarget`) while the lone video sits in program-index 1
/// (`video_streams[0]` empty, `video_streams[1]` non-empty) — the exact
/// shape that exposes the `pack(0, 0)` bug.
fn config_video_in_program_one() -> MuxerConfig {
    let mut prog1 = MuxerProgramConfig::new(1, 0x1000);
    prog1.streams = vec![StreamSpec::Audio {
        pid: 0x1011,
        codec: AudioCodec::Mp2,
        language: None,
    }];
    prog1.stream_descriptors = vec![Vec::new()];
    let mut prog2 = MuxerProgramConfig::new(2, 0x1100);
    prog2.streams = vec![StreamSpec::Video {
        pid: 0x1111,
        codec: VideoCodec::H264,
    }];
    prog2.stream_descriptors = vec![Vec::new()];
    let mut cfg = MuxerConfig::default();
    cfg.programs = vec![prog1, prog2];
    cfg
}

/// Two-program config where the lone KLV stream lives in program-index 1:
/// - Program 1: video only at PID 0x1011.
/// - Program 2: audio (Mp2) at PID 0x1121 + one KLV stream at PID 0x1131.
///
/// Audio is added alongside KLV so PCR-fallback (`video > audio`)
/// resolves to the audio PID — KLV is never PCR-eligible (a KLV-only
/// program is rejected by `MuxerConfig::validate`; ETSI TR 101 290
/// §5.6.1 requires ≤100 ms between PCRs, KLV streams are sparse).
fn config_klv_in_program_one() -> MuxerConfig {
    let mut prog1 = MuxerProgramConfig::new(1, 0x1000);
    prog1.streams = vec![StreamSpec::Video {
        pid: 0x1011,
        codec: VideoCodec::H264,
    }];
    prog1.stream_descriptors = vec![Vec::new()];
    let mut prog2 = MuxerProgramConfig::new(2, 0x1100);
    prog2.streams = vec![
        StreamSpec::Audio {
            pid: 0x1121,
            codec: AudioCodec::Mp2,
            language: None,
        },
        StreamSpec::Klv {
            pid: 0x1131,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    // Pin PCR to the audio PID. The default fallback chain is
    // `video > klv > audio`, so with no video in this program
    // the auto-fallback would land on the KLV PID and trip
    // `MuxError::KlvPidUsedAsPcrPid` at validate time.
    prog2.pcr_pid = Some(0x1121);
    prog2.stream_descriptors = vec![Vec::new(), Vec::new()];
    let mut cfg = MuxerConfig::default();
    cfg.programs = vec![prog1, prog2];
    cfg
}

/// Drain all TS packets from the muxer into a flat byte buffer.
fn drain_all(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 188];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Count packets on a given PID in a flat 188-byte-chunk TS buffer.
fn count_packets_on_pid(data: &[u8], pid: u16) -> usize {
    data.chunks_exact(188)
        .filter(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == pid)
        .count()
}

#[test]
fn push_video_routes_to_correct_program_when_lone_video_in_program_one() {
    let mut muxer = Muxer::new(config_video_in_program_one()).unwrap();
    let nal = synthetic_nal::h264_au(200, true);

    muxer
        .push_video(&nal, Pts90khz::new(90_000), true)
        .expect("push_video must route correctly when lone video is in program-index 1");

    let out = drain_all(&mut muxer);
    let expected_pid = 0x1111u16; // program 2's video PID
    let bogus_pid = 0x1011u16; // program 1's audio PID — must NOT receive video
    assert!(
        count_packets_on_pid(&out, expected_pid) > 0,
        "video packets must appear on PID 0x{expected_pid:04X} (program 2's video PID)"
    );
    assert_eq!(
        count_packets_on_pid(&out, bogus_pid),
        0,
        "video must NOT be misrouted to PID 0x{bogus_pid:04X} (program 1's audio PID)"
    );
}

#[test]
fn push_klv_routes_to_correct_program_when_lone_klv_in_program_one() {
    let mut muxer = Muxer::new(config_klv_in_program_one()).unwrap();
    let klv = synthetic_nal::klv_blob(32);

    muxer
        .push_klv(&klv, Pts90khz::new(90_000), 0x00)
        .expect("push_klv must route correctly when lone KLV is in program-index 1");

    let out = drain_all(&mut muxer);
    let expected_pid = 0x1131u16; // program 2's KLV PID
    let bogus_pid = 0x1011u16; // program 1's video PID — must NOT receive KLV
    assert!(
        count_packets_on_pid(&out, expected_pid) > 0,
        "KLV packets must appear on PID 0x{expected_pid:04X} (program 2's KLV PID)"
    );
    assert_eq!(
        count_packets_on_pid(&out, bogus_pid),
        0,
        "KLV must NOT be misrouted to PID 0x{bogus_pid:04X} (program 1's video PID)"
    );
}
