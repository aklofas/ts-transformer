//! Round-trip integration: Muxer -> bytes -> Demuxer.
//!
//! Drives the existing `mpegts::mux::Muxer` to produce TS bytes, feeds them
//! into `Demuxer`, asserts the events recovered match the inputs.

use crate::common::mux_demux::{collect_events, drain_mux};
use crate::common::synthetic::build_dummy_klv;
use crate::common::synthetic::build_minimal_h264_au;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, MetadataKind, SamplePayload, StreamKind, VideoCodec,
};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

fn build_minimal_h265_au() -> Vec<u8> {
    // Annex-B: AUD (nal_type=35) + IDR_W_RADL (nal_type=19, layer=0, temp=1).
    vec![
        0x00, 0x00, 0x00, 0x01, 0x46, 0x01, 0x10, 0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xCC, 0xDD,
    ]
}

#[test]
fn h264_async_klv_roundtrip() {
    // Async KLV per ST 1402: PrivateData stream_type with no PTS in PES.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let au = build_minimal_h264_au();
    mux.push_video(&au, Pts90khz::new(90_000), true).unwrap();
    let klv = build_dummy_klv();
    mux.push_klv(&klv, Pts90khz::new(90_000), 0x00).unwrap();
    let bytes = drain_mux(&mut mux);

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    // Drain partial-PES buffered by unbounded video PES (PES_packet_length=0).
    // In live receive loops this happens at TransportError::Closed; here we
    // call it explicitly because the test produces a finite byte stream.
    d.flush();
    let evs = collect_events(&mut d);

    assert!(
        evs.iter().any(|e| matches!(e, DemuxEvent::ProgramMap(_))),
        "expected ProgramMap event, got: {evs:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Video {
                    codec: VideoCodec::H264,
                    ..
                },
                ..
            }
        )),
        "expected H.264 video Sample, got: {evs:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            DemuxEvent::Metadata {
                kind: MetadataKind::KlvAsync,
                stream: tst_core::mpegts::demux::StreamId {
                    kind: StreamKind::KlvAsync,
                    ..
                },
                ..
            }
        )),
        "expected async-KLV Metadata, got: {evs:?}"
    );
}

#[test]
fn h265_async_klv_roundtrip() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H265);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let au = build_minimal_h265_au();
    mux.push_video(&au, Pts90khz::new(90_000), true).unwrap();
    mux.push_klv(&build_dummy_klv(), Pts90khz::new(90_000), 0x00)
        .unwrap();
    let bytes = drain_mux(&mut mux);

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    // Drain partial-PES buffered by unbounded video PES (PES_packet_length=0).
    // In live receive loops this happens at TransportError::Closed; here we
    // call it explicitly because the test produces a finite byte stream.
    d.flush();
    let evs = collect_events(&mut d);

    assert!(
        evs.iter().any(|e| matches!(
            e,
            DemuxEvent::Sample {
                payload: SamplePayload::Video {
                    codec: VideoCodec::H265,
                    ..
                },
                ..
            }
        )),
        "expected H.265 video Sample, got: {evs:?}"
    );
}
