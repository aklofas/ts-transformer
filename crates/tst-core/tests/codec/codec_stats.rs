//! Integration coverage for `Demuxer::stream_codec_stats`.
//!
//! Six tests cover the receiver-side accessor's full state space:
//! * `None` for never-seen PIDs
//! * Typed `Video` / `Klv` / `Audio` variants for each codec family that
//!   has a counter in v1
//! * `Some(Unknown)` for known-but-uncounted PIDs (subtitles)
//! * `reset_stats` CLEARS codec entries (a reset PID returns
//!   `None`, not `Some(0-valued variant)`)
//!
//! All fixtures use the in-crate `mpegts::mux::Muxer` to produce TS bytes
//! deterministically — same pattern as `mpegts_demux.rs` / `mpegts_demux_audio.rs`
//! / `mpegts_demux_subtitle.rs`. Hand-rolling raw PAT/PMT/PES bytes would
//! duplicate dozens of LoC of muxer logic for no real coverage gain.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video,
};
use tst_core::mpegts::mux::{
    AudioCodec as MuxAudioCodec, Av1CarriageMode, KlvStreamType, Muxer, MuxerConfig,
    MuxerProgramConfigBuilder, SubtitleCodec as MuxSubtitleCodec, VideoCodec as MuxVideoCodec,
};
use tst_core::mpegts::stats::StreamCodecStats;
use tst_core::shared::SharedBytes;

// --- Helpers ----------------------------------------------------------------

/// Drain every queued packet from the muxer into a single Vec.
fn drain_all(mux: &mut Muxer) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = vec![0u8; 188 * 256];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    all
}

/// Feed `bytes` to a fresh Demuxer and drain all events, returning the
/// demuxer for follow-on `stream_codec_stats` / `reset_stats` queries.
fn demux_drain(bytes: &[u8]) -> Demuxer {
    let mut d = Demuxer::new();
    d.feed(bytes).unwrap();
    // Drain partial-PES buffered by unbounded video PES (PES_packet_length=0).
    // Live-receive loops do this at TransportError::Closed; here we call it
    // explicitly because the test produces a finite byte stream.
    d.flush();
    while d.next_event().is_some() {}
    d
}

/// Minimal H.264 AU: AUD (nal_type=9) + IDR (nal_type=5). Same shape as
/// `mpegts_demux.rs::build_minimal_h264_au`.
fn build_minimal_h264_au() -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC,
    ]
}

/// Build a minimal SMPTE-UL-shaped KLV blob. Same shape as
/// `mpegts_demux.rs::build_dummy_klv`. Only what matters for routing is the
/// SMPTE UL prefix (`06 0E 2B 34`); the demuxer treats this as one record.
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

/// Build a single ADTS frame with `total_len` bytes (header + body). The
/// header pattern mirrors `crates/tst-core/src/codec/aac/mod.rs`'s
/// `build_frame` test helper — MPEG-2 ID, no CRC, AAC-LC profile, 1 block.
/// We need 4 of these back-to-back so the demuxer's
/// `codec::aac::frames(...).filter_map(Result::ok).count()` returns exactly 4.
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

// --- Tests ------------------------------------------------------------------

#[test]
fn stream_codec_stats_returns_none_for_never_seen_pid() {
    // Fresh demuxer, no feeds. Any PID query returns None — the
    // 3-state accessor distinguishes "never seen" from
    // "seen but uncounted" from "seen and counted".
    let demux = Demuxer::new();
    assert_eq!(demux.stream_codec_stats(0x9999), None);
}

#[test]
fn stream_codec_stats_h264_idr_increments_video_counters() {
    // Mux: H.264 video on PID 0x100, single IDR AU (AUD + IDR slice).
    // `key_frame=true` on push_video causes the muxer to set the
    // TS adaptation-field RAI bit on the PES_start packet, which the
    // demuxer surfaces via `SamplePayload::Video::random_access_indicator`
    // and counts in `random_access_aus`.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let bytes = drain_all(&mut mux);

    // Raw-first: the demuxer counts `random_access_aus` (from the PES_start
    // RAI bit) but no longer counts `nals_or_obus` — NAL splitting is the
    // opt-in `split_video` call, so the demux-side counter stays 0. The "2
    // NALs (AUD + IDR)" intent is preserved by splitting the raw AU below.
    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    d.flush();
    let mut raw_au: Option<SharedBytes> = None;
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = ev
        {
            raw_au = Some(raw);
        }
    }
    match d.stream_codec_stats(0x100) {
        Some(StreamCodecStats::Video {
            nals_or_obus,
            random_access_aus,
            ..
        }) => {
            assert_eq!(
                nals_or_obus, 0,
                "raw-first demuxer no longer counts NALs (split is opt-in)"
            );
            assert_eq!(
                random_access_aus, 1,
                "RAI bit on key-frame PES_start should produce 1 RA AU"
            );
        }
        other => panic!("expected Some(Video {{..}}), got {:?}", other),
    }
    // The 2 NALs (AUD + IDR) are still recoverable via the opt-in split.
    let raw = raw_au.expect("expected a Video Sample event");
    let VideoPayload::Nals(nals) =
        split_video(&raw, VideoCodec::H264, Av1CarriageMode::Mpeg2TsBinding).0
    else {
        panic!("expected NALs from split_video");
    };
    assert!(
        nals.len() >= 2,
        "expected ≥ 2 NALs (AUD + IDR), got {}",
        nals.len()
    );
}

