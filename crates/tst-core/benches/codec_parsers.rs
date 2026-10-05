//! Codec parser micro-benchmarks.
//!
//! Six sub-benches across all four video parameter-set parsers and both audio
//! frame iterators:
//!
//! | bench name              | parser                         | input source                  |
//! |-------------------------|--------------------------------|-------------------------------|
//! | h264_parse_sps          | `codec::h264::parse_sps`       | real x264 High@4.0 fixture    |
//! | h265_parse_sps          | `codec::h265::parse_sps`       | real x265 Main@4.0 fixture    |
//! | h266_parse_sps          | `codec::h266::parse_sps`       | real vvenc Main10 fixture     |
//! | av1_parse_sequence_hdr  | `codec::av1::parse_sequence_header` | synthetic Main 320x240 fixture |
//! | mpegaudio_frame_iter    | `codec::mpegaudio::frames`     | 50 synthetic MP3 frames inline |
//! | adts_frame_iter         | `codec::aac::frames`           | 50 synthetic ADTS frames inline|
//!
//! ## Why these inputs
//!
//! Parameter-set parsers run once per IDR (cold path) — a single SPS parse
//! on a real fixture is the most representative measurement. Audio iterators
//! run every PES packet (warm path) — 50 frames (≈2 ×50 kB) exercises the
//! inner loop without saturating the CPU cache in the way millions of frames
//! would. Fifty frames is long enough to amortize iterator setup overhead and
//! short enough to give a stable measurement within criterion's default
//! sampling budget.
//!
//! ## Role
//!
//! These are tripwire benches: they record baseline throughput so that
//! refactors can be verified not to degrade hot paths. The video parsers are
//! expected to take 1–30 µs each (single-invocation); the audio iterators are
//! expected to complete 50 frames in 5–100 µs.
//!
//! Run: `cargo bench -p tst-core --bench codec_parsers`.
//! Quick mode (shorter warmup): add `-- --quick` at the end.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tst_core::codec::{aac, av1, h264, h265, h266, mpegaudio};

// ---------------------------------------------------------------------------
// Fixture bytes — parameter-set parsers
// ---------------------------------------------------------------------------

/// Real x264 High@4.0 1080p SPS RBSP, produced by the fixture generator.
/// Exercises profile/level/VUI/crop math in `h264::parse_sps`.
const H264_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h264/h264_1080p_high40_bt709_sps.bin");

/// Real x265 Main@4.0 1080p SPS RBSP (no RPS — x265 puts RPS in slice
/// headers with `repeat-headers=1`). Exercises PTL + VUI + conformance
/// window math in `h265::parse_sps`.
const H265_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");

/// Real vvenc Main10 320×240 SPS RBSP from the fixture generator.
/// Exercises the hand-rolled H.266 V4 §7.3.4.3 walk in `h266::parse_sps`.
const H266_SPS_RBSP: &[u8] =
    include_bytes!("../tests/fixtures/codec/h266/h266_320x240_main10_sps.bin");

/// Synthetic AV1 Sequence Header OBU payload (Main profile, 320×240, 8-bit
/// 4:2:0). These 10 bytes come from the fixture generator
/// (`gen-av1-fixtures`) and are the same bytes used by
/// `codec::av1::sequence_header` unit tests.
const AV1_SEQ_HDR: &[u8] =
    include_bytes!("../tests/fixtures/codec/av1/av1_320x240_main_seq_header.bin");

// ---------------------------------------------------------------------------
// Synthetic audio buffers — audio frame iterators
// ---------------------------------------------------------------------------

/// Build a buffer containing `count` back-to-back MPEG Audio (MP3) frames.
///
/// Each frame uses the 4-byte V1L3 128 kbps 44.1 kHz joint-stereo header
/// `[0xFF, 0xFB, 0x90, 0x40]` followed by zero-filled body bytes. The frame
/// length formula (ISO 11172-3 §2.4.2.3) gives 417 bytes at 128 kbps 44.1
/// kHz with no padding bit. This matches the header used in the
/// `codec::mpegaudio` unit tests — it is a real header byte pattern
/// produced by LAME and other encoders for this bitrate/rate combination.
fn make_mp3_buf(count: usize) -> Vec<u8> {
    // 4-byte sync word + parameter bytes: MPEG-1, Layer III, 128 kbps,
    // 44100 Hz, joint stereo, no CRC, no padding.
    const HEADER: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];
    // Frame length: 144 * 128_000 / 44100 = 417 bytes (no padding).
    const FRAME_LEN: usize = 417;
    let mut buf = Vec::with_capacity(count * FRAME_LEN);
    for _ in 0..count {
        buf.extend_from_slice(&HEADER);
        buf.extend(std::iter::repeat(0u8).take(FRAME_LEN - 4));
    }
    buf
}

