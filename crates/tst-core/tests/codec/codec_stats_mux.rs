//! Integration coverage for `Muxer::stream_codec_stats`.
//!
//! Five tests covering the sender-side accessor:
//! * `Video` variant with `nals_or_obus` + `random_access_aus` bumps
//!   driven by `push_video`
//! * `key_frame=false` exercising the non-RA branch
//! * `Klv` variant counting one record per `push_klv` call
//! * `Audio` variant counting AAC ADTS frames via the
//!   `codec::aac::frames` iterator
//! * `Some(Unknown)` for an AC-3 stream — no codec counter is
//!   materialized (the AAC/MP2 dispatch returns 0 for AC-3),
//!   so the accessor falls back to the `per_stream.contains_key` path
//!   and returns Unknown.
//!
//! The Muxer-driven shape mirrors `codec_stats.rs` (Demuxer side). The
//! `build_adts_frame` helper is copy-pasted from that file; the duplication
//! is small enough to live with rather than promoting
//! to `tests/common/mod.rs` (which currently only holds the unrelated
//! `imapb_tol` helper).

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec as MuxAudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder,
    VideoCodec as MuxVideoCodec,
};
use tst_core::mpegts::stats::StreamCodecStats;

// --- Helpers ----------------------------------------------------------------

/// Minimal H.264 AU: AUD (nal_type=9) + IDR (nal_type=5). Same shape as
/// the helper in `codec_stats.rs` and `mpegts_demux.rs::build_minimal_h264_au`.
fn build_minimal_h264_au() -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC,
    ]
}

/// Minimal SMPTE-UL-shaped KLV blob — same shape as
/// `codec_stats.rs::build_dummy_klv`. Only what matters for routing
/// is the SMPTE UL prefix (`06 0E 2B 34`).
fn build_dummy_klv() -> Vec<u8> {
    let body = [2u8, 8, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut out = Vec::with_capacity(17 + body.len());
    out.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    out.push(body.len() as u8); // BER short-form length
    out.extend_from_slice(&body);
    out
}

/// Build a single ADTS frame — copy of `codec_stats.rs::build_adts_frame`.
/// Header pattern matches `crates/tst-core/src/codec/aac/mod.rs`'s
/// `build_frame` test helper: MPEG-2 ID, no CRC, AAC-LC, 1 block.
fn build_adts_frame(sample_rate_index: u8, channel_config: u8, total_len: u32) -> Vec<u8> {
    let mut h = vec![0u8; 7];
    h[0] = 0xFF;
    h[1] = 0b1111_0000 | (1 << 3) | 1; // ID=MPEG-2, layer=0, no CRC
    h[2] = (1 << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_config >> 2) & 1);
    h[3] = ((channel_config & 0b11) << 6) | (((total_len >> 11) & 0b11) as u8);
    h[4] = ((total_len >> 3) & 0xFF) as u8;
    h[5] = (((total_len & 0b111) as u8) << 5) | 0b1_1111;
    h[6] = 0b11_1111 << 2;
    let pad = total_len as usize - 7;
    let mut out = h;
    out.extend(std::iter::repeat(0u8).take(pad));
    out
}

/// Two back-to-back ADTS frames in a single buffer — what Test 4 pushes.
fn build_two_adts_frames() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(build_adts_frame(4, 2, 200));
    out.extend(build_adts_frame(4, 2, 200));
    out
}

/// Minimal MuxerConfig: 1 program + 1 video stream on `pid` with `codec`.
fn build_one_video_muxer(codec: MuxVideoCodec, pid: u16) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(pid, codec);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Muxer::new(b.build().unwrap()).unwrap()
}

/// Minimal MuxerConfig for a KLV-only test: 1 program with H.264 video
/// (to anchor PCR + force PMT emission) + 1 SynchronousMetadata KLV
/// stream on `pid`. Mirrors `codec_stats.rs::stream_codec_stats_klv_increments_records`.
fn build_one_klv_muxer(pid: u16) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, MuxVideoCodec::H264);
    prog.add_klv(pid, KlvStreamType::SynchronousMetadata, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Muxer::new(b.build().unwrap()).unwrap()
}

/// Minimal MuxerConfig for an audio test: 1 program with H.264 video
/// (to anchor PCR + force PMT emission) + 1 audio stream on `pid` with
/// the requested codec.
fn build_one_audio_muxer(codec: MuxAudioCodec, pid: u16) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, MuxVideoCodec::H264);
    prog.add_audio(pid, codec);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Muxer::new(b.build().unwrap()).unwrap()
}

// --- Tests ------------------------------------------------------------------

