//! Guard: feeding already-carried (binding-framed) bytes to the
//! WRAPPING push must return a typed error, never emit an empty AU.
use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

#[test]
fn wrapping_push_rejects_already_framed_av1_input() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    // Binding-framed bytes (what the demuxer produces in ev.raw): a
    // ts_open_bitstream_unit start code 00 00 01 + escaped OBU body.
    // The leading 0x00 has obu_has_size_field=0, so the wrap loop
    // bails at offset 0 without consuming all input.
    let framed = [0x00u8, 0x00, 0x01, 0x0A, 0x00];
    let err = mux
        .push_video_to(h, &framed, Pts90khz::new(90_000), true)
        .unwrap_err();
    assert!(matches!(err, MuxError::InvalidAv1Obu));
}
