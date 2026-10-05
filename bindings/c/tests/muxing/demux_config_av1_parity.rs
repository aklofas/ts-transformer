//! End-to-end parity proof for the C-ABI `tst_demux_config_*` AV1
//! carriage knob.
//!
//! Builds two real AV1 access units (one carried in `Mpeg2TsBinding`
//! mode, one in `InteropRawObu` mode) via the in-process Rust muxer,
//! then drives them through a `tst_core::Demuxer` whose options come
//! from the C ABI builder (`tst_demux_config_new` +
//! `tst_demux_config_set_av1_carriage`). With a matching carriage
//! mode, neither AV1 binding-nonconformance issue must fire and the
//! Sample must arrive. With a mismatched mode, the binding-mode demux
//! must surface both `Av1WrongStreamId` and `Av1MissingTsObuFraming`
//! issues against the interop-mode wire payload.
//!
//! Exercises the full plumbing path from the C wrapper through
//! `TstDemuxConfig::build_options()` into the Rust demuxer — the
//! previous shape silently forced `av1_carriage = Mpeg2TsBinding`,
//! hiding the configuration knob from C callers entirely.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::event::{
    DemuxEvent, NonConformantIssue, SamplePayload, VideoCodec, VideoPayload,
};
use tst_core::mpegts::demux::{Demuxer, split_video};
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tstrans::demux_config::{
    TstAv1CarriageMode, test_build_options, tst_demux_config_free, tst_demux_config_new,
    tst_demux_config_set_av1_carriage,
};

/// Build a minimal AV1 access unit (TD, Sequence Header, Frame Header,
/// Tile Group OBUs). Each OBU has `obu_has_size_field = 1` so the
/// demuxer can recover them from raw OBU framing without ambiguity
/// (matches the existing `crates/tst-core/tests/av1_carriage_roundtrip.rs`
/// corpus).
fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        // AV1 spec §5.3.2 OBU header byte:
        //   obu_forbidden_bit  f(1) = 0
        //   obu_type           f(4)
        //   obu_extension_flag f(1) = 0
        //   obu_has_size_field f(1) = 1
        //   obu_reserved_1bit  f(1) = 0
        // = (obu_type << 3) | 0b010
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        // Single-byte LEB128 size (bodies < 128 bytes here).
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter
    au.extend(obu(1, &[0x00, 0x00])); // Sequence Header (placeholder)
    au.extend(obu(3, &[0x00])); // Frame Header (placeholder)
    au.extend(obu(4, &[0x00, 0x01, 0x02])); // Tile Group (placeholder)
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

/// Synthesize a TS stream carrying one AV1 AU under the requested
/// muxer carriage mode.
fn build_av1_ts(mux_mode: Av1CarriageMode) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(mux_mode);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &synthetic_av1_au(), Pts90khz::new(90_000), true)
        .unwrap();
    drain_mux(&mut mux)
}

/// Build a Rust `Demuxer` whose configuration originates from the C
/// ABI builder set to the given `mode`. Validates the wiring:
/// `TstDemuxConfig::build_options()` must propagate the carriage mode
/// down to `DemuxerConfig::av1_carriage` rather than silently use the
/// Rust default.
fn demuxer_from_c_with_av1_mode(mode: TstAv1CarriageMode) -> Demuxer {
    unsafe {
        let cfg = tst_demux_config_new();
        assert!(!cfg.is_null(), "tst_demux_config_new returned null");
        let rc = tst_demux_config_set_av1_carriage(cfg, mode as i32);
        assert_eq!(rc, 0, "set_av1_carriage failed");
        let opts = test_build_options(cfg);
        tst_demux_config_free(cfg);
        tst_core::mpegts::demux::Demuxer::with_config(
            tst_core::mpegts::demux::DemuxerConfig::builder()
                .av1_carriage(opts.av1_carriage)
                .build(),
        )
    }
}

/// Drain every event out of a Demuxer (after `feed` + `flush`) and
/// return them as a Vec for assertion-friendly inspection.
fn drain_events(demux: &mut Demuxer) -> Vec<DemuxEvent> {
    let mut out = Vec::new();
    while let Some(e) = demux.next_event() {
        out.push(e);
    }
    out
}

#[test]
fn c_demux_config_av1_interop_round_trip_no_binding_issues() {
    // Interop-mode mux + C-configured interop-mode demux: no binding
    // issues should fire; Sample must arrive.
    let ts = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let mut demux = demuxer_from_c_with_av1_mode(TstAv1CarriageMode::InteropRawObu);
    demux.feed(&ts).unwrap();
    demux.flush();

    let evts = drain_events(&mut demux);
    let mut saw_sample = false;
    for ev in &evts {
        if let DemuxEvent::NonConformant { issue, .. } = ev {
            assert!(
                !matches!(
                    issue,
                    NonConformantIssue::Av1WrongStreamId { .. }
                        | NonConformantIssue::Av1MissingTsObuFraming { .. }
                ),
                "C-configured interop demuxer must not raise binding issues against an interop sender: {issue:?}"
            );
        }
        if let DemuxEvent::Sample {
            payload:
                SamplePayload::Video {
                    codec: VideoCodec::Av1,
                    raw,
                    ..
                },
            ..
        } = ev
        {
            // Raw-first: the demuxer emits the encoded AU; recover the OBUs via
            // the opt-in `split_video`. Interop carriage round-trips cleanly, so
            // the four OBUs come back with no split issues.
            let (split, _issues) =
                split_video(raw, VideoCodec::Av1, Av1CarriageMode::InteropRawObu);
            let VideoPayload::Obus(obus) = split else {
                panic!("expected OBUs from split_video");
            };
            assert_eq!(
                obus.len(),
                4,
                "expected 4 OBUs (TD/SeqHeader/FrameHeader/TileGroup)"
            );
            saw_sample = true;
        }
    }
    assert!(
        saw_sample,
        "matching-carriage interop round-trip should emit the AV1 Sample"
    );
}

