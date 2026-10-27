//! Integration tests for sender-side audio carriage in `mpegts::mux`.

use crate::common::mux_demux::es_entries;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

#[test]
fn audio_only_program_mux_produces_pat_pmt_audio_pes() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_audio(0x300, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let frames = vec![
        0xFF, 0xFD, 0x00, 0x10, /* MP2 frame header */ 0xDE, 0xAD, 0xBE, 0xEF,
    ];
    muxer.push_audio(&frames, Pts90khz::new(90_000)).unwrap();

    let mut buf = vec![0u8; 188 * 64];
    let n = muxer.pull(&mut buf);
    assert!(n > 0);
    // PAT, PMT, audio PES all present.
    let packets: Vec<&[u8]> = buf[..n].chunks_exact(188).collect();
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0)
    ); // PAT
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1000)
    ); // PMT
    assert!(
        packets
            .iter()
            .any(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x300)
    ); // audio PES
}

#[test]
fn three_stream_program_audio_video_klv_routing() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.add_klv(0x200, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();

    let video_handle = muxer.video_handles()[0];
    let audio_handle = muxer.audio_handles()[0];
    let klv_handle = muxer.klv_handles()[0];

    let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F]; // tiny SPS
    muxer
        .push_video_to(video_handle, &nal, Pts90khz::new(90_000), true)
        .unwrap();
    muxer
        .push_audio_to(audio_handle, Pts90khz::new(90_000), b"audio_payload_bytes")
        .unwrap();
    muxer
        .push_klv_to(klv_handle, b"klv_record_bytes", Pts90khz::new(90_000), 0x00)
        .unwrap();

    let mut buf = vec![0u8; 188 * 256];
    let n = muxer.pull(&mut buf);
    assert!(n > 0);

    // Confirm PESs land on the correct PIDs.
    let pids: std::collections::BTreeSet<u16> = buf[..n]
        .chunks_exact(188)
        .map(|p| ((p[1] as u16 & 0x1F) << 8) | p[2] as u16)
        .collect();
    assert!(pids.contains(&0x100), "video PID present");
    assert!(pids.contains(&0x200), "klv PID present");
    assert!(pids.contains(&0x300), "audio PID present");
}

#[test]
fn each_audio_codec_has_correct_pmt_stream_type() {
    let codecs = [
        (AudioCodec::Mp2, 0x03),
        (AudioCodec::Aac, 0x0F),
        (AudioCodec::AacLatm, 0x11),
        (AudioCodec::Ac3, 0x81),
    ];
    for (codec, expected_st) in codecs {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x300, codec);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = Muxer::new(cfg).unwrap();
        muxer.push_audio(b"x", Pts90khz::new(90_000)).unwrap();

        let mut buf = vec![0u8; 188 * 16];
        let n = muxer.pull(&mut buf);
        let pmt = buf[..n]
            .chunks_exact(188)
            .find(|p| (((p[1] as u16 & 0x1F) << 8) | p[2] as u16) == 0x1000)
            .expect("a PMT packet on PID 0x1000");
        let st = es_entries(pmt)
            .find(|(pid, _)| *pid == 0x300)
            .expect("PID 0x300 in the PMT")
            .1;
        assert_eq!(st, expected_st, "codec {codec:?} stream_type mismatch");
    }
}
