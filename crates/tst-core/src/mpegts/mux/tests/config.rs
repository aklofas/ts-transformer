//! `MuxerConfig` construction and validation tests.
//!
//! Declared from `mux/mod.rs` via `#[path = "tests/config.rs"] mod tests_config;`
//! so `super` here is the `mpegts::mux` module. `use super::*` brings in the
//! full mux namespace including `pub(super)` helpers from `mux::config`.

use super::*;
use crate::mpegts::common::Pts90khz;
use crate::test_prelude::*;

#[test]
fn default_config_validates() {
    MuxerConfig::default().validate().expect("default is valid");
}

#[test]
fn audio_codec_real_variants() {
    let codecs = [
        AudioCodec::Mp2,
        AudioCodec::Aac,
        AudioCodec::AacLatm,
        AudioCodec::Ac3,
    ];
    // Trivially constructible; equality holds.
    assert_ne!(codecs[0], codecs[1]);
}

#[test]
fn stream_spec_audio_variant() {
    let spec = StreamSpec::Audio {
        pid: 0x300,
        codec: AudioCodec::Aac,
        language: None,
    };
    assert_eq!(spec.pid(), 0x300);
}

#[test]
fn stream_spec_subtitle_variant() {
    let spec = StreamSpec::Subtitle {
        pid: 0x400,
        codec: SubtitleCodec::WebVttInTs,
    };
    assert_eq!(spec.pid(), 0x400);
}

#[test]
fn rejects_video_pid_zero() {
    let mut cfg = MuxerConfig::default();
    if let Some(StreamSpec::Video { pid, .. }) = cfg.programs[0]
        .streams
        .iter_mut()
        .find(|s| matches!(s, StreamSpec::Video { .. }))
    {
        *pid = 0x0000;
    }
    assert!(matches!(
        cfg.validate(),
        Err(MuxError::InvalidConfig(
            "video pid must be in 0x0010..=0x1FFE"
        ))
    ));
}

#[test]
fn rejects_klv_pid_null() {
    let mut cfg = MuxerConfig::default();
    if let Some(StreamSpec::Klv { pid, .. }) = cfg.programs[0]
        .streams
        .iter_mut()
        .find(|s| matches!(s, StreamSpec::Klv { .. }))
    {
        *pid = 0x1FFF;
    }
    assert!(matches!(
        cfg.validate(),
        Err(MuxError::InvalidConfig(
            "klv pid must be in 0x0010..=0x1FFE"
        ))
    ));
}