/// Build a buffer containing `count` back-to-back ADTS AAC frames.
///
/// Each frame uses a 7-byte no-CRC ADTS header encoding:
///   - MPEG-2 ID bit
///   - AAC-LC profile (profile_ObjectType bits = 0b01 → profile index 1)
///   - sample_rate_index 4 → 44100 Hz (ISO 13818-7 Table 35)
///   - channel_configuration 2 → stereo
///   - aac_frame_length = total_len = `FRAME_LEN`
///   - adts_buffer_fullness = 0x7FF (VBR)
///   - num_raw_data_blocks_in_frame = 0 (1 raw data block)
///
/// This header layout matches the `build_frame` helper in the
/// `codec::aac` unit tests and is verified to parse cleanly by those tests.
fn make_adts_buf(count: usize) -> Vec<u8> {
    // Total frame length including the 7-byte header.
    const FRAME_LEN: u32 = 207; // 7-byte header + 200-byte body
    const FRAME_LEN_USIZE: usize = FRAME_LEN as usize;

    let mut buf = Vec::with_capacity(count * FRAME_LEN_USIZE);
    for _ in 0..count {
        // Encode the 7-byte no-CRC ADTS header.
        // Byte 0: syncword high (0xFF)
        // Byte 1: syncword low (0xF) | ID(1)=MPEG-2 | layer(2)=0 | protection_absent=1
        //         → 0b1111_1001 = 0xF9, but ID bit is bit 3 (0=MPEG-4, 1=MPEG-2)
        //         → 0xFF, 0b1111_0001 | (1<<3) | 1 = 0xFF, 0xF9
        // Following the same bit layout as build_frame(sample_rate_index=4, channel_config=2, total_len):
        let sample_rate_index: u8 = 4; // 44100 Hz
        let channel_config: u8 = 2; // stereo

        let mut h = vec![0u8; 7];
        h[0] = 0xFF;
        // ID=MPEG-2 (bit 3), layer=0, no CRC (protection_absent=1 in bit 0)
        h[1] = 0b1111_0000 | (1 << 3) | 1;
        // profile=AAC-LC (0b01 → stored as 0b01 in bits 7-6), sample_rate_index, channel_config MSB
        h[2] = (1 << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_config >> 2) & 1);
        // channel_config low 2 bits | frame_length bits 12-11
        h[3] = ((channel_config & 0b11) << 6) | (((FRAME_LEN >> 11) & 0b11) as u8);
        // frame_length bits 10-3
        h[4] = ((FRAME_LEN >> 3) & 0xFF) as u8;
        // frame_length bits 2-0 | buffer_fullness high 5 bits (0x7FF → 0b1_1111)
        h[5] = (((FRAME_LEN & 0b111) as u8) << 5) | 0b1_1111;
        // buffer_fullness low 6 bits | num_blocks=0
        h[6] = 0b11_1111 << 2;

        buf.extend_from_slice(&h);
        buf.extend(std::iter::repeat(0u8).take(FRAME_LEN_USIZE - 7));
    }
    buf
}

// ---------------------------------------------------------------------------
// Bench functions — parameter-set parsers (cold, one call per iteration)
// ---------------------------------------------------------------------------

fn bench_h264_parse_sps(c: &mut Criterion) {
    // H.264 uses the external `h264-reader 0.8` crate. `parse_sps` receives
    // raw RBSP bytes (NAL header byte stripped; emulation-prevention bytes
    // still present — `h264-reader` removes them internally via ByteReader).
    c.bench_function("h264_parse_sps", |b| {
        b.iter(|| {
            let sps = h264::parse_sps(black_box(H264_SPS_RBSP)).expect("parse H.264 SPS");
            let _ = black_box(sps);
        })
    });
}

