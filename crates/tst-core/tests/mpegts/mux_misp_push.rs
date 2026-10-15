//! Integration tests for `Muxer::push_video_misp_to` and
//! `push_video_misp_to_with_dts` — ST 0604 MISP timestamp SEI splice on the
//! mux path.

use crate::common::mux_demux::drain_mux as drain_all;
use tst_core::codec::misp_time::{self, MispTimestamp};
use tst_core::error::{MuxError, MuxErrorKind};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    Demuxer, DemuxerConfig, VideoCodec as DemuxVideoCodec,
    event::{DemuxEvent, SamplePayload},
    split_video,
};
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

// ─────────────────────────────────────────────────────────────────────────────
// Minimal Annex-B AU builders reused across tests
// ─────────────────────────────────────────────────────────────────────────────

/// AUD + SPS + PPS + IDR slice — the canonical "full keyframe AU" for H.264.
fn h264_keyframe_au() -> Vec<u8> {
    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let nri: u8 = if nal_type == 5 { 0b11 } else { 0b00 };
        let mut v = vec![0x00, 0x00, 0x00, 0x01, (nri << 5) | nal_type];
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(nal(9, &[0xF0])); // AUD
    au.extend(nal(7, &[0x42, 0xC0, 0x28, 0xD9])); // SPS
    au.extend(nal(8, &[0xCE, 0x38, 0x80])); // PPS
    au.extend(nal(5, &[0x88, 0x84, 0x0A, 0x7C, 0x11])); // IDR
    au
}

/// AUD-only AU with no VCL — used for the NoVclNal rejection test.
fn h264_no_vcl_au() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 9u8, 0xF0] // AUD only (nal_ref_idc=0 | nal_unit_type=9)
}

// ─────────────────────────────────────────────────────────────────────────────
// Round-trip + NAL-ordering test
// ─────────────────────────────────────────────────────────────────────────────

/// Push one H.264 AU through `push_video_misp_to`, demux, then:
/// (a) `extract()` recovers the exact timestamp.
/// (b) The spliced NAL sequence is AUD(9), SPS(7), PPS(8), SEI(6), IDR(5).
#[test]
fn misp_push_round_trips_and_orders_sei() {
    // Single-stream H.264 muxer via the targeted path (no KLV stream).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let handle = mux.video_handles()[0];

    let ts = MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F);
    let au = h264_keyframe_au();
    let pts = Pts90khz::new(90_000);

    mux.push_video_misp_to(handle, &au, pts, true, &ts).unwrap();
    let ts_bytes = drain_all(&mut mux);
    assert!(!ts_bytes.is_empty(), "expected TS output");

    // Demux to recover the raw video AU.
    let mut demux = Demuxer::with_config(DemuxerConfig::builder().build());
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut video_raw = None;
    while let Some(event) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = event
        {
            video_raw = Some(raw);
        }
    }
    let raw = video_raw.expect("expected a video sample from demux");

    // (a) extract() must recover the exact timestamp.
    let extracted = misp_time::extract(&raw, VideoCodec::H264)
        .expect("extract should not error")
        .expect("expected MISP timestamp in demuxed AU");
    assert_eq!(
        extracted, ts,
        "extracted timestamp must match the pushed one"
    );

    // (b) NAL order: AUD(9), SPS(7), PPS(8), SEI(6), IDR(5).
    let (video_payload, issues) =
        split_video(&raw, DemuxVideoCodec::H264, Av1CarriageMode::Mpeg2TsBinding);
    assert!(
        issues.is_empty(),
        "unexpected conformance issues: {issues:?}"
    );

    use tst_core::codec::NalUnit;
    use tst_core::mpegts::demux::event::VideoPayload;
    let nals = match video_payload {
        VideoPayload::Nals(n) => n,
        other => panic!("expected Nals variant, got {other:?}"),
    };

    let nal_types: Vec<u8> = nals
        .iter()
        .map(|n| match n {
            NalUnit::H264 { nal_type, .. } => *nal_type,
            other => panic!("expected H264 NalUnit, got {other:?}"),
        })
        .collect();

    assert_eq!(
        nal_types,
        vec![9, 7, 8, 6, 5],
        "NAL order must be AUD(9), SPS(7), PPS(8), SEI(6), IDR(5); got {nal_types:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// DTS variant smoke test
// ─────────────────────────────────────────────────────────────────────────────

/// `push_video_misp_to_with_dts` must also splice the SEI correctly.
#[test]
fn misp_push_with_dts_round_trips() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let handle = mux.video_handles()[0];

    let ts = MispTimestamp::micros(42, 0x00);
    let au = h264_keyframe_au();
    let pts = Pts90khz::new(90_000);
    let dts = Pts90khz::new(87_000);

    mux.push_video_misp_to_with_dts(handle, &au, pts, dts, true, &ts)
        .unwrap();
    let ts_bytes = drain_all(&mut mux);
    assert!(!ts_bytes.is_empty());

    let mut demux = Demuxer::with_config(DemuxerConfig::builder().build());
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut video_raw = None;
    while let Some(event) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = event
        {
            video_raw = Some(raw);
        }
    }
    let raw = video_raw.expect("expected video sample");
    let extracted = misp_time::extract(&raw, VideoCodec::H264)
        .unwrap()
        .expect("expected MISP timestamp");
    assert_eq!(extracted, ts);
}