#[test]
fn rejects_pid_collision() {
    let mut cfg = MuxerConfig::default();
    // Pin both video and KLV to the same PID.
    if let Some(StreamSpec::Klv { pid, .. }) = cfg.programs[0]
        .streams
        .iter_mut()
        .find(|s| matches!(s, StreamSpec::Klv { .. }))
    {
        *pid = 0x1011; // matches the default video PID
    }
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_unrelated_pcr_pid() {
    let mut cfg = MuxerConfig::default();
    cfg.programs[0].pcr_pid = Some(0x9999);
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_pcr_pid_pinned_to_klv() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x200, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.pcr_pid(0x101);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(matches!(
        cfg,
        Err(MuxError::KlvPidUsedAsPcrPid { pid: 0x101 })
    ));
}

#[test]
fn rejects_pcr_interval_zero() {
    let cfg = MuxerConfig {
        pcr_interval_ms: 0,
        ..Default::default()
    };
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_pcr_interval_over_100() {
    let cfg = MuxerConfig {
        pcr_interval_ms: 101,
        ..Default::default()
    };
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_psi_interval_too_small() {
    let cfg = MuxerConfig {
        psi_interval_ms: 0,
        ..Default::default()
    };
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_buffer_too_small() {
    let cfg = MuxerConfig {
        buffer_packets: 0,
        ..Default::default()
    };
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

#[test]
fn rejects_sync_without_pts() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, false); // carries_pts=false invalid
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(matches!(cfg, Err(MuxError::InvalidConfig(_))));
}

#[test]
fn accepts_async_with_pts_combo() {
    // 0x06 + PTS — the common-practice "sync KLV everyone recognizes"
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    let _ = cfg.expect("0x06 + PTS is valid");
}

#[test]
fn resolved_pcr_pid_default() {
    let cfg = MuxerConfig::default();
    // Default config has video at 0x1011; PCR falls to the first video PID.
    let prog = &cfg.programs[0];
    assert!(prog.pcr_pid.is_none());
    // The first video pid from streams
    let first_video = prog
        .streams
        .iter()
        .find_map(|s| {
            if let StreamSpec::Video { pid, .. } = s {
                Some(*pid)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(first_video, 0x1011);
}

#[test]
fn resolved_pcr_pid_explicit() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x200, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.pcr_pid(0x200);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    assert_eq!(cfg.programs[0].pcr_pid, Some(0x200));
}

#[test]
fn muxer_constructs_with_valid_config() {
    let m = Muxer::new(MuxerConfig::default());
    assert!(m.is_ok());
}

#[test]
fn muxer_rejects_invalid_config() {
    let cfg = MuxerConfig {
        buffer_packets: 0,
        ..Default::default()
    };
    assert!(Muxer::new(cfg).is_err());
}

#[test]
fn config_rejects_empty_streams() {
    // A program with zero streams must fail validation.
    let cfg = {
        let prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_err());
}

#[test]
fn config_rejects_duplicate_pids() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x100, KlvStreamType::PrivateData, false); // same PID
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_err());
}

#[test]
fn config_pcr_pid_must_match_stream() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.pcr_pid(0x9999); // no stream has this PID
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_err());
}

#[test]
fn config_validate_accepts_dual_video_plus_klv() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_video(0x101, VideoCodec::H265);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_ok());
}

#[test]
fn config_validate_accepts_dual_klv_plus_video() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        prog.add_klv(0x103, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_ok());
}

#[test]
fn config_validate_rejects_seventeen_video_streams() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    for i in 0..17 {
        prog.add_video(0x100 + i as u16, VideoCodec::H264);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let err = b.build().unwrap_err();
    assert!(
        matches!(err, MuxError::TooManyVideoStreams { count: 17, cap: 16 }),
        "expected TooManyVideoStreams {{ 17, 16 }}, got {err:?}",
    );
}

#[test]
fn config_validate_rejects_seventeen_klv_streams() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    for i in 0..17 {
        prog.add_klv(0x200 + i as u16, KlvStreamType::PrivateData, false);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let err = b.build().unwrap_err();
    assert!(
        matches!(err, MuxError::TooManyKlvStreams { count: 17, cap: 16 }),
        "expected TooManyKlvStreams {{ 17, 16 }}, got {err:?}",
    );
}

#[test]
fn muxer_new_accepts_video_only() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    assert!(Muxer::new(cfg).is_ok());
}

#[test]
fn muxer_new_accepts_video_plus_klv() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    assert!(Muxer::new(cfg).is_ok());
}

#[test]
fn config_validate_rejects_too_many_audio_streams() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    for i in 0..17 {
        prog.add_audio(0x300 + i as u16, AudioCodec::Aac);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let err = b.build().unwrap_err();
    assert!(
        matches!(err, MuxError::TooManyAudioStreams { count: 17, cap: 16 }),
        "expected TooManyAudioStreams {{ 17, 16 }}, got {err:?}",
    );
}

#[test]
fn pcr_falls_back_to_first_audio_pid_for_audio_only_program() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_audio(0x300, AudioCodec::Aac);
        prog.add_audio(0x301, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    // First audio PID = 0x300, no video, no KLV → 0x300 wins PCR.
    assert_eq!(muxer.pcr_pid_for_program(0).unwrap(), 0x300);
}

