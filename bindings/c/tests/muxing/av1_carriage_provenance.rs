//! C ABI provenance-byte and wire-push tests for AV1 carriage.
//!
//! Verifies:
//! 1. `ev.u.sample.av1_carriage` is set to 0 (`TST_AV1_CARRIAGE_MODE_MPEG2_TS_BINDING`)
//!    for a demuxed AV1 binding-mode sample and to 0xFF (N/A sentinel) for non-AV1
//!    samples (H.264).
//! 2. `tst_mux_config_set_av1_carriage` + `tst_muxer_push_video_wire[_to]` round-trip:
//!    demux an AV1 binding sample → push `payload` via `push_video_wire_to` into a
//!    new binding-mode muxer → re-demux → payload bytes match the original.
//!
//! TS bytes are built via the Rust `tst_core::Muxer` (same as
//! `demux_config_av1_parity.rs`) then driven through the C ABI demuxer surface.
//!
//! Safety note: `tst_demuxer_next_event` payload pointers are valid only until the
//! next call to `next_event` / `close` (the arena is reset on each call). Always
//! copy payload bytes before advancing to the next event.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_mux_config_set_av1_carriage,
};
use tstrans::demux_config::{
    TstAv1CarriageMode, tst_demux_config_free, tst_demux_config_new,
    tst_demux_config_set_av1_carriage,
};
use tstrans::demuxer::{
    tst_demuxer_close, tst_demuxer_feed, tst_demuxer_flush, tst_demuxer_next_event,
    tst_demuxer_open_with_config,
};
use tstrans::event::{TstEvent, TstEventKind};
use tstrans::muxer::{
    tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_video_wire,
    tst_muxer_push_video_wire_to,
};

/// Build a minimal AV1 access unit (TD, Sequence Header, Frame Header, Tile Group OBUs).
fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02; // obu_has_size_field = 1
        let mut v = vec![header];
        v.push(body.len() as u8); // single-byte LEB128
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

fn drain_mux_rust(mux: &mut Muxer) -> Vec<u8> {
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

/// Build a TS stream carrying one AV1 AU under the given Rust carriage mode.
fn build_av1_ts_rust(mode: Av1CarriageMode) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(mode);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    mux.push_video_to(h, &synthetic_av1_au(), Pts90khz::new(90_000), true)
        .unwrap();
    drain_mux_rust(&mut mux)
}

/// Build a TS stream carrying one H.264 IDR AU (single-NAL, Annex-B framed).
fn build_h264_ts_rust() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];
    mux.push_video_to(h, nal, Pts90khz::new(90_000), true)
        .unwrap();
    drain_mux_rust(&mut mux)
}

/// Open a C demuxer configured for the given AV1 carriage mode, feed `ts`, flush,
/// then invoke `visit` for each event (event pointer valid only during the call).
/// The callback returns `Option<T>` — iteration stops and the `Some` value is returned
/// on the first `Some`; if no call returns `Some`, returns `None`.
unsafe fn drain_events_c<T, F>(ts: &[u8], mode: TstAv1CarriageMode, mut visit: F) -> Option<T>
where
    F: FnMut(&TstEvent) -> Option<T>,
{
    // Edition 2024: unsafe calls inside an `unsafe fn` still need explicit unsafe {}.
    let cfg = unsafe { tst_demux_config_new() };
    assert!(!cfg.is_null());
    let rc = unsafe { tst_demux_config_set_av1_carriage(cfg, mode as i32) };
    assert_eq!(rc, 0);

    let demux = unsafe { tst_demuxer_open_with_config(cfg) };
    unsafe { tst_demux_config_free(cfg) };
    assert!(!demux.is_null());

    let rc = unsafe { tst_demuxer_feed(demux, ts.as_ptr(), ts.len()) };
    assert_eq!(rc, 0, "tst_demuxer_feed failed: {rc}");
    unsafe { tst_demuxer_flush(demux) };

    let mut result = None;
    let mut ev = TstEvent::default();
    loop {
        // rc == 0 → event written to `ev`; payload pointers valid until
        // the next call to next_event or close. Copy bytes BEFORE continuing.
        let rc = unsafe { tst_demuxer_next_event(demux, &mut ev) };
        if rc != 0 {
            break;
        }
        if let Some(v) = visit(&ev) {
            result = Some(v);
            break;
        }
    }
    unsafe { tst_demuxer_close(demux) };
    result
}

