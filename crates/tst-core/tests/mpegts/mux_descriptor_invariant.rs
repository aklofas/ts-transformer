//! The `MuxerProgramConfig.stream_descriptors`
//! length-mismatch invariant is enforced via the rich
//! `MuxError::ConfigInvalid { reason: String }` variant. The reason
//! string names the offending program_number plus both lengths so
//! callers can locate the bug in a multi-program config without
//! re-reading the code.

use tst_core::error::MuxError;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfig, StreamSpec, VideoCodec,
};

/// Hand-built `MuxerProgramConfig` with mismatched `stream_descriptors`
/// length is rejected by `Muxer::new` with the rich
/// `MuxError::ConfigInvalid` variant. The reason names the program
/// number and both lengths.
#[test]
fn muxer_new_rejects_descriptor_length_mismatch_with_rich_diagnostic() {
    let mut prog = MuxerProgramConfig::new(7, 0x1000);
    prog.streams = vec![
        StreamSpec::Video {
            pid: 0x1011,
            codec: VideoCodec::H264,
        },
        StreamSpec::Klv {
            pid: 0x1031,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    // INTENTIONAL MISMATCH: 2 streams but only 1 descriptor list.
    prog.stream_descriptors = vec![vec![]];
    let mut cfg = MuxerConfig::default();
    cfg.programs = vec![prog];
    cfg.pcr_interval_ms = 40;
    cfg.psi_interval_ms = 100;
    cfg.buffer_packets = 10_000;
    let result = Muxer::new(cfg);
    let err = match result {
        Ok(_) => panic!("descriptor-length mismatch must be rejected"),
        Err(e) => e,
    };
    match err {
        MuxError::ConfigInvalid { reason } => {
            assert!(
                reason.contains("program 7"),
                "reason missing program_number: {reason}"
            );
            assert!(
                reason.contains("2 streams"),
                "reason missing actual streams.len: {reason}"
            );
            assert!(
                reason.contains("1 stream_descriptors"),
                "reason missing actual stream_descriptors.len: {reason}",
            );
        }
        other => panic!("expected MuxError::ConfigInvalid, got {other:?}"),
    }
}

/// Inverse: matched lengths construct the Muxer successfully.
#[test]
fn muxer_new_accepts_descriptor_length_match() {
    let mut prog = MuxerProgramConfig::new(1, 0x1000);
    prog.streams = vec![StreamSpec::Video {
        pid: 0x1011,
        codec: VideoCodec::H264,
    }];
    // 1 stream, 1 descriptor list (empty is fine).
    prog.stream_descriptors = vec![vec![]];
    let mut cfg = MuxerConfig::default();
    cfg.programs = vec![prog];
    cfg.pcr_interval_ms = 40;
    cfg.psi_interval_ms = 100;
    cfg.buffer_packets = 10_000;
    let result = Muxer::new(cfg);
    let err = result.err();
    assert!(
        err.is_none(),
        "matched lengths must construct: {:?}",
        err.unwrap()
    );
}
