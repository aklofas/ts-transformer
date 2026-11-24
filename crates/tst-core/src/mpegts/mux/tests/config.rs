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
fn pcr_lead_defaults_to_100ms() {
    assert_eq!(MuxerConfig::default().pcr_lead_ms, 100);
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    assert_eq!(b.build().unwrap().pcr_lead_ms, 100);
    b.pcr_lead_ms(250);
    assert_eq!(b.build().unwrap().pcr_lead_ms, 250);
}

/// The lead may not exceed the one second an H.222.0 decoder buffer holds
/// an access unit.
#[test]
fn rejects_pcr_lead_over_one_second() {
    let with_lead = |pcr_lead_ms| MuxerConfig {
        pcr_lead_ms,
        ..Default::default()
    };
    assert!(matches!(
        with_lead(1001).validate(),
        Err(MuxError::InvalidConfig("pcr_lead_ms must be in 0..=1000"))
    ));
    assert!(with_lead(1000).validate().is_ok());
    assert!(with_lead(0).validate().is_ok());
}

#[test]
fn rejects_psi_interval_too_small() {
    let cfg = MuxerConfig {
        psi_interval_ms: 0,
        ..Default::default()
    };
    assert!(matches!(cfg.validate(), Err(MuxError::InvalidConfig(_))));
}

/// A PSI version number is five bits.
#[test]
fn rejects_a_psi_version_base_above_31() {
    for base in [32, 255] {
        let mut b = MuxerConfig::builder();
        b.add_program(MuxerConfig::default().programs[0].clone())
            .psi_version_base(base);
        assert!(matches!(
            b.build(),
            Err(MuxError::InvalidConfig(
                "psi_version_base must be in 0..=31"
            ))
        ));
    }
    for base in [0, 7, 31] {
        let mut b = MuxerConfig::builder();
        b.add_program(MuxerConfig::default().programs[0].clone())
            .psi_version_base(base);
        assert_eq!(b.build().unwrap().psi_version_base, base);
    }
    for id in [0, 0xFFFF] {
        let mut b = MuxerConfig::builder();
        b.add_program(MuxerConfig::default().programs[0].clone())
            .transport_stream_id(id);
        assert_eq!(b.build().unwrap().transport_stream_id, id);
    }
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

#[test]
fn audio_language_is_validated_like_subtitle_language() {
    let with_language = |code: [u8; 3]| {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio_with_language(0x102, AudioCodec::Aac, code);
        build_cfg(prog.build())
    };
    for code in [[0, 1, 2], *b"e1g", *b"en ", *b"   "] {
        match with_language(code) {
            Err(MuxError::InvalidLanguageCode { code: got }) => assert_eq!(got, code),
            other => panic!("{code:02X?}: expected InvalidLanguageCode, got {other:?}"),
        }
    }
    for code in [*b"eng", *b"ENG"] {
        let _ = with_language(code).expect("ASCII letters build");
    }
}

#[test]
fn invalid_language_code_names_the_code_as_text() {
    let shown = |code: [u8; 3]| MuxError::InvalidLanguageCode { code }.to_string();
    assert_eq!(
        shown(*b"e1g"),
        "invalid ISO 639-2 language code \"e1g\" (must be 3 ASCII letters)"
    );
    assert_eq!(
        shown([0, 1, b'g']),
        "invalid ISO 639-2 language code \"\\x00\\x01g\" (must be 3 ASCII letters)"
    );
}

#[test]
fn data_stream_rejects_section_only_stream_types() {
    for st in [0x05u8, 0x13, 0x16, 0x2C, 0x2F, 0x30] {
        match build_cfg(data_prog(st, vec![])) {
            Err(MuxError::ConfigInvalid { reason }) => {
                assert!(reason.contains("sections"), "0x{st:02X}: {reason}");
                assert!(reason.contains(&format!("0x{st:02X}")), "{reason}");
            }
            other => panic!("0x{st:02X}: expected ConfigInvalid, got {other:?}"),
        }
    }
}

#[test]
fn data_stream_accepts_private_pes_types() {
    for st in [0x06u8, 0x80, 0x00, 0x0B, 0x0C, 0x0D, 0x14] {
        let cfg = build_cfg(data_prog(st, vec![]));
        assert!(cfg.is_ok(), "0x{st:02X}: {:?}", cfg.err());
    }
}

/// One program: video 0x101 plus, when `klv` is given, a KLV stream 0x102.
fn program_with_klv(
    number: u16,
    pmt_pid: u16,
    pids: u16,
    klv: Option<KlvStreamType>,
) -> MuxerProgramConfig {
    let mut prog = MuxerProgramConfigBuilder::new(number, pmt_pid);
    prog.add_video(pids + 1, VideoCodec::H264);
    if let Some(st) = klv {
        prog.add_klv(pids + 2, st, st == KlvStreamType::SynchronousMetadata);
    }
    prog.build()
}

fn with_psi_interval(ms: u32, programs: Vec<MuxerProgramConfig>) -> Result<MuxerConfig, MuxError> {
    let mut b = MuxerConfig::builder();
    for p in programs {
        b.add_program(p);
    }
    b.psi_interval_ms(ms);
    b.build()
}

#[test]
fn rejects_a_psi_interval_above_200_ms_when_a_program_carries_klv() {
    for klv in [
        KlvStreamType::SynchronousMetadata,
        KlvStreamType::PrivateData,
    ] {
        let one = || vec![program_with_klv(1, 0x1000, 0x100, Some(klv))];
        assert!(
            matches!(
                with_psi_interval(201, one()),
                Err(MuxError::InvalidConfig(_))
            ),
            "{klv:?}"
        );
        let r = with_psi_interval(200, one());
        assert!(r.is_ok(), "{klv:?}: {r:?}");
    }
    // The default configuration declares an asynchronous KLV stream.
    let raised = |ms| MuxerConfig {
        psi_interval_ms: ms,
        ..Default::default()
    };
    assert!(matches!(
        raised(201).validate(),
        Err(MuxError::InvalidConfig(_))
    ));
    assert!(raised(200).validate().is_ok());
    // Without KLV in any program the interval is not bounded above.
    for ms in [201, 60_000] {
        let r = with_psi_interval(
            ms,
            vec![
                program_with_klv(1, 0x1000, 0x100, None),
                program_with_klv(2, 0x1100, 0x200, None),
            ],
        );
        assert!(r.is_ok(), "{ms}: {r:?}");
    }
    // A KLV stream in the second program only is enough.
    assert!(matches!(
        with_psi_interval(
            201,
            vec![
                program_with_klv(1, 0x1000, 0x100, None),
                program_with_klv(2, 0x1100, 0x200, Some(KlvStreamType::PrivateData)),
            ],
        ),
        Err(MuxError::InvalidConfig(_))
    ));
}

#[test]
fn reconfigure_refuses_adding_klv_above_the_psi_bound() {
    let without = with_psi_interval(1_000, vec![program_with_klv(1, 0x1000, 0x100, None)]).unwrap();
    let mut m = Muxer::new(without.clone()).unwrap();
    let with = MuxerConfig {
        programs: vec![program_with_klv(
            1,
            0x1000,
            0x100,
            Some(KlvStreamType::SynchronousMetadata),
        )],
        ..without.clone()
    };
    let r = m.reconfigure(with);
    assert!(matches!(r, Err(MuxError::InvalidConfig(_))), "{r:?}");
    assert_eq!(
        format!("{:?}", m.config),
        format!("{without:?}"),
        "a refused reconfigure leaves the muxer unchanged"
    );
}
