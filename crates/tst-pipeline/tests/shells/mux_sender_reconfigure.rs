//! `MuxSender::reconfigure` runs under the sender's lock, drains the muxer
//! output to the transport, and maps refusals into `MuxSenderErrorSource::Mux`.

use crate::common::{ScriptedSend, build_h264_au, to_ts_packets};

use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::{MuxSender, MuxSenderErrorSource};

fn config(with_second_program: bool, first_codec: VideoCodec) -> MuxerConfig {
    let mut b = MuxerConfig::builder();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1011, first_codec);
    p1.add_klv(0x1031, KlvStreamType::PrivateData, true);
    b.add_program(p1.build());
    if with_second_program {
        let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
        p2.add_video(0x1111, VideoCodec::H264);
        p2.add_klv(0x1131, KlvStreamType::PrivateData, true);
        b.add_program(p2.build());
    }
    b.build().expect("valid muxer config")
}

fn pat_versions(wire: &[u8]) -> Vec<u8> {
    to_ts_packets(wire)
        .iter()
        .filter(|p| (u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])) == 0)
        .map(|p| (p[10] >> 1) & 0x1F)
        .collect()
}

#[test]
fn reconfigure_through_sender_emits_new_pat_version() {
    let sink = ScriptedSend::ok();
    let sender = MuxSender::new(sink.clone(), config(true, VideoCodec::H264)).expect("sender");
    let vid = sender.video_handles()[0];
    sender
        .send_video_to(vid, &build_h264_au(0x11), Pts90khz::new(0), true)
        .expect("first send");
    let before = pat_versions(&sink.wire_bytes()).len();
    assert!(before >= 1, "the first push emits a PAT");

    sender
        .reconfigure(config(false, VideoCodec::H264))
        .expect("reconfigure");
    sender
        .send_video(&build_h264_au(0x22), Pts90khz::new(3000), true)
        .expect("send after reconfigure");

    let versions = pat_versions(&sink.wire_bytes());
    assert_eq!(
        versions.get(before).copied(),
        Some(1),
        "first PAT after reconfigure carries version 1, got {versions:?}"
    );
}

#[test]
fn refused_reconfigure_maps_to_mux_source() {
    let sender =
        MuxSender::new(ScriptedSend::ok(), config(true, VideoCodec::H264)).expect("sender");
    let err = sender
        .reconfigure(config(true, VideoCodec::H265))
        .expect_err("codec change refused");
    assert!(
        matches!(
            err.source,
            MuxSenderErrorSource::Mux(MuxError::ConfigInvalid { .. })
        ),
        "got {:?}",
        err.source
    );
}

#[test]
fn reconfigure_after_close_is_the_closed_error() {
    let sender =
        MuxSender::new(ScriptedSend::ok(), config(true, VideoCodec::H264)).expect("sender");
    sender.close();
    let closed = sender
        .send_video(&build_h264_au(1), Pts90khz::new(0), true)
        .expect_err("send after close");
    let err = sender
        .reconfigure(config(false, VideoCodec::H264))
        .expect_err("reconfigure after close");
    assert_eq!(err.kind, closed.kind);
    assert_eq!(format!("{:?}", err.source), format!("{:?}", closed.source));
}
