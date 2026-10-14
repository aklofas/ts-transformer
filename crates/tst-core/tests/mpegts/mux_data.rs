//! Mux→demux round-trip golden for Data streams (`push_data` / `push_data_to`).
//!
//! Property under test: a `StreamSpec::Data` stream is a PES pass-through —
//! the muxer applies no framing (no AU cell, no payload inspection), and the
//! demuxer surfaces the stream as `StreamKind::Unknown(stream_type)` with
//! byte-identical raw payloads. Caller-supplied PMT descriptors survive the
//! trip verbatim (the muxer never auto-emits descriptors on a data stream).
//!
//! Pinned no-PTS semantics (read from `demux/pes_emit.rs`): when a PES carries
//! no PTS, the demuxer substitutes `Pts90khz::new(0)` on the `Sample` event
//! and — because Unknown streams have no H.222.0 §2.7.4 PTS mandate — emits
//! no `NonConformant` for the omission.

use crate::common::mux_demux::{demux_bytes as drain_events, drain_mux};
use crate::common::synthetic::synthetic_h264_au;
use tst_core::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload, StreamKind as DemuxStreamKind};
use tst_core::mpegts::mux::{
    DataStreamHandle, Muxer, MuxerConfig, MuxerProgramConfigBuilder, StreamKind, VideoCodec,
};

/// One video + three data streams (0xF0 w/ descriptor, 0xF1, bare 0x06
/// without PTS) — the corpus-shaped private-stream mix.
fn three_data_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_data(0x1100, 0xF0, /*carries_pts=*/ true);
    prog.stream_descriptors_for_data(0, vec![b"\xFF\x0ASERIAL_ADF".to_vec()])
        .unwrap();
    prog.add_data(0x1101, 0xF1, /*carries_pts=*/ true);
    prog.add_data(0x1102, 0x06, /*carries_pts=*/ false);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Collect `(pts, raw_bytes)` for every Unknown `Sample` on `pid`.
fn unknown_samples_on_pid(events: &[DemuxEvent], pid: u16) -> Vec<(Pts90khz, Vec<u8>)> {
    events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Sample {
                stream,
                pts,
                payload: SamplePayload::Unknown { raw, .. },
                ..
            } if stream.pid == pid => Some((*pts, raw.as_slice().to_vec())),
            _ => None,
        })
        .collect()
}

