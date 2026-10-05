//! Integration tests for receiver-side audio carriage in `mpegts::demux`.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    Demuxer,
    event::{AudioCodec, DemuxEvent, SamplePayload},
};
use tst_core::mpegts::mux::{
    AudioCodec as MuxAudioCodec, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Helper: mux audio + video, drain bytes, return them.
fn mux_audio_video(codec: MuxAudioCodec, audio_pid: u16) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(audio_pid, codec);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F];
    muxer.push_video(&nal, Pts90khz::new(90_000), true).unwrap();
    muxer
        .push_audio(b"synthetic_audio_frame_payload", Pts90khz::new(90_000))
        .unwrap();
    let mut buf = vec![0u8; 188 * 256];
    let n = muxer.pull(&mut buf);
    buf.truncate(n);
    buf
}

#[test]
fn demux_classifies_each_audio_codec_correctly() {
    let cases = [
        (MuxAudioCodec::Mp2, AudioCodec::Mp2),
        (MuxAudioCodec::Aac, AudioCodec::Aac),
        (MuxAudioCodec::AacLatm, AudioCodec::AacLatm),
        (MuxAudioCodec::Ac3, AudioCodec::Ac3),
    ];
    for (mux_codec, expected_demux_codec) in cases {
        let bytes = mux_audio_video(mux_codec, 0x300);
        let mut demuxer = Demuxer::new();
        demuxer.feed(&bytes).unwrap();
        demuxer.flush();

        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }

        let sample = events
            .iter()
            .find(|e| {
                matches!(
                    e,
                    DemuxEvent::Sample {
                        payload: SamplePayload::Audio { .. },
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("no audio sample for {mux_codec:?}"));
        if let DemuxEvent::Sample {
            payload: SamplePayload::Audio { codec, frames },
            ..
        } = sample
        {
            assert_eq!(*codec, expected_demux_codec);
            assert!(!frames.is_empty(), "audio frames non-empty");
        }
    }
}

#[test]
fn demux_audio_pts_surfaces() {
    let bytes = mux_audio_video(MuxAudioCodec::Aac, 0x300);
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).unwrap();
    demuxer.flush();

    let mut events = Vec::new();
    while let Some(e) = demuxer.next_event() {
        events.push(e);
    }

    let sample = events
        .iter()
        .find_map(|e| match e {
            DemuxEvent::Sample {
                pts,
                dts,
                payload: SamplePayload::Audio { .. },
                ..
            } => Some((pts, dts)),
            _ => None,
        })
        .unwrap();
    assert_eq!(sample.0.as_ticks(), 90_000);
    assert_eq!(*sample.1, None, "audio always has dts: None");
}

/// AC-3 PES with `data_alignment_indicator=1` MUST start
/// with the syncword 0x0B77 (ATSC A/52:2018 §A.6.3). The mux side sets the
/// flag unconditionally for AC-3 (`mpegts::mux::pes::write_audio_pes`), so
/// any caller pushing non-syncframe bytes (a stub payload, a mis-aligned
/// upstream demux) triggers this issue on the receive side.
#[test]
fn demux_ac3_payload_missing_syncword_surfaces_nonconformant() {
    use tst_core::mpegts::demux::event::NonConformantIssue;

    // mux_audio_video pushes b"synthetic_audio_frame_payload" — first
    // bytes are 0x73, 0x79, NOT 0x0B 0x77 — the AC-3 syncword contract violation.
    let bytes = mux_audio_video(MuxAudioCodec::Ac3, 0x300);
    let mut demuxer = Demuxer::new();
    demuxer.feed(&bytes).unwrap();
    demuxer.flush();

    let mut nonconformant_count = 0;
    let mut sample_count = 0;
    while let Some(e) = demuxer.next_event() {
        match e {
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::Ac3SyncMissing { pid },
                ..
            } => {
                assert_eq!(pid, 0x300);
                nonconformant_count += 1;
            }
            DemuxEvent::Sample {
                payload:
                    SamplePayload::Audio {
                        codec: AudioCodec::Ac3,
                        ..
                    },
                ..
            } => {
                sample_count += 1;
            }
            _ => {}
        }
    }
    assert!(
        nonconformant_count >= 1,
        "expected ≥1 Ac3SyncMissing event, got {nonconformant_count}"
    );
    // Lenient mode (default) still surfaces the sample alongside the
    // nonconformant issue — caller can correlate.
    assert!(
        sample_count >= 1,
        "lenient mode should still emit the sample"
    );
}

/// A valid AC-3 syncframe starting with 0x0B77 should NOT
/// trigger Ac3SyncMissing. Smoke test using a minimal synthetic syncframe.
#[test]
fn demux_ac3_payload_with_syncword_no_nonconformant() {
    use tst_core::mpegts::demux::event::NonConformantIssue;

    // Build a minimal valid AC-3 syncframe (just header bytes; body is
    // zero-padded). 48 kHz, frmsizecod=20 (192 kbps, frame_length=768
    // bytes), bsid=8, bsmod=0, acmod=2, lfeon=false. The demux checks
    // only the first two bytes (0x0B 0x77) — body content irrelevant
    // for the AC-3 syncword contract.
    let mut frame = vec![0u8; 768];
    frame[0] = 0x0B;
    frame[1] = 0x77;
    // crc1 = 0 (don't care)
    // fscod=0, frmsizecod=20: byte 4 = (0<<6)|20 = 20
    frame[4] = 20;
    // bsid=8, bsmod=0: byte 5 = (8<<3)|0 = 0x40
    frame[5] = 0x40;
    // acmod=2, dsurmod=0, lfeon=0: byte 6 high bits = 0b010_00_0... = 0x40
    frame[6] = 0x40;

    let cfg = {
        let mut prog = tst_core::mpegts::mux::MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, tst_core::mpegts::mux::VideoCodec::H264);
        prog.add_audio(0x300, MuxAudioCodec::Ac3);
        let mut b = tst_core::mpegts::mux::MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = tst_core::mpegts::mux::Muxer::new(cfg).unwrap();
    let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1F];
    muxer.push_video(&nal, Pts90khz::new(90_000), true).unwrap();
    muxer.push_audio(&frame, Pts90khz::new(90_000)).unwrap();
    let mut buf = vec![0u8; 188 * 512];
    let n = muxer.pull(&mut buf);
    buf.truncate(n);

    let mut demuxer = Demuxer::new();
    demuxer.feed(&buf).unwrap();
    demuxer.flush();

    let mut sync_missing = 0;
    while let Some(e) = demuxer.next_event() {
        if let DemuxEvent::NonConformant {
            issue: NonConformantIssue::Ac3SyncMissing { .. },
            ..
        } = e
        {
            sync_missing += 1;
        }
    }
    assert_eq!(
        sync_missing, 0,
        "valid syncword should not trigger Ac3SyncMissing"
    );
}
