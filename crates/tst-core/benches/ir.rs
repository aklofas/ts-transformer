//! Fixed-iteration workloads for instruction counting.
//!
//! Criterion sizes its iteration count by wall-clock time, so its benches
//! cannot be compared under callgrind, and their wall-clock numbers move by
//! up to a fifth between two builds of identical code (the linker places a
//! hot loop differently). Each workload here repeats one operation a fixed
//! number of times; `scripts/dev/bench-ir.sh` builds this file against two
//! commits, runs every workload under `valgrind --tool=callgrind` and
//! compares instructions per iteration.
//!
//! ```text
//! ir list                               one "<workload> <iterations>" line each
//! ir gen-stream <file>                  write the demux input stream
//! ir run <workload> <iterations> [stream-file]
//! ```
//!
//! This file must stay self-contained and keep to long-lived API: the script
//! compiles the newer commit's copy against the older commit too. `cargo
//! bench` runs it with flags of its own; any invocation other than the three
//! above prints a pointer to the script and succeeds.

use std::hint::black_box;
use tst_core::codec::{aac, av1, h264, h265, h266, mpegaudio};
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use tst_core::klv::st0601::UasDatalinkLs;
use tst_core::klv::st0903::{VTargetPack, VmtiLs};
use tst_core::klv::{st0102, st0601, st0903};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

const H264_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h264/h264_1080p_high40_bt709_sps.bin");
const H265_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");
const H266_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h266/h266_320x240_main10_sps.bin");
const AV1_SEQ_HDR: &[u8] =
    include_bytes!("../tests/fixtures/codec/av1/av1_320x240_main_seq_header.bin");

/// Workload names with the iteration count the script uses. The counts keep
/// every callgrind run to a few seconds and make process start-up (about
/// 150 000 instructions) less than 0.1 % of the total.
const WORKLOADS: &[(&str, usize)] = &[
    ("st0601_decode", 200_000),
    ("st0102_decode", 400_000),
    ("st0903_decode", 200_000),
    ("st0601_encode", 30_000),
    ("st0601_encoded_len", 200_000),
    ("h264_parse_sps", 150_000),
    ("h265_parse_sps", 100_000),
    ("h266_parse_sps", 200_000),
    ("av1_parse_sequence_header", 400_000),
    ("mpegaudio_frames", 30_000),
    ("adts_frames", 30_000),
    ("demux_feed_per_188", 300),
    ("demux_feed_whole", 300),
    ("muxer_new", 100_000),
    ("mux_push_video_3k", 30_000),
    ("mux_push_klv_1k", 300_000),
];

fn st0601_decode_input() -> Vec<u8> {
    let record = UasDatalinkLs {
        platform_designation: Some("TestAircraft".into()),
        image_source_sensor: Some("EO".into()),
        timestamp_us: Some(1_700_000_000_000_000),
        platform_heading_deg: Some(180.0),
        platform_pitch_deg: Some(-2.5),
        platform_roll_deg: Some(0.5),
        sensor_lat_deg: Some(33.5),
        sensor_lon_deg: Some(-112.0),
        sensor_alt_m: Some(1500.0),
        sensor_hfov_deg: Some(15.0),
        sensor_vfov_deg: Some(8.5),
        slant_range_m: Some(2000.0),
        frame_center_lat_deg: Some(33.49),
        frame_center_lon_deg: Some(-111.99),
        ..UasDatalinkLs::default()
    };
    st0601::encode_to_vec(&record).expect("encode st0601")
}

fn st0601_encode_input() -> UasDatalinkLs {
    UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        mission_id: Some("MISSION".into()),
        platform_tail_number: Some("N12345".into()),
        platform_designation: Some("TestAircraft".into()),
        image_source_sensor: Some("EO".into()),
        platform_heading_deg: Some(180.0),
        platform_pitch_deg: Some(-2.5),
        platform_roll_deg: Some(0.5),
        sensor_lat_deg: Some(33.5),
        sensor_lon_deg: Some(-112.0),
        sensor_alt_m: Some(1500.0),
        sensor_hfov_deg: Some(15.0),
        sensor_vfov_deg: Some(8.5),
        slant_range_m: Some(2000.0),
        frame_center_lat_deg: Some(33.49),
        frame_center_lon_deg: Some(-111.99),
        altitude_agl_m: Some(1200.0),
        radar_altimeter_m: Some(1190.0),
        platform_course_angle_deg: Some(180.0),
        zoom_percentage: Some(50.0),
        ..UasDatalinkLs::default()
    }
}

