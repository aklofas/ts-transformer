//! `MuxSender` over an in-memory `Transport` must produce TS bytes
//! byte-identical to the CI-guarded video-roundtrip golden. This is the std
//! mirror of the on-device check in `embedded/baremetal-qemu/`.

use crate::common::ScriptedSend;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::MuxSender;

/// Verbatim of `tst_integration::scenarios::synthetic_h264_idr()`.
fn synthetic_h264_idr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    buf.push(0x65);
    for i in 0u8..15 {
        buf.push(0xA5 ^ i);
    }
    buf
}

#[test]
fn mux_sender_matches_video_roundtrip_golden() {
    let golden = include_bytes!(
        "../../../tst-integration/tests/fixtures/scenarios/video-roundtrip/output.ts"
    );

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };

    let sink = ScriptedSend::ok();
    let sender = MuxSender::new(sink.clone(), cfg).expect("mux sender");
    sender
        .send_video(&synthetic_h264_idr(), Pts90khz::new(0), true)
        .expect("send_video");
    sender.close();

    assert_eq!(
        sink.wire_bytes().as_slice(),
        golden.as_slice(),
        "MuxSender output diverged from the video-roundtrip golden"
    );
}
