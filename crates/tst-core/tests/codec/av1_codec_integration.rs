//! End-to-end: synthetic AV1 OBUs → mux → demux → codec::av1 parse.

use tst_core::codec::av1::parse_obu_stream;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::demux::event::{DemuxEvent, Obu, SamplePayload, VideoCodec, VideoPayload};
use tst_core::mpegts::demux::split_video;
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

fn obu_with_size(obu_type: u8, payload: &[u8]) -> Vec<u8> {
    let header = (obu_type << 3) | 0x02; // ext=0, has_size=1
    let mut v = vec![header, payload.len() as u8];
    v.extend_from_slice(payload);
    v
}

#[test]
fn av1_end_to_end_parses_seq_header_via_obu_stream() {
    // Bytes captured from codec::av1::sequence_header::tests::minimal_sequence_header.
    // Same Main profile / 320x240 / 8-bit 4:2:0 minimal SH.
    let seq_payload: Vec<u8> = vec![0, 0, 0, 4, 60, 255, 188, 0, 0, 0];
    // Keyframe header from keyframe_header_body().
    let frame_payload: Vec<u8> = vec![0x10];

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];

    let mut au = Vec::new();
    au.extend(obu_with_size(2, &[])); // TemporalDelimiter
    au.extend(obu_with_size(1, &seq_payload)); // Sequence Header
    au.extend(obu_with_size(3, &frame_payload)); // Frame Header (keyframe)
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .expect("push");

    // Drain TS bytes via mux.pull loop.
    let mut ts_bytes = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        ts_bytes.extend_from_slice(&buf[..n]);
    }

    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    demux.flush();

    let mut all_obus: Vec<Obu> = Vec::new();
    while let Some(e) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload:
                SamplePayload::Video {
                    codec: VideoCodec::Av1,
                    raw,
                    ..
                },
            ..
        } = e
        {
            // Raw-first: split the encoded AU into OBUs via the opt-in call.
            if let VideoPayload::Obus(obus) =
                split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding).0
            {
                all_obus.extend(obus);
            }
        }
    }
    assert!(
        !all_obus.is_empty(),
        "expected at least one Sample with OBUs"
    );

    let stream = parse_obu_stream(&all_obus);
    assert_eq!(stream.sequence_headers.len(), 1, "expected one SH");
    assert_eq!(stream.frame_headers.len(), 1, "expected one keyframe FH");
    assert!(
        stream.unparseable.is_empty(),
        "unparseable: {:?}",
        stream.unparseable
    );

    // Spot-check the parsed SH.
    let sh = &stream.sequence_headers[0];
    assert_eq!(sh.profile, 0);
    assert_eq!(sh.max_frame_width, 320);
    assert_eq!(sh.max_frame_height, 240);
    assert_eq!(sh.bit_depth, 8);

    // Spot-check the parsed FH.
    let fh = &stream.frame_headers[0];
    assert_eq!(fh.frame_type, 0); // KEY_FRAME
    assert!(fh.show_frame);
    assert!(!fh.show_existing_frame);
}