fn st0102_decode_input() -> Vec<u8> {
    let record = st0102::SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".into()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".into()),
        version: Some(12),
        ..Default::default()
    };
    st0102::encode_to_vec(&record).expect("encode st0102")
}

fn st0903_decode_input() -> Vec<u8> {
    let target = |id: u64, row: u64, col: u64| VTargetPack {
        target_id: id,
        centroid_pix_row: Some(row),
        centroid_pix_col: Some(col),
        priority: Some(id as u8),
        confidence_level: Some(90),
        ..Default::default()
    };
    let ls = VmtiLs {
        version_number: Some(6),
        total_targets_in_frame: Some(3),
        num_targets_reported: Some(3),
        frame_width: Some(1920),
        frame_height: Some(1080),
        targets: vec![
            target(1, 540, 960),
            target(2, 300, 400),
            target(3, 700, 1500),
        ],
        ..Default::default()
    };
    st0903::encode_to_vec(&ls).expect("encode st0903")
}

/// 50 back-to-back MPEG-1 Layer III frames (128 kbit/s, 44.1 kHz: 417 bytes).
fn mp3_frames() -> Vec<u8> {
    let mut buf = Vec::new();
    for _ in 0..50 {
        buf.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x40]);
        buf.extend(std::iter::repeat_n(0u8, 417 - 4));
    }
    buf
}

/// 50 back-to-back ADTS frames (AAC-LC, 44.1 kHz, stereo, 207 bytes, no CRC).
fn adts_frames() -> Vec<u8> {
    const FRAME_LEN: u32 = 207;
    let (sample_rate_index, channels): (u8, u8) = (4, 2);
    let header = [
        0xFF,
        0b1111_1001,
        (1 << 6) | (sample_rate_index << 2) | (channels >> 2),
        ((channels & 0b11) << 6) | ((FRAME_LEN >> 11) & 0b11) as u8,
        ((FRAME_LEN >> 3) & 0xFF) as u8,
        (((FRAME_LEN & 0b111) as u8) << 5) | 0b1_1111,
        0b11_1111 << 2,
    ];
    let mut buf = Vec::new();
    for _ in 0..50 {
        buf.extend_from_slice(&header);
        buf.extend(std::iter::repeat_n(0u8, FRAME_LEN as usize - header.len()));
    }
    buf
}

/// An Annex-B access unit of `size` payload bytes.
fn access_unit(size: usize, key: bool) -> Vec<u8> {
    let mut au = vec![0x00, 0x00, 0x00, 0x01, if key { 0x65 } else { 0x41 }];
    au.resize(5 + size, 0xA5);
    au
}

fn drain(mux: &mut Muxer, buf: &mut [u8]) {
    loop {
        let n = mux.pull(buf);
        if n == 0 {
            break;
        }
        black_box(&buf[..n]);
    }
}