#[test]
fn validate_accepts_audio_as_pcr() {
    // AAC frames push at ~21 ms intervals — within the 100 ms ETSI TR
    // 101 290 ceiling. Audio-as-PCR remains permitted.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_audio(0x201, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    let _ = cfg.expect("audio-as-PCR fallback is fine");
}

#[test]
fn validate_language_code_accepts_uppercase_per_en_300_468() {
    // ISO/IEC 8859-1 character coding does not mandate lowercase. Real-world
    // DVB encoders sometimes emit uppercase ISO 639-2 codes.
    assert!(
        validate_language_code(*b"ENG").is_ok(),
        "uppercase ASCII alphabetic must validate"
    );
    assert!(
        validate_language_code(*b"eng").is_ok(),
        "lowercase still accepted"
    );
    assert!(
        validate_language_code(*b"EnG").is_ok(),
        "mixed case accepted"
    );
    // Non-letters still rejected — admitting digits/symbols would let
    // junk through.
    assert!(validate_language_code(*b"123").is_err());
    assert!(validate_language_code(*b"e n").is_err());
    assert!(validate_language_code([0x00, 0x01, 0x02]).is_err());
}

// ─── StreamSpec::Data validation ────────────────────────────────────────────

/// One video + one data stream; `descs` (if any) go on the data stream.
fn data_prog(stream_type: u8, descs: Vec<Vec<u8>>) -> MuxerProgramConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_data(0x1100, stream_type, true);
    if !descs.is_empty() {
        prog.stream_descriptors_for_data(0, descs).unwrap();
    }
    prog.build()
}

fn build_cfg(prog: MuxerProgramConfig) -> Result<MuxerConfig, MuxError> {
    let mut b = MuxerConfig::builder();
    b.add_program(prog);
    b.build()
}

#[test]
fn data_user_private_stream_types_accepted() {
    // User-private bytes, bare 0x06, and unclassified 0x87 (E-AC-3) all
    // re-demux as Unknown — the write-side dual holds, so validate accepts.
    for st in [0xF0u8, 0xF1, 0x06, 0x87] {
        let cfg = build_cfg(data_prog(st, vec![]));
        assert!(
            cfg.is_ok(),
            "stream_type 0x{st:02X} should be accepted, got {:?}",
            cfg.err()
        );
    }
}

#[test]
fn data_typed_stream_types_rejected() {
    // Every stream_type the demux cascade classifies as a typed kind must
    // be rejected — use the typed StreamSpec variant instead.
    for st in [0x1Bu8, 0x24, 0x33, 0x15, 0x03, 0x04, 0x0F, 0x11, 0x81] {
        let err = build_cfg(data_prog(st, vec![])).unwrap_err();
        match err {
            MuxError::ConfigInvalid { ref reason } => assert!(
                reason.contains(&format!("stream_type 0x{st:02X}")),
                "reason for stream_type 0x{st:02X} should name it, got: {reason}"
            ),
            other => panic!("expected ConfigInvalid for stream_type 0x{st:02X}, got {other:?}"),
        }
    }
}

#[test]
fn data_0x06_masquerade_descriptors_rejected() {
    // 0x06 + a classifying descriptor re-demuxes as a typed kind (KLV /
    // AV1 video / subtitle) — a Data stream must not masquerade as one.
    let masquerades: [Vec<u8>; 7] = [
        b"\x05\x04KLVA".to_vec(),
        b"\x05\x04AV01".to_vec(),
        b"\x05\x04VTTC".to_vec(),
        b"\x05\x04GA94".to_vec(),
        // subtitling_descriptor (tag 0x59)
        vec![0x59, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x02],
        // teletext_descriptor (tag 0x56)
        vec![0x56, 0x05, b'e', b'n', b'g', 0x08, 0x88],
        // VBI_teletext_descriptor (tag 0x46) — same body shape as 0x56
        vec![0x46, 0x05, b'e', b'n', b'g', 0x08, 0x88],
    ];
    for tlv in masquerades {
        let err = build_cfg(data_prog(0x06, vec![tlv.clone()])).unwrap_err();
        assert!(
            matches!(err, MuxError::ConfigInvalid { .. }),
            "descriptor {tlv:02X?} on 0x06 should be rejected, got {err:?}"
        );
    }
}

