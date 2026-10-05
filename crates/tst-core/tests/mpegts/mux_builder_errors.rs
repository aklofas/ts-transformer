//! Regression tests: descriptor-index builder methods must never panic;
//! out-of-range indices surface as [`MuxError::DescriptorIndexOutOfRange`]
//! from the descriptor setter call itself (immediate-error semantics).

use tst_core::MuxError;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, MuxerProgramConfigBuilder, StreamKind, SubtitleCodec, VideoCodec,
};

#[test]
fn stream_descriptors_for_video_out_of_range_does_not_panic() {
    // Only one video stream (index 0); index 5 is out of range.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let result =
        prog.stream_descriptors_for_video(5, vec![vec![0x05, 0x04, 0x4B, 0x4C, 0x56, 0x41]]);
    match result {
        Err(MuxError::DescriptorIndexOutOfRange {
            kind,
            index,
            program_number,
        }) => {
            assert_eq!(kind, StreamKind::Video);
            assert_eq!(index, 5);
            assert_eq!(program_number, 1);
        }
        other => panic!("expected DescriptorIndexOutOfRange, got {:?}", other),
    }
}

#[test]
fn stream_descriptors_for_klv_out_of_range_does_not_panic() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
    let result = prog.stream_descriptors_for_klv(5, vec![vec![0x05, 0x04, 0x4B, 0x4C, 0x56, 0x41]]);
    match result {
        Err(MuxError::DescriptorIndexOutOfRange {
            kind,
            index,
            program_number,
        }) => {
            assert_eq!(kind, StreamKind::Klv);
            assert_eq!(index, 5);
            assert_eq!(program_number, 1);
        }
        other => panic!("expected DescriptorIndexOutOfRange, got {:?}", other),
    }
}

#[test]
fn stream_descriptors_for_audio_out_of_range_does_not_panic() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_audio(0x1021, AudioCodec::Aac);
    let result = prog.stream_descriptors_for_audio(5, vec![vec![]]);
    match result {
        Err(MuxError::DescriptorIndexOutOfRange {
            kind,
            index,
            program_number,
        }) => {
            assert_eq!(kind, StreamKind::Audio);
            assert_eq!(index, 5);
            assert_eq!(program_number, 1);
        }
        other => panic!("expected DescriptorIndexOutOfRange, got {:?}", other),
    }
}

#[test]
fn stream_descriptors_for_subtitle_out_of_range_does_not_panic() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_subtitle(
        0x1041,
        SubtitleCodec::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
    );
    let result = prog.stream_descriptors_for_subtitle(5, vec![vec![]]);
    match result {
        Err(MuxError::DescriptorIndexOutOfRange {
            kind,
            index,
            program_number,
        }) => {
            assert_eq!(kind, StreamKind::Subtitle);
            assert_eq!(index, 5);
            assert_eq!(program_number, 1);
        }
        other => panic!("expected DescriptorIndexOutOfRange, got {:?}", other),
    }
}

#[test]
fn stream_descriptors_for_stream_out_of_range_does_not_panic() {
    // One stream total (abs index 0); abs_idx 99 is out of range.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let result = prog.stream_descriptors_for_stream(99, vec![vec![]]);
    assert!(
        matches!(
            result,
            Err(MuxError::AbsIndexOutOfRange {
                abs_idx: 99,
                len: 1,
                program_number: 1
            })
        ),
        "expected AbsIndexOutOfRange, got {:?}",
        result
    );
}