#[test]
fn h264_push_aud_plus_idr_counts_2_nals_1_ra() {
    // Single video stream so the unqualified `push_video` shorthand
    // resolves to PID 0x100 unambiguously. AUD + IDR ⇒ 2 NALs;
    // key_frame=true ⇒ 1 random-access AU.
    let mut mux = build_one_video_muxer(MuxVideoCodec::H264, 0x100);
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();

    match mux.stream_codec_stats(0x100) {
        Some(StreamCodecStats::Video {
            nals_or_obus,
            random_access_aus,
            ..
        }) => {
            assert_eq!(nals_or_obus, 2, "AUD + IDR ⇒ 2 NALs");
            assert_eq!(random_access_aus, 1, "key_frame=true ⇒ 1 RA AU");
        }
        other => panic!("expected Some(Video {{..}}), got {:?}", other),
    }
}

#[test]
fn h264_push_non_key_frame_does_not_bump_ra() {
    // One non-key NAL pushed with key_frame=false. Just an IDR slice
    // start code — the count_nal_units helper sees one Annex B start
    // code so nals_or_obus=1. random_access_aus stays at 0 because
    // key_frame=false (RA delta = u64::from(key_frame)).
    let mut mux = build_one_video_muxer(MuxVideoCodec::H264, 0x100);
    let one_nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC];
    mux.push_video(one_nal, Pts90khz::new(90_000), false)
        .unwrap();

    match mux.stream_codec_stats(0x100) {
        Some(StreamCodecStats::Video {
            nals_or_obus,
            random_access_aus,
            ..
        }) => {
            assert_eq!(nals_or_obus, 1, "one NAL ⇒ nals_or_obus=1");
            assert_eq!(
                random_access_aus, 0,
                "key_frame=false ⇒ random_access_aus stays at 0"
            );
        }
        other => panic!("expected Some(Video {{..}}), got {:?}", other),
    }
}

#[test]
fn klv_push_three_records_counts_three() {
    // Three push_klv calls on the same SynchronousMetadata KLV stream.
    // The muxer's contract is one record per call
    // (`bump_klv_counters(.., 1)`), so records=3.
    let mut mux = build_one_klv_muxer(0x200);
    // Anchor video push so the muxer has something to PCR on; not
    // strictly required for the counter bump but matches codec_stats.rs.
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let klv = build_dummy_klv();
    mux.push_klv(&klv, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(&klv, Pts90khz::new(93_000), 0x00).unwrap();
    mux.push_klv(&klv, Pts90khz::new(96_000), 0x00).unwrap();

    match mux.stream_codec_stats(0x200) {
        Some(StreamCodecStats::Klv { records, .. }) => {
            assert_eq!(records, 3, "3 push_klv calls ⇒ records=3");
        }
        other => panic!("expected Some(Klv {{ records: 3 }}), got {:?}", other),
    }
}

#[test]
fn aac_push_two_frames_counts_two() {
    // Two ADTS frames concatenated in one push_audio call. The
    // dispatch uses `codec::aac::frames(...).filter_map(Result::ok).count()`
    // which sees 2 valid frames ⇒ frames=2.
    let mut mux = build_one_audio_muxer(MuxAudioCodec::Aac, 0x300);
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_audio(&build_two_adts_frames(), Pts90khz::new(90_000))
        .unwrap();

    match mux.stream_codec_stats(0x300) {
        Some(StreamCodecStats::Audio { frames, .. }) => {
            assert_eq!(frames, 2, "2 ADTS frames ⇒ frames=2");
        }
        other => panic!("expected Some(Audio {{ frames: 2 }}), got {:?}", other),
    }
}

#[test]
fn ac3_push_audio_returns_unknown() {
    // AC-3 stream configured ⇒ per_stream has PID 0x300 (eager
    // population at config time). push_audio with AC-3 takes the
    // `Ac3 => 0` arm in the dispatch, so bump_audio_counters is
    // never called and stream_codec_counters has no entry. The
    // accessor falls back to per_stream.contains_key ⇒ Some(Unknown).
    //
    // We push raw bytes (not a valid AC-3 frame); the muxer doesn't
    // parse audio payloads — it just frames them into PES.
    let mut mux = build_one_audio_muxer(MuxAudioCodec::Ac3, 0x300);
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    mux.push_audio(
        &[0x0Bu8, 0x77, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        Pts90khz::new(90_000),
    )
    .unwrap();

    assert_eq!(
        mux.stream_codec_stats(0x300),
        Some(StreamCodecStats::Unknown),
        "AC-3 PID is configured (per_stream eager) but no codec counter \
         materializes ⇒ Some(Unknown)"
    );
}