#[test]
fn data_round_trip_preserves_type_descriptors_payload_pts() {
    let mut mux = Muxer::new(three_data_cfg()).unwrap();

    // Handles come back in declaration order: (prog 0, within 0..3).
    let handles = mux.data_handles();
    assert_eq!(handles.len(), 3);
    assert_eq!(handles[0].unpack(), (0, 0));
    assert_eq!(handles[1].unpack(), (0, 1));
    assert_eq!(handles[2].unpack(), (0, 2));
    // Accessor parity: single-program index lookup + per-program list.
    assert_eq!(mux.data_stream_handle(1), Some(handles[1]));
    assert_eq!(mux.data_stream_handle(3), None);
    assert_eq!(mux.data_handles_for_program(1).unwrap(), handles);

    // One video AU so PSI/PCR pacing starts, then one payload per stream.
    mux.push_video(&synthetic_h264_au(), Pts90khz::new(900_000), true)
        .unwrap();
    let serial_payload = b"USR01,line-record-1\r\n".to_vec();
    let binary_payload = vec![0xA5u8; 128];
    let json_payload = b"{\"command\":\"action\"}".to_vec();
    // ~10 KB payload spans many TS packets — exercises the packetize loop
    // past the single-packet case.
    let big_payload: Vec<u8> = (0..10_240u32).map(|i| (i % 251) as u8).collect();
    mux.push_data_to(handles[0], &serial_payload, Pts90khz::new(900_000))
        .unwrap();
    mux.push_data_to(handles[1], &binary_payload, Pts90khz::new(903_000))
        .unwrap();
    mux.push_data_to(handles[2], &json_payload, Pts90khz::new(906_000))
        .unwrap();
    mux.push_data_to(handles[0], &big_payload, Pts90khz::new(909_000))
        .unwrap();

    let ts = drain_mux(&mut mux);
    let events = drain_events(&ts);

    // ── PMT: all three data PIDs classify Unknown(stream_type) ──────────
    let pm = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .expect("ProgramMap emitted");
    let kind_of = |pid: u16| {
        pm.streams
            .iter()
            .find(|s| s.pid == pid)
            .unwrap_or_else(|| panic!("PMT entry for pid 0x{pid:04X}"))
            .kind
    };
    assert_eq!(kind_of(0x1100), DemuxStreamKind::Unknown(0xF0));
    assert_eq!(kind_of(0x1101), DemuxStreamKind::Unknown(0xF1));
    assert_eq!(kind_of(0x1102), DemuxStreamKind::Unknown(0x06));

    // ── Descriptors: caller TLV survives verbatim, nothing auto-emitted ──
    let serial_info = pm.streams.iter().find(|s| s.pid == 0x1100).unwrap();
    assert_eq!(serial_info.raw_descriptors.len(), 1);
    assert_eq!(serial_info.raw_descriptors[0].tag, 0xFF);
    assert_eq!(serial_info.raw_descriptors[0].data, b"SERIAL_ADF");
    // The other two declared no descriptors; the muxer must not invent any.
    let f1_info = pm.streams.iter().find(|s| s.pid == 0x1101).unwrap();
    assert!(f1_info.raw_descriptors.is_empty());
    let bare_info = pm.streams.iter().find(|s| s.pid == 0x1102).unwrap();
    assert!(bare_info.raw_descriptors.is_empty());

    // ── Samples: byte-identical payloads, PTS per carries_pts ───────────
    let serial = unknown_samples_on_pid(&events, 0x1100);
    assert_eq!(serial.len(), 2);
    assert_eq!(serial[0].0, Pts90khz::new(900_000));
    assert_eq!(serial[0].1, serial_payload);
    assert_eq!(serial[1].0, Pts90khz::new(909_000));
    assert_eq!(serial[1].1, big_payload, "multi-packet payload round-trips");

    let binary = unknown_samples_on_pid(&events, 0x1101);
    assert_eq!(binary.len(), 1);
    assert_eq!(binary[0].0, Pts90khz::new(903_000));
    assert_eq!(binary[0].1, binary_payload);

    // carries_pts=false: the PES omits the PTS field; the demuxer
    // substitutes pts=0 (pes_emit.rs `unwrap_or(Pts90khz::new(0))`) and
    // emits no NonConformant for Unknown streams (PTS is only mandatory
    // for video/audio per H.222.0 §2.7.4).
    let json = unknown_samples_on_pid(&events, 0x1102);
    assert_eq!(json.len(), 1);
    assert_eq!(json[0].0, Pts90khz::new(0));
    assert_eq!(json[0].1, json_payload);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DemuxEvent::NonConformant { stream, .. } if stream.pid == 0x1102)),
        "PTS-less data stream must not trip NonConformant"
    );
}