/// `av1_carriage` provenance byte is 0 (binding) for an AV1 binding-mode sample.
#[test]
fn av1_binding_sample_has_carriage_byte_0() {
    let ts = build_av1_ts_rust(Av1CarriageMode::Mpeg2TsBinding);
    let found = unsafe {
        drain_events_c(&ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    return Some(sample.av1_carriage);
                }
            }
            None
        })
    };
    let carriage = found.expect("expected at least one AV1 video sample event");
    assert_eq!(
        carriage, 0,
        "AV1 binding-mode sample must have av1_carriage=0 (MPEG2_TS_BINDING)"
    );
}

/// `av1_carriage` provenance byte is 1 (interop) for an AV1 interop-mode sample.
#[test]
fn av1_interop_sample_has_carriage_byte_1() {
    let ts = build_av1_ts_rust(Av1CarriageMode::InteropRawObu);
    let found = unsafe {
        drain_events_c(&ts, TstAv1CarriageMode::InteropRawObu, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    return Some(sample.av1_carriage);
                }
            }
            None
        })
    };
    let carriage = found.expect("expected at least one AV1 video sample event");
    assert_eq!(
        carriage, 1,
        "AV1 interop-mode sample must have av1_carriage=1 (INTEROP_RAW_OBU)"
    );
}

/// `av1_carriage` provenance byte is 0xFF (N/A sentinel) for non-AV1 (H.264) samples.
#[test]
fn h264_sample_has_carriage_byte_0xff() {
    let ts = build_h264_ts_rust();
    let found = unsafe {
        drain_events_c(&ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::H264 as i32 {
                    return Some(sample.av1_carriage);
                }
            }
            None
        })
    };
    let carriage = found.expect("expected at least one H.264 video sample event");
    assert_eq!(
        carriage, 0xFF,
        "H.264 sample must have av1_carriage=0xFF (N/A sentinel)"
    );
}

/// Remux fixpoint via `tst_muxer_push_video_wire_to`: demux an AV1 binding-mode
/// sample, push its raw payload through the wire-push C entry point into a new
/// binding-mode muxer, re-demux, and verify the payload bytes are preserved.
#[test]
fn av1_binding_remux_fixpoint_via_wire_push_to() {
    // Step 1: demux the source TS and copy the AV1 payload bytes.
    let src_ts = build_av1_ts_rust(Av1CarriageMode::Mpeg2TsBinding);
    let (av1_payload, pts) = unsafe {
        drain_events_c(&src_ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    assert_eq!(sample.av1_carriage, 0, "source must be binding mode");
                    assert!(!sample.payload.is_null());
                    // Copy the payload bytes NOW, before next_event clears the arena.
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some((bytes, sample.pts));
                }
            }
            None
        })
    }
    .expect("expected an AV1 sample in the source TS");

    // Step 2: open a new binding-mode muxer via the C ABI and push via _to.
    let remuxed_ts = unsafe {
        let cfg = tst_mux_config_new();
        assert!(!cfg.is_null());
        let prog = tst_mux_config_add_program(cfg, 1, 0x100);
        // hv is the video stream handle returned by add_video_stream.
        let hv = tst_mux_config_add_video_stream(cfg, prog, 0x101, TstVideoCodec::Av1);
        let rc = tst_mux_config_set_av1_carriage(cfg, TstAv1CarriageMode::Mpeg2TsBinding as i32);
        assert_eq!(rc, 0, "tst_mux_config_set_av1_carriage failed");
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        let rc = tst_muxer_push_video_wire_to(
            mux,
            hv,
            av1_payload.as_ptr(),
            av1_payload.len(),
            pts,
            true,
        );
        assert_eq!(rc, 0, "tst_muxer_push_video_wire_to failed: {rc}");

        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 188];
        loop {
            let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        tst_muxer_close(mux);
        out
    };

    assert!(!remuxed_ts.is_empty(), "remux must produce TS output");

    // Step 3: re-demux and verify payload bytes match.
    let found = unsafe {
        drain_events_c(&remuxed_ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some(bytes);
                }
            }
            None
        })
    };
    let remuxed_payload = found.expect("expected an AV1 sample in the re-demuxed TS");
    assert_eq!(
        remuxed_payload, av1_payload,
        "remuxed AV1 payload must be byte-identical to the source payload"
    );
}

