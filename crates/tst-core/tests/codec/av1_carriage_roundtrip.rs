//! AV1 mux -> demux carriage round-trip.
//!
//! Asserts that pushing a synthetic AV1 access unit (Temporal Delimiter +
//! Sequence Header + Frame Header + Tile Group OBUs) through the muxer +
//! demuxer round-trip preserves the codec classification (PMT stream_type
//! 0x06 with `format_identifier "AV01"` -> `VideoCodec::Av1`) and the
//! per-OBU header bytes.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::event::{
    DemuxEvent, NonConformantIssue, Obu, SamplePayload, StreamId, StreamKind, VideoCodec,
    VideoPayload,
};
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
use tst_core::mpegts::demux::{split_video, split_video_strict};
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_core::shared::SharedBytes;

/// Build a minimal AV1 access unit: Temporal Delimiter + Sequence Header +
/// Frame Header + Tile Group. Each OBU has `obu_has_size_field = 1`. Bodies
/// are placeholders — what matters for this test is that the demuxer recovers
/// each OBU with the correct `obu_type`.
fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        // AV1 spec §5.3.2 OBU header byte:
        //   obu_forbidden_bit  f(1) = 0
        //   obu_type           f(4)
        //   obu_extension_flag f(1) = 0
        //   obu_has_size_field f(1) = 1   <-- required by AV1-in-MPEG-2-TS §3.1
        //   obu_reserved_1bit  f(1) = 0
        // = (obu_type << 3) | 0b010
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        // Single-byte LEB128 size (body lengths < 128 here).
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter (always empty body)
    au.extend(obu(1, &[0x00, 0x00])); // Sequence Header (placeholder body)
    au.extend(obu(3, &[0x00])); // Frame Header (placeholder body)
    au.extend(obu(4, &[0x00, 0x01, 0x02])); // Tile Group (placeholder body)
    au
}

fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn av1_mux_demux_roundtrip_emits_obus() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let video_handle = mux.video_handles()[0];
    let au = synthetic_av1_au();

    // Push one AU at PTS=90000 (1 second), key frame.
    mux.push_video_to(video_handle, &au, Pts90khz::new(90_000), true)
        .expect("push");
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    // Unbounded video PES (PES_packet_length=0) buffers in-flight; flush
    // drains it. Live receive loops do this on TransportError::Closed.
    demux.flush();

    let mut sample_evt: Option<(StreamId, SamplePayload)> = None;
    while let Some(e) = demux.next_event() {
        if let DemuxEvent::Sample {
            stream, payload, ..
        } = e
        {
            sample_evt = Some((stream, payload));
            break;
        }
    }
    let (stream, payload) = sample_evt.expect("expected Sample event");
    assert_eq!(stream.kind, StreamKind::Video(VideoCodec::Av1));
    // Raw-first: the demuxer emits the encoded AU verbatim; OBU splitting is
    // the opt-in `split_video` call (which reverses the binding framing).
    match payload {
        SamplePayload::Video {
            codec: VideoCodec::Av1,
            raw,
            ..
        } => {
            let (split, issues) =
                split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
            assert!(issues.is_empty(), "binding-conformant AU emits no issues");
            let VideoPayload::Obus(obus) = split else {
                panic!("expected OBUs from split_video");
            };
            assert_eq!(
                obus.len(),
                4,
                "expected 4 OBUs (TD/SeqHeader/FrameHeader/TileGroup)"
            );
            let types: Vec<u8> = obus.iter().map(|o: &Obu| o.obu_type).collect();
            assert_eq!(types, vec![2, 1, 3, 4]);
        }
        other => panic!("unexpected SamplePayload: {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// AV1-in-MPEG-2-TS binding-conformant round-trips
// ─────────────────────────────────────────────────────────────────────────

/// Default mux + default demux both use `Av1CarriageMode::Mpeg2TsBinding`.
/// The OBUs must round-trip without raising any binding nonconformance
/// issues, even when the payload contains bytes that would alias the
/// `ts_open_bitstream_unit` start code (the mux escapes, the demux unwraps).
#[test]
fn av1_binding_mode_round_trip_no_issues_with_emulation_prevention() {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    // OBU bodies contain bytes that, when concatenated, produce a
    // 0x00 0x00 0x01 sequence — the muxer MUST insert emulation
    // prevention, the demuxer MUST strip it back out.
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter
    au.extend(obu(1, &[0x00, 0x00, 0x01, 0xAA])); // Sequence Header (carries the aliasing sequence)
    au.extend(obu(3, &[0x00, 0xFF])); // Frame Header
    au.extend(obu(4, &[0x00, 0x00, 0x02])); // Tile Group (another aliasing sequence)

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    assert_eq!(cfg.av1_carriage, Av1CarriageMode::Mpeg2TsBinding);
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut issues: Vec<NonConformantIssue> = Vec::new();
    let mut sample_evt: Option<SamplePayload> = None;
    while let Some(e) = demux.next_event() {
        match e {
            DemuxEvent::Sample { payload, .. } => sample_evt = Some(payload),
            DemuxEvent::NonConformant { issue, .. } => issues.push(issue),
            _ => {}
        }
    }
    // No binding-conformance issues should fire on a binding-conformant
    // mux+demux pair. (Other issues like the AV1 OBU forbidden/reserved
    // bit checks are not exercised here — the synthetic OBUs are clean.)
    for i in &issues {
        assert!(
            !matches!(
                i,
                NonConformantIssue::Av1WrongStreamId { .. }
                    | NonConformantIssue::Av1MissingTsObuFraming { .. }
            ),
            "unexpected binding-conformance issue on a conformant round-trip: {i:?}"
        );
    }
    let payload = sample_evt.expect("expected Sample event");
    if let SamplePayload::Video { raw, .. } = payload {
        // split_video reverses the binding framing + emulation-prevention,
        // recovering the original OBU bytes byte-for-byte. A binding-conformant
        // AU raises no split issues.
        let (split, split_issues) =
            split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
        assert!(
            split_issues.is_empty(),
            "binding-conformant AU emits no split issues: {split_issues:?}"
        );
        let VideoPayload::Obus(obus) = split else {
            panic!("expected OBUs from split_video");
        };
        let types: Vec<u8> = obus.iter().map(|o| o.obu_type).collect();
        assert_eq!(types, vec![2, 1, 3, 4]);
        // Body bytes recovered byte-for-byte after demux unwrap +
        // emulation-prevention strip.
        assert_eq!(obus[1].payload.as_slice(), &[0x00, 0x00, 0x01, 0xAA]);
        assert_eq!(obus[3].payload.as_slice(), &[0x00, 0x00, 0x02]);
    } else {
        panic!("expected AV1 video sample, got {payload:?}");
    }
}

/// Interop-mode sender + binding-mode demuxer. Raw-first split: the demuxer
/// surfaces the PES-layer `Av1WrongStreamId` (stream_id=0xE0 instead of 0xBD)
/// and emits the Sample verbatim. The ES-content `Av1MissingTsObuFraming` (no
/// start-code prefix) is no longer a demux event — it moves to the opt-in
/// `split_video` call, which still recovers the OBUs via the raw-OBU fallback.
#[test]
fn av1_interop_sender_into_binding_demuxer_surfaces_both_issues() {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[]));
    au.extend(obu(1, &[0xAA]));

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(Av1CarriageMode::InteropRawObu);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    // Default demuxer = binding-mode.
    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut saw_wrong_stream_id = false;
    let mut raw_au: Option<SharedBytes> = None;
    while let Some(e) = demux.next_event() {
        match e {
            DemuxEvent::NonConformant { issue, .. } => {
                if let NonConformantIssue::Av1WrongStreamId { observed, .. } = issue {
                    assert_eq!(observed, 0xE0);
                    saw_wrong_stream_id = true;
                }
                // The demuxer no longer raises Av1MissingTsObuFraming — that
                // ES-content issue moved to split_video (asserted below).
                assert!(
                    !matches!(issue, NonConformantIssue::Av1MissingTsObuFraming { .. }),
                    "demuxer must not raise the ES-content Av1MissingTsObuFraming"
                );
            }
            DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            } => {
                raw_au = Some(raw);
            }
            _ => {}
        }
    }
    assert!(saw_wrong_stream_id, "expected Av1WrongStreamId issue");
    let raw = raw_au.expect("lenient mode still emits the Sample (raw AU)");
    // The opt-in split now carries the missing-framing conformance signal and
    // still recovers the OBUs via the raw-OBU fallback. Pass Mpeg2TsBinding:
    // the demuxer here is binding-mode, so the binding-absent framing IS a
    // genuine issue.
    let (split, issues) = split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
    assert!(
        issues
            .iter()
            .any(|i| matches!(i, NonConformantIssue::Av1MissingTsObuFraming { .. })),
        "split_video should report Av1MissingTsObuFraming for raw-OBU carriage: {issues:?}"
    );
    let VideoPayload::Obus(obus) = split else {
        panic!("expected OBUs from split_video");
    };
    assert_eq!(obus.len(), 2, "raw-OBU fallback recovers both OBUs");
}

/// Interop-mode sender + interop-mode demuxer: no binding issues, classic
/// round-trip. Exercises the escape-hatch matching.
#[test]
fn av1_interop_round_trip_no_binding_issues() {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[]));
    au.extend(obu(1, &[0xAA]));

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(Av1CarriageMode::InteropRawObu);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::with_config(
        DemuxerConfig::builder()
            .av1_carriage(Av1CarriageMode::InteropRawObu)
            .build(),
    );
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut saw_sample = false;
    while let Some(e) = demux.next_event() {
        match e {
            DemuxEvent::NonConformant { issue, .. } => {
                assert!(
                    !matches!(
                        issue,
                        NonConformantIssue::Av1WrongStreamId { .. }
                            | NonConformantIssue::Av1MissingTsObuFraming { .. }
                    ),
                    "interop demuxer must not raise binding-conformance issues: {issue:?}"
                );
            }
            DemuxEvent::Sample { .. } => saw_sample = true,
            _ => {}
        }
    }
    assert!(saw_sample, "interop round-trip should emit Sample");
}