#[test]
fn data_zero_length_payload_round_trips() {
    // A zero-length push is legal pass-through — one PES whose payload is
    // empty — and surfaces as an empty Unknown sample, with pts semantics
    // per carries_pts (preserved with PTS, substituted 0 without).
    let mut mux = Muxer::new(three_data_cfg()).unwrap();
    let handles = mux.data_handles();

    // One video AU so PSI/PCR pacing starts (as in the round-trip test).
    mux.push_video(&synthetic_h264_au(), Pts90khz::new(900_000), true)
        .unwrap();
    mux.push_data_to(handles[0], b"", Pts90khz::new(903_000))
        .unwrap();
    mux.push_data_to(handles[2], b"", Pts90khz::new(906_000))
        .unwrap();

    let ts = drain_mux(&mut mux);
    let events = drain_events(&ts);

    let with_pts = unknown_samples_on_pid(&events, 0x1100);
    assert_eq!(with_pts.len(), 1);
    assert_eq!(with_pts[0].0, Pts90khz::new(903_000));
    assert!(with_pts[0].1.is_empty(), "payload must round-trip empty");

    // carries_pts=false: the PES omits the PTS field; the demuxer
    // substitutes pts=0 (same pin as the main round-trip test).
    let no_pts = unknown_samples_on_pid(&events, 0x1102);
    assert_eq!(no_pts.len(), 1);
    assert_eq!(no_pts[0].0, Pts90khz::new(0));
    assert!(no_pts[0].1.is_empty(), "payload must round-trip empty");
}

#[test]
fn data_push_errors() {
    // >1 data stream: the no-handle shorthand is ambiguous.
    let mut mux = Muxer::new(three_data_cfg()).unwrap();
    match mux.push_data(b"x", Pts90khz::new(0)) {
        Err(MuxError::AmbiguousTarget { kind, count }) => {
            assert_eq!(kind, StreamKind::Data);
            assert_eq!(count, 3);
        }
        other => panic!("expected AmbiguousTarget, got {other:?}"),
    }

    // Out-of-range handle (program 0 has only 3 data streams).
    match mux.push_data_to(DataStreamHandle::pack(0, 7), b"x", Pts90khz::new(0)) {
        Err(MuxError::InvalidStreamHandle { kind, index }) => {
            assert_eq!(kind, StreamKind::Data);
            assert_eq!(index, 7);
        }
        other => panic!("expected InvalidStreamHandle, got {other:?}"),
    }

    // Payload past the PES_packet_length ceiling. carries_pts=true →
    // 8 bytes of PES overhead after the length field → max 65527.
    let handles = mux.data_handles();
    match mux.push_data_to(handles[0], &vec![0u8; 70_000], Pts90khz::new(0)) {
        Err(MuxError::DataTooLarge { size, max }) => {
            assert_eq!(size, 70_000);
            assert_eq!(max, 65_527);
        }
        other => panic!("expected DataTooLarge, got {other:?}"),
    }

    // carries_pts=false drops the 5-byte PTS field → 3 bytes of PES
    // overhead after the length field → max 65532. Pin the boundary one
    // past the ceiling.
    match mux.push_data_to(handles[2], &vec![0u8; 65_533], Pts90khz::new(0)) {
        Err(MuxError::DataTooLarge { size, max }) => {
            assert_eq!(size, 65_533);
            assert_eq!(max, 65_532);
        }
        other => panic!("expected DataTooLarge, got {other:?}"),
    }

    // At-ceiling payloads succeed — the documented maxima are exact, not
    // off-by-one. (Default buffer_packets = 10_000 comfortably holds the
    // ~720 TS packets these two pushes produce.)
    mux.push_data_to(handles[0], &vec![0u8; 65_527], Pts90khz::new(0))
        .expect("65527 bytes is exactly the with-PTS ceiling");
    mux.push_data_to(handles[2], &vec![0u8; 65_532], Pts90khz::new(0))
        .expect("65532 bytes is exactly the no-PTS ceiling");

    // Exactly one data stream: the shorthand routes without a handle.
    let single_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_data(0x1100, 0xF0, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut single = Muxer::new(single_cfg).unwrap();
    single
        .push_data(b"single-stream", Pts90khz::new(0))
        .unwrap();

    // Zero data streams: the shorthand names the actual problem.
    let video_only_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut video_only = Muxer::new(video_only_cfg).unwrap();
    match video_only.push_data(b"x", Pts90khz::new(0)) {
        Err(MuxError::NoDataStreamsConfigured) => {}
        other => panic!("expected NoDataStreamsConfigured, got {other:?}"),
    }
}
