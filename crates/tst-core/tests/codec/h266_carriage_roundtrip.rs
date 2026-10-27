//! H.266 mux -> demux carriage round-trip.
//!
//! Asserts that pushing a synthetic Annex-B H.266 access unit through the
//! muxer + demuxer round-trip preserves the codec classification (PMT
//! stream_type 0x33 -> `VideoCodec::H266`) and the per-NAL header fields.

use crate::common::mux_demux::{collect_events, drain_mux};
use crate::common::synthetic::synthetic_h266_au;
use tst_core::codec::{NalUnit, VideoCodec};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::demux::event::{DemuxEvent, SamplePayload, StreamKind, VideoPayload};
use tst_core::mpegts::demux::split_video;
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

#[test]
fn h266_mux_demux_roundtrip_emits_h266_nals() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::H266);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let video_handle = mux.video_handles()[0];
    let au = synthetic_h266_au();

    // Push one AU at PTS=90000 (1 second), key frame.
    mux.push_video_to(video_handle, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    // Unbounded video PES (PES_packet_length=0) buffers in-flight; flush
    // drains it. Live receive loops do this on TransportError::Closed.
    demux.flush();
    let events = collect_events(&mut demux);

    // Find the Sample event and assert codec + payload shape.
    let (stream, payload) = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::Sample {
                stream, payload, ..
            } => Some((stream, payload)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected at least one Sample event, got: {events:?}"));
    assert_eq!(stream.kind, StreamKind::Video(VideoCodec::H266));
    match payload {
        SamplePayload::Video {
            codec: VideoCodec::H266,
            raw,
            ..
        } => {
            // Raw-first: split the encoded AU into NALs via the opt-in call.
            let (split, issues) =
                split_video(raw, VideoCodec::H266, Av1CarriageMode::Mpeg2TsBinding);
            assert!(issues.is_empty(), "conformant H.266 AU emits no issues");
            let VideoPayload::Nals(nals) = split else {
                panic!("expected NALs from split_video");
            };
            assert_eq!(nals.len(), 5, "expected 5 NALs (AUD/VPS/SPS/PPS/IDR)");
            // Spot-check the VPS_NUT.
            match &nals[1] {
                NalUnit::H266 {
                    nal_type,
                    layer_id,
                    temporal_id_plus1,
                    ..
                } => {
                    assert_eq!(*nal_type, 14);
                    assert_eq!(*layer_id, 0);
                    assert_eq!(*temporal_id_plus1, 1);
                }
                other => panic!("expected NalUnit::H266, got: {other:?}"),
            }
        }
        other => panic!("unexpected SamplePayload: {other:?}"),
    }
}