/// Remux fixpoint via `tst_muxer_push_video_wire` (single-stream shorthand):
/// same as above but uses the no-handle C entry point.
#[test]
fn av1_binding_remux_fixpoint_via_wire_push_single_stream() {
    let src_ts = build_av1_ts_rust(Av1CarriageMode::Mpeg2TsBinding);
    let (av1_payload, pts) = unsafe {
        drain_events_c(&src_ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some((bytes, sample.pts));
                }
            }
            None
        })
    }
    .expect("expected an AV1 sample in the source TS");

    let remuxed_ts = unsafe {
        let cfg = tst_mux_config_new();
        assert!(!cfg.is_null());
        let prog = tst_mux_config_add_program(cfg, 1, 0x100);
        tst_mux_config_add_video_stream(cfg, prog, 0x101, TstVideoCodec::Av1);
        let rc = tst_mux_config_set_av1_carriage(cfg, TstAv1CarriageMode::Mpeg2TsBinding as i32);
        assert_eq!(rc, 0);
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        // Single-stream shorthand — no handle needed.
        let rc = tst_muxer_push_video_wire(mux, av1_payload.as_ptr(), av1_payload.len(), pts, true);
        assert_eq!(rc, 0, "tst_muxer_push_video_wire failed: {rc}");

        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 188];
        loop {
            let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        tst_muxer_close(mux);
        out
    };

    let found = unsafe {
        drain_events_c(&remuxed_ts, TstAv1CarriageMode::Mpeg2TsBinding, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some(bytes);
                }
            }
            None
        })
    };
    let remuxed_payload = found.expect("expected an AV1 sample in the re-demuxed TS");
    assert_eq!(
        remuxed_payload, av1_payload,
        "single-stream wire-push must produce byte-identical payload"
    );
}

/// Remux fixpoint in INTEROP mode: configure both the source and the
/// destination muxer for `INTEROP_RAW_OBU` via `tst_mux_config_set_av1_carriage`,
/// demux → `tst_muxer_push_video_wire` → re-demux → bytes equal. In interop mode
/// neither generation wraps, so this guards against the wire push accidentally
/// re-wrapping interop payloads (the mirror of the binding fixpoint).
#[test]
fn av1_interop_remux_fixpoint_via_wire_push_single_stream() {
    let src_ts = build_av1_ts_rust(Av1CarriageMode::InteropRawObu);
    let (av1_payload, pts) = unsafe {
        drain_events_c(&src_ts, TstAv1CarriageMode::InteropRawObu, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    assert_eq!(sample.av1_carriage, 1, "source must be interop mode");
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some((bytes, sample.pts));
                }
            }
            None
        })
    }
    .expect("expected an AV1 sample in the source TS");

    let remuxed_ts = unsafe {
        let cfg = tst_mux_config_new();
        assert!(!cfg.is_null());
        let prog = tst_mux_config_add_program(cfg, 1, 0x100);
        tst_mux_config_add_video_stream(cfg, prog, 0x101, TstVideoCodec::Av1);
        let rc = tst_mux_config_set_av1_carriage(cfg, TstAv1CarriageMode::InteropRawObu as i32);
        assert_eq!(rc, 0, "tst_mux_config_set_av1_carriage failed");
        let mux = tst_muxer_open(cfg);
        tst_mux_config_free(cfg);
        assert!(!mux.is_null());

        let rc = tst_muxer_push_video_wire(mux, av1_payload.as_ptr(), av1_payload.len(), pts, true);
        assert_eq!(rc, 0, "tst_muxer_push_video_wire failed: {rc}");

        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 188];
        loop {
            let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        tst_muxer_close(mux);
        out
    };

    let found = unsafe {
        drain_events_c(&remuxed_ts, TstAv1CarriageMode::InteropRawObu, |ev| {
            if ev.kind == TstEventKind::Sample as i32 {
                let sample = &ev.u.sample;
                if sample.codec == TstVideoCodec::Av1 as i32 {
                    let bytes =
                        core::slice::from_raw_parts(sample.payload, sample.payload_len).to_vec();
                    return Some(bytes);
                }
            }
            None
        })
    };
    let remuxed_payload = found.expect("expected an AV1 sample in the re-demuxed TS");
    assert_eq!(
        remuxed_payload, av1_payload,
        "interop-mode wire-push must produce byte-identical payload"
    );
}