#[test]
fn stream_codec_stats_klv_increments_records() {
    // Mux: SynchronousMetadata KLV on PID 0x200, 3 separate push_klv calls.
    // SynchronousMetadata requires carries_pts=true (validated at build time).
    // Each push_klv → 1 PES → 1 Metadata event → bump_klv_counters(+1).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x200, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    // Video push to anchor PCR + force PMT emission early.
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let klv = build_dummy_klv();
    mux.push_klv(&klv, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(&klv, Pts90khz::new(93_000), 0x00).unwrap();
    mux.push_klv(&klv, Pts90khz::new(96_000), 0x00).unwrap();
    let bytes = drain_all(&mut mux);

    let demux = demux_drain(&bytes);
    match demux.stream_codec_stats(0x200) {
        Some(StreamCodecStats::Klv { records, .. }) => {
            assert_eq!(records, 3, "3 push_klv calls ⇒ 3 records");
        }
        other => panic!("expected Some(Klv {{ records: 3 }}), got {:?}", other),
    }
}

#[test]
fn stream_codec_stats_aac_adts_increments_frames() {
    // Mux: AAC audio on PID 0x300, 1 PES carrying 4 ADTS frames back-to-back.
    // The demuxer iterates ADTS frames via `codec::aac::frames` per
    // emitted Audio Sample and bumps `frames` by the iterator's Ok count.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_audio(0x300, MuxAudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let mut adts = Vec::new();
    for _ in 0..4 {
        adts.extend(build_adts_frame(4, 2, 200));
    }
    mux.push_audio(&adts, Pts90khz::new(90_000)).unwrap();
    let bytes = drain_all(&mut mux);

    let demux = demux_drain(&bytes);
    match demux.stream_codec_stats(0x300) {
        Some(StreamCodecStats::Audio { frames, .. }) => {
            assert_eq!(frames, 4, "4 ADTS frames concatenated ⇒ frames=4");
        }
        other => panic!("expected Some(Audio {{ frames: 4 }}), got {:?}", other),
    }
}

#[test]
fn stream_codec_stats_returns_unknown_for_subtitle_pid() {
    // Mux: WebVTT-in-TS subtitle on PID 0x400 (alongside H.264 video that
    // the mux config requires). Subtitles populate `stats_per_stream` but
    // not `stream_codec_counters`, so the accessor's fallback path returns
    // `Some(StreamCodecStats::Unknown)`.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_subtitle(0x400, MuxSubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let sub_handle = mux.subtitle_handles()[0];
    mux.push_subtitle_to(sub_handle, Pts90khz::new(90_000), b"WEBVTT\nx-cue\n")
        .unwrap();
    let bytes = drain_all(&mut mux);

    let demux = demux_drain(&bytes);
    assert_eq!(
        demux.stream_codec_stats(0x400),
        Some(StreamCodecStats::Unknown),
        "subtitle PID is known to the site but has no counter family"
    );
}

#[test]
fn reset_stats_clears_codec_counters() {
    // Feed an H.264 fixture so PID 0x100 ends up with a Video counter.
    // Then reset_stats(). The counter is CLEARED
    // (not zeroed) — so the post-reset query returns None.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&build_minimal_h264_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let bytes = drain_all(&mut mux);

    let mut demux = demux_drain(&bytes);
    // Sanity: before reset, PID 0x100 has a typed Video counter.
    assert!(
        matches!(
            demux.stream_codec_stats(0x100),
            Some(StreamCodecStats::Video { .. })
        ),
        "pre-reset: PID 0x100 should have a Video counter"
    );

    demux.reset_stats();

    // Post-reset: the PID is back to None — entries are CLEARED, not zeroed.
    assert_eq!(
        demux.stream_codec_stats(0x100),
        None,
        "post-reset: PID 0x100 should be None (counter cleared, not zeroed)"
    );
}