// ─────────────────────────────────────────────────────────────────────────────
// Rejection tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn misp_push_rejects_no_vcl() {
    // An AUD-only AU (no VCL NAL) must return MispTime(NoVclNal).
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let handle = mux.video_handles()[0];
    let ts = MispTimestamp::micros(1, 0x1F);
    let err = mux
        .push_video_misp_to(handle, &h264_no_vcl_au(), Pts90khz::new(0), false, &ts)
        .unwrap_err();
    assert_eq!(err.kind(), MuxErrorKind::InputMalformed);
    assert!(
        matches!(
            err,
            MuxError::MispTime(tst_core::codec::misp_time::MispTimeError::NoVclNal)
        ),
        "expected MispTime(NoVclNal), got {err:?}"
    );
}

#[test]
fn misp_push_rejects_av1_stream() {
    // An AV1-configured stream must return MispTime(UnsupportedCodec).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(Av1CarriageMode::InteropRawObu);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let handle = mux.video_handles()[0];
    // Any non-empty bytes with a start code are enough — the AV1 stream
    // rejection fires before Annex-B or VCL checks.
    // Build a minimal fake Annex-B AU: note validate_annex_b is skipped for
    // AV1, so we can pass arbitrary bytes. For the rejection we just need
    // build_sei_nal to fail — which it does for Av1 regardless of input.
    // However, splice_misp_sei calls validate_annex_b before build_sei_nal,
    // so provide a valid-looking start-code prefix.
    let fake_au = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA]; // start code + IDR-like byte
    let ts = MispTimestamp::micros(1, 0x1F);
    let err = mux
        .push_video_misp_to(handle, &fake_au, Pts90khz::new(0), false, &ts)
        .unwrap_err();
    assert_eq!(err.kind(), MuxErrorKind::InputMalformed);
    assert!(
        matches!(
            err,
            MuxError::MispTime(tst_core::codec::misp_time::MispTimeError::UnsupportedCodec { .. })
        ),
        "expected MispTime(UnsupportedCodec), got {err:?}"
    );
}

#[test]
fn misp_push_rejects_nano_on_h264() {
    // A nano-precision timestamp on an H.264 stream must return
    // MispTime(NanoUnsupportedForCodec).
    let mut mux = Muxer::new(MuxerConfig::default()).unwrap();
    let handle = mux.video_handles()[0];
    let nano_ts = MispTimestamp::nanos(1_000_000, 0x1F);
    let au = h264_keyframe_au();
    let err = mux
        .push_video_misp_to(handle, &au, Pts90khz::new(0), true, &nano_ts)
        .unwrap_err();
    assert_eq!(err.kind(), MuxErrorKind::InputMalformed);
    assert!(
        matches!(
            err,
            MuxError::MispTime(
                tst_core::codec::misp_time::MispTimeError::NanoUnsupportedForCodec { .. }
            )
        ),
        "expected MispTime(NanoUnsupportedForCodec), got {err:?}"
    );
}