#[test]
fn data_0x06_benign_descriptors_accepted() {
    // Unrecognized registration identifier on 0x06 stays Unknown.
    let _ = build_cfg(data_prog(0x06, vec![b"\x05\x04ABCD".to_vec()]))
        .expect("unrecognized registration ABCD is benign");
    // Private name tag (0xFF) on a user-private stream_type stays Unknown.
    let _ = build_cfg(data_prog(0xF0, vec![b"\xFF\x0ASERIAL_ADF".to_vec()]))
        .expect("private name tag on 0xF0 is benign");
}

#[test]
fn data_pid_rejected_as_pcr_pid() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
    prog.add_video(0x1011, VideoCodec::H264);
    prog.add_data(0x1100, 0xF0, true);
    prog.pcr_pid(0x1100);
    let err = build_cfg(prog.build()).unwrap_err();
    assert!(
        matches!(err, MuxError::DataPidUsedAsPcrPid { pid: 0x1100 }),
        "expected DataPidUsedAsPcrPid {{ pid: 0x1100 }}, got {err:?}"
    );
}

#[test]
fn data_only_program_rejected_no_pcr_eligible_stream() {
    // Data streams are excluded from the PCR fallback chain, so a
    // data-only program trips the no-PCR-eligible-stream guard.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
    prog.add_data(0x1100, 0xF0, true);
    let err = build_cfg(prog.build()).unwrap_err();
    assert!(
        matches!(err, MuxError::NoPcrEligibleStream { program_number: 1 }),
        "expected NoPcrEligibleStream {{ program_number: 1 }}, got {err:?}"
    );
}

#[test]
fn data_malformed_descriptor_rejected_by_existing_walk() {
    // Bad length byte (declares 9, body is 1): must surface as
    // MalformedDescriptor from the well-formedness walk, NOT panic in the
    // classify rule — pins the ordering (walk BEFORE classify parse).
    let err = build_cfg(data_prog(0xF0, vec![vec![0xFF, 0x09, b'X']])).unwrap_err();
    assert!(
        matches!(err, MuxError::MalformedDescriptor { .. }),
        "expected MalformedDescriptor, got {err:?}"
    );
    // 1-byte TLV: would panic on tlv[2..] if the classify rule parsed it
    // first — the true ordering pin.
    let err = build_cfg(data_prog(0xF0, vec![vec![0xFF]])).unwrap_err();
    assert!(
        matches!(err, MuxError::MalformedDescriptor { .. }),
        "expected MalformedDescriptor for 1-byte TLV, got {err:?}"
    );
}

#[test]
fn data_cap_enforced() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
    prog.add_video(0x1011, VideoCodec::H264);
    for i in 0..17 {
        prog.add_data(0x1100 + i as u16, 0xF0, true);
    }
    let err = build_cfg(prog.build()).unwrap_err();
    assert!(
        matches!(err, MuxError::TooManyDataStreams { count: 17, cap: 16 }),
        "expected TooManyDataStreams {{ 17, 16 }}, got {err:?}"
    );
}

#[test]
fn stream_kind_display() {
    assert_eq!(MuxStreamKind::Video.to_string(), "video");
    assert_eq!(MuxStreamKind::Audio.to_string(), "audio");
    assert_eq!(MuxStreamKind::Klv.to_string(), "klv");
    assert_eq!(MuxStreamKind::Subtitle.to_string(), "subtitle");
}

#[test]
fn teletext_field_display() {
    assert_eq!(TeletextField::MagazineNumber.to_string(), "magazine_number");
    assert_eq!(TeletextField::TeletextType.to_string(), "teletext_type");
}

// Keep Pts90khz import active — some helper configurations need it transitively.
const _: fn() = || {
    let _ = Pts90khz::new(0);
};