/// Spec-byte assertion: for an AV1 binding-mode
/// access unit carrying N OBUs, the on-wire PES payload MUST contain
/// EXACTLY N `0x00 0x00 0x01` start codes — one per `ts_open_bitstream_unit()`
/// invocation per binding §3.2 syntax. Single-start-code-per-AU framing is
/// non-conformant; this test pairs the round-trip test above with a
/// wire-format assertion to catch any regression to it.
#[test]
fn av1_binding_mode_emits_one_start_code_per_obu_on_wire() {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    // 4-OBU AU — same shape as the round-trip test above but with bodies
    // chosen so no body needs escape padding (verifies the COUNT
    // independent of escape mechanics).
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter
    au.extend(obu(1, &[0xAA, 0xBB])); // Sequence Header
    au.extend(obu(3, &[0xCC])); // Frame Header
    au.extend(obu(4, &[0xDD, 0xEE, 0xFF])); // Tile Group

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    assert_eq!(cfg.av1_carriage, Av1CarriageMode::Mpeg2TsBinding);
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    // Count occurrences of the 3-byte binding §3.2 start code in the TS
    // byte stream. The PES payload region is interleaved with TS-packet
    // headers (4 bytes every 188 bytes), but the start code as a 3-byte
    // sequence cannot straddle a TS header boundary in a meaningful way:
    // either the start code lands entirely within the PES payload of one
    // TS packet, or its bytes are contiguous in the demuxer's reassembled
    // PES buffer. For the small synthetic AU here all OBUs land in a
    // single PES, and the simple windows() scan over the raw TS byte
    // stream gives a stable lower bound.
    //
    // We expect AT LEAST 4 occurrences (one per OBU). PSI sections and
    // TS headers don't contain `0x00 0x00 0x01` as a natural pattern, so
    // false positives are extremely unlikely on a synthetic stream like
    // this — but assert ≥ 4 (not == 4) to keep the test robust to any
    // future TS-stuffing change.
    let count = ts_bytes
        .windows(3)
        .filter(|w| *w == [0x00, 0x00, 0x01])
        .count();
    assert!(
        count >= 4,
        "expected ≥ 4 binding start codes (one per OBU) in PES wire bytes, got {count}"
    );
}

/// The demux `SamplePayload::Video` is stamped with the
/// carriage mode the demuxer was configured for. A binding-mode demuxer on a
/// binding-mode sender must report `Some(Av1CarriageMode::Mpeg2TsBinding)`.
#[test]
fn binding_demux_stamps_sample_with_binding_carriage() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &synthetic_av1_au(), Pts90khz::new(90_000), true)
        .unwrap();
    let ts = drain_mux(&mut mux);
    let mut demux = Demuxer::new(); // default = binding
    demux.feed(&ts).unwrap();
    demux.flush();
    let mut carriage = None;
    while let Some(e) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { av1_carriage, .. },
            ..
        } = e
        {
            carriage = av1_carriage;
            break;
        }
    }
    assert_eq!(carriage, Some(Av1CarriageMode::Mpeg2TsBinding));
}

/// An interop sample (raw OBUs, no binding framing) must parse clean
/// under `InteropRawObu` carriage and must surface `Av1MissingTsObuFraming`
/// under `Mpeg2TsBinding` carriage. This pins the carriage-aware branch in
/// `split_video` / `split_video_strict`.
#[test]
fn interop_sample_split_is_clean_under_interop_carriage() {
    // Raw OBUs (no binding framing) — the interop on-wire payload.
    // OBU header byte: (obu_type << 3) | 0x02 (obu_has_size_field=1).
    // TD obu_type=2: header=0x12, size=0.
    // SeqHdr obu_type=1: header=0x0A, size=1, body=0xAA.
    let raw = SharedBytes::from_vec(vec![0x12, 0x00, 0x0A, 0x01, 0xAA]);

    // Interop carriage: raw OBUs are expected — no Av1MissingTsObuFraming.
    let strict = split_video_strict(&raw, VideoCodec::Av1, Av1CarriageMode::InteropRawObu);
    assert!(
        strict.is_ok(),
        "interop raw OBUs must parse clean under InteropRawObu carriage: {strict:?}"
    );

    // Binding carriage on the same raw OBU input: missing framing IS an issue.
    let (_payload, issues) = split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
    assert!(
        issues
            .iter()
            .any(|i| matches!(i, NonConformantIssue::Av1MissingTsObuFraming { .. })),
        "binding carriage on raw-OBU input must surface missing-framing: {issues:?}"
    );
}

// Suppress unused import warning when StreamId-only branches don't fire.
#[allow(dead_code)]
fn _unused_imports(_: StreamId) {}