fn bench_h265_parse_sps(c: &mut Criterion) {
    // H.265 uses the hand-rolled parser in `codec::h265::sps`. Unlike H.264,
    // the parser directly calls an internal `Bitreader` on the RBSP; VUI and
    // PTL parsing are interleaved with the SPS walk, making this a non-trivial
    // path even for a real-world fixture with no short-term RPS entries.
    c.bench_function("h265_parse_sps", |b| {
        b.iter(|| {
            let sps = h265::parse_sps(black_box(H265_SPS_RBSP)).expect("parse H.265 SPS");
            let _ = black_box(sps);
        })
    });
}

fn bench_h266_parse_sps(c: &mut Criterion) {
    // H.266 / VVC SPS parser: hand-rolled per H.266 V4 §7.3.4.3 + §E.2.1
    // VUI annex. Notably more complex than H.265 — the SPS includes per-sublayer
    // PTL, general constraints flags, and a longer set of sub-picture parameters.
    c.bench_function("h266_parse_sps", |b| {
        b.iter(|| {
            let sps = h266::parse_sps(black_box(H266_SPS_RBSP)).expect("parse H.266 SPS");
            let _ = black_box(sps);
        })
    });
}

fn bench_av1_parse_sequence_header(c: &mut Criterion) {
    // AV1 Sequence Header OBU payload (no OBU framing — caller strips the OBU
    // header + LEB128 size before passing to `parse_sequence_header`). The
    // fixture is 10 bytes: a minimal Main-profile 320×240 8-bit 4:2:0 SH.
    // Though tiny, this exercises all the bitfield reads in §5.5.1.
    c.bench_function("av1_parse_sequence_header", |b| {
        b.iter(|| {
            let sh =
                av1::parse_sequence_header(black_box(AV1_SEQ_HDR)).expect("parse AV1 seq header");
            let _ = black_box(sh);
        })
    });
}

// ---------------------------------------------------------------------------
// Bench functions — audio frame iterators (warm, N frames per iteration)
// ---------------------------------------------------------------------------

/// Number of frames per audio bench iteration. 50 frames is long enough to
/// amortize iterator dispatch overhead; short enough to keep each sample under
/// criterion's default 5-second total budget without needing `--quick`.
const AUDIO_FRAMES: usize = 50;

fn bench_mpegaudio_frame_iter(c: &mut Criterion) {
    // Build the buffer once outside the bench loop — we're measuring the
    // iterator, not allocation. `Frames` is a lazy stateless iterator: it
    // advances a cursor through the slice on each call to `next()`, parses
    // the 4-byte header, and returns a zero-copy `Frame<'_>` pointing into
    // the original slice.
    let buf = make_mp3_buf(AUDIO_FRAMES);
    c.bench_function("mpegaudio_frame_iter_50_frames", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for result in mpegaudio::frames(black_box(&buf)) {
                black_box(result.expect("frame should parse"));
                count += 1;
            }
            // Sanity: ensure the compiler doesn't dead-code-eliminate the loop.
            assert_eq!(count, AUDIO_FRAMES);
        })
    });
}

fn bench_adts_frame_iter(c: &mut Criterion) {
    // Same pattern as the MPEG audio bench. ADTS headers are 7 bytes (no CRC
    // mode); the parser reads syncword, profile, sample rate index, channel
    // configuration, and frame length — more fields than the MPEG-audio header
    // but still O(1) per frame.
    let buf = make_adts_buf(AUDIO_FRAMES);
    c.bench_function("adts_frame_iter_50_frames", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for result in aac::frames(black_box(&buf)) {
                black_box(result.expect("ADTS frame should parse"));
                count += 1;
            }
            assert_eq!(count, AUDIO_FRAMES);
        })
    });
}

criterion_group!(
    codec_parser_benches,
    bench_h264_parse_sps,
    bench_h265_parse_sps,
    bench_h266_parse_sps,
    bench_av1_parse_sequence_header,
    bench_mpegaudio_frame_iter,
    bench_adts_frame_iter,
);
criterion_main!(codec_parser_benches);