#[test]
fn c_demux_config_av1_binding_round_trip_no_binding_issues() {
    // Binding-mode mux + C-configured binding-mode demux: same proof
    // for the default carriage choice, but exercised through the C
    // setter so the wiring is end-to-end verified (and so a future
    // regression that ignores the C value would fail here too).
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let mut demux = demuxer_from_c_with_av1_mode(TstAv1CarriageMode::Mpeg2TsBinding);
    demux.feed(&ts).unwrap();
    demux.flush();

    let evts = drain_events(&mut demux);
    let mut saw_sample = false;
    for ev in &evts {
        if let DemuxEvent::NonConformant { issue, .. } = ev {
            assert!(
                !matches!(
                    issue,
                    NonConformantIssue::Av1WrongStreamId { .. }
                        | NonConformantIssue::Av1MissingTsObuFraming { .. }
                ),
                "C-configured binding demuxer must not raise binding issues against a binding sender: {issue:?}"
            );
        }
        if let DemuxEvent::Sample {
            payload:
                SamplePayload::Video {
                    codec: VideoCodec::Av1,
                    raw,
                    ..
                },
            ..
        } = ev
        {
            // Raw-first: recover the OBUs via the opt-in `split_video` (which
            // reverses the binding framing in `Mpeg2TsBinding` mode).
            let (split, _issues) =
                split_video(raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
            let VideoPayload::Obus(obus) = split else {
                panic!("expected OBUs from split_video");
            };
            assert_eq!(obus.len(), 4);
            saw_sample = true;
        }
    }
    assert!(
        saw_sample,
        "matching-carriage binding round-trip should emit the AV1 Sample"
    );
}

#[test]
fn c_demux_config_av1_mismatch_surfaces_both_binding_issues() {
    // Interop sender vs C-configured binding demuxer: the diagnostic surface is
    // now SPLIT across the raw-first boundary. The PES-layer `Av1WrongStreamId`
    // (stream_id=0xE0 instead of 0xBD) is still a demux event; the ES-content
    // `Av1MissingTsObuFraming` (no ts_open_bitstream_unit framing) moved to the
    // opt-in `split_video` call. The Sample still arrives (raw AU) and split
    // recovers the OBUs via the lenient raw-OBU fallback.
    //
    // Without the C-ABI knob, the bug shape would be invisible: the
    // demuxer would silently use `Mpeg2TsBinding` regardless of the
    // sender's carriage and the `Av1WrongStreamId` surface would look
    // identical even when the C caller asked for `InteropRawObu`. This
    // test pins that diagnostic surface.
    let ts = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let mut demux = demuxer_from_c_with_av1_mode(TstAv1CarriageMode::Mpeg2TsBinding);
    demux.feed(&ts).unwrap();
    demux.flush();

    let evts = drain_events(&mut demux);
    let mut saw_wrong_stream_id = false;
    let mut saw_sample = false;
    let mut raw_au = None;
    for ev in &evts {
        match ev {
            DemuxEvent::NonConformant { issue, .. } => match issue {
                NonConformantIssue::Av1WrongStreamId { observed, .. } => {
                    assert_eq!(*observed, 0xE0);
                    saw_wrong_stream_id = true;
                }
                // The demuxer no longer raises Av1MissingTsObuFraming — that
                // ES-content issue moved to split_video (asserted below).
                NonConformantIssue::Av1MissingTsObuFraming { .. } => {
                    panic!("demuxer must not raise the ES-content Av1MissingTsObuFraming");
                }
                _ => {}
            },
            DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            } => {
                saw_sample = true;
                raw_au = Some(raw);
            }
            _ => {}
        }
    }
    assert!(saw_wrong_stream_id, "expected Av1WrongStreamId");
    // The opt-in split now carries the missing-framing conformance signal and
    // still recovers the OBUs via the raw-OBU fallback.
    let raw = raw_au.expect("lenient mode still emits the raw AU Sample");
    let (_split, split_issues) = split_video(raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding);
    assert!(
        split_issues
            .iter()
            .any(|i| matches!(i, NonConformantIssue::Av1MissingTsObuFraming { .. })),
        "split_video should report Av1MissingTsObuFraming for raw-OBU carriage: {split_issues:?}"
    );
    assert!(
        saw_sample,
        "lenient raw-OBU fallback should still emit the Sample"
    );
}