/// About 1 800 TS packets: 50 video frames (one 50 kB key frame, then 5 kB)
/// with a 200-byte KLV unit each.
fn demux_stream() -> Vec<u8> {
    let mut cfg = MuxerConfig::default();
    cfg.buffer_packets = 200_000;
    let mut mux = Muxer::new(cfg).expect("muxer");
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    for i in 0..50i64 {
        let au = access_unit(if i == 0 { 50_000 } else { 5_000 }, i == 0);
        mux.push_video(&au, Pts90khz::new(i * 3000), i == 0)
            .expect("push video");
        mux.push_klv(&[0x42u8; 200], Pts90khz::new(i * 3000), 0x00)
            .expect("push klv");
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
    out
}

fn run(workload: &str, iters: usize, stream_file: Option<&str>) {
    // The demux workloads read the stream from a file when one is given, so
    // two builds can be handed the same bytes.
    let stream = || match stream_file {
        Some(path) => std::fs::read(path).expect("read the stream file"),
        None => demux_stream(),
    };
    match workload {
        "st0601_decode" => {
            let input = st0601_decode_input();
            for _ in 0..iters {
                let _ = black_box(st0601::decode(black_box(&input)).expect("decode"));
            }
        }
        "st0102_decode" => {
            let input = st0102_decode_input();
            for _ in 0..iters {
                let _ = black_box(st0102::decode(black_box(&input)).expect("decode"));
            }
        }
        "st0903_decode" => {
            let input = st0903_decode_input();
            for _ in 0..iters {
                let _ = black_box(st0903::decode(black_box(&input)).expect("decode"));
            }
        }
        "st0601_encode" => {
            let record = st0601_encode_input();
            for _ in 0..iters {
                let _ = black_box(st0601::encode_to_vec(black_box(&record)).expect("encode"));
            }
        }
        "st0601_encoded_len" => {
            let record = st0601_encode_input();
            for _ in 0..iters {
                let _ = black_box(st0601::encoded_len(black_box(&record)));
            }
        }
        "h264_parse_sps" => {
            for _ in 0..iters {
                let _ = black_box(h264::parse_sps(black_box(H264_SPS_RBSP)).expect("sps"));
            }
        }
        "h265_parse_sps" => {
            for _ in 0..iters {
                let _ = black_box(h265::parse_sps(black_box(H265_SPS_RBSP)).expect("sps"));
            }
        }
        "h266_parse_sps" => {
            for _ in 0..iters {
                let _ = black_box(h266::parse_sps(black_box(H266_SPS_RBSP)).expect("sps"));
            }
        }
        "av1_parse_sequence_header" => {
            for _ in 0..iters {
                let _ = black_box(
                    av1::parse_sequence_header(black_box(AV1_SEQ_HDR)).expect("sequence header"),
                );
            }
        }
        "mpegaudio_frames" => {
            let buf = mp3_frames();
            for _ in 0..iters {
                for frame in mpegaudio::frames(black_box(&buf)) {
                    let _ = black_box(frame.expect("frame"));
                }
            }
        }
        "adts_frames" => {
            let buf = adts_frames();
            for _ in 0..iters {
                for frame in aac::frames(black_box(&buf)) {
                    let _ = black_box(frame.expect("frame"));
                }
            }
        }
        "demux_feed_per_188" => {
            let stream = stream();
            for _ in 0..iters {
                let mut demux = Demuxer::new();
                for packet in stream.chunks_exact(188) {
                    let packet: &[u8; 188] = packet.try_into().expect("188 bytes");
                    demux.feed_aligned(black_box(packet)).expect("feed");
                    while let Some(event) = demux.next_event() {
                        black_box(event);
                    }
                }
            }
        }
        "demux_feed_whole" => {
            let stream = stream();
            for _ in 0..iters {
                let mut demux = Demuxer::new();
                demux.feed(black_box(&stream)).expect("feed");
                while let Some(event) = demux.next_event() {
                    black_box(event);
                }
            }
        }
        "muxer_new" => {
            for _ in 0..iters {
                let _ = black_box(Muxer::new(MuxerConfig::default()).expect("muxer"));
            }
        }
        // The two push workloads keep one muxer for the whole run, so they
        // count the push and the drain and nothing of the construction.
        "mux_push_video_3k" => {
            let au = access_unit(3000, false);
            let mut mux = Muxer::new(MuxerConfig::default()).expect("muxer");
            let mut buf = [0u8; 1316];
            for i in 0..iters {
                mux.push_video(black_box(&au), Pts90khz::new(i as i64 * 3000), false)
                    .expect("push video");
                drain(&mut mux, &mut buf);
            }
        }
        "mux_push_klv_1k" => {
            let klv = [0xC3u8; 1024];
            let mut mux = Muxer::new(MuxerConfig::default()).expect("muxer");
            let mut buf = [0u8; 1316];
            mux.push_video(&access_unit(500, true), Pts90khz::new(0), true)
                .expect("push video");
            for i in 0..iters {
                mux.push_klv(black_box(&klv), Pts90khz::new(i as i64 * 3000), 0x00)
                    .expect("push klv");
                drain(&mut mux, &mut buf);
            }
        }
        other => {
            eprintln!("ir: unknown workload {other}");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1), arg(2)) {
        (Some("list"), None, None) => {
            for (name, iters) in WORKLOADS {
                println!("{name} {iters}");
            }
        }
        (Some("gen-stream"), Some(path), None) => {
            std::fs::write(path, demux_stream()).expect("write the stream file");
        }
        (Some("run"), Some(workload), Some(iters)) => {
            let iters = iters.parse().expect("iterations is a number");
            run(workload, iters, arg(3));
        }
        _ => println!("ir: instruction-count workloads, driven by scripts/dev/bench-ir.sh"),
    }
}
