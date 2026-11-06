//! `MuxerConfig::validate` edge cases: descriptor count mismatch, malformed
//! descriptor bytes, oversized PMT, builder routing by stream-class index,
//! and KLV PMT descriptor cache auto-emit / suppression rules.

use super::*;

#[test]
fn default_config_has_empty_per_stream_descriptors() {
    let cfg = MuxerConfig::default();
    let prog = &cfg.programs[0];
    assert_eq!(prog.stream_descriptors.len(), prog.streams.len());
    for descs in &prog.stream_descriptors {
        assert!(descs.is_empty());
    }
}

#[test]
fn validate_rejects_descriptor_count_mismatch() {
    let mut cfg = MuxerConfig::default();
    // streams has 2, overwrite with 1-entry descriptor vec
    cfg.programs[0].stream_descriptors = vec![Vec::new()];
    let err = cfg.validate().unwrap_err();
    assert!(matches!(err, MuxError::ConfigInvalid { .. }));
}

#[test]
fn validate_rejects_malformed_descriptor() {
    // Length byte claims 5 bytes of body but only 1 follows.
    let bad = vec![0xFF, 0x05, 0x00];
    let mut cfg = MuxerConfig::default();
    cfg.programs[0].stream_descriptors = vec![vec![bad], Vec::new()];
    let err = cfg.validate().unwrap_err();
    assert!(matches!(
        err,
        MuxError::MalformedDescriptor {
            stream_index: 0,
            descriptor_index: 0,
            ..
        }
    ));
}

#[test]
fn validate_rejects_oversized_pmt() {
    // 4 streams × 100-byte descriptor = ~400 bytes > 166 max.
    let big = crate::mpegts::descriptors::user_private(&[0u8; 100]).expect("100B within cap");
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        prog.add_klv(0x103, KlvStreamType::PrivateData, false);
        prog.stream_descriptors_for_stream(0, vec![big.clone()])
            .unwrap();
        prog.stream_descriptors_for_stream(1, vec![big.clone()])
            .unwrap();
        prog.stream_descriptors_for_stream(2, vec![big.clone()])
            .unwrap();
        prog.stream_descriptors_for_stream(3, vec![big]).unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(matches!(cfg, Err(MuxError::PmtTooLarge { .. })));
}

#[test]
fn builder_routes_video_descriptors_by_video_index() {
    // 2 video, 1 KLV. Setting video_index=1 should land on absolute index 2
    // (streams: [video0, klv, video1]).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x102, KlvStreamType::PrivateData, false);
        prog.add_video(0x101, VideoCodec::H264);
        prog.stream_descriptors_for_video(
            1,
            vec![crate::mpegts::descriptors::user_private(b"V2").expect("label within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let prog = &cfg.programs[0];
    assert_eq!(prog.stream_descriptors[0], Vec::<Vec<u8>>::new());
    assert_eq!(prog.stream_descriptors[1], Vec::<Vec<u8>>::new());
    assert_eq!(prog.stream_descriptors[2].len(), 1);
    assert_eq!(prog.stream_descriptors[2][0][0], 0xFF);
}

#[test]
fn builder_rejects_out_of_range_video_index() {
    // Out-of-range video_idx surfaces immediately from the descriptor
    // setter call.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    let result = prog.stream_descriptors_for_video(
        7,
        vec![crate::mpegts::descriptors::user_private(b"X").expect("label within cap")],
    );
    assert!(
        matches!(
            result,
            Err(MuxError::DescriptorIndexOutOfRange {
                kind: MuxStreamKind::Video,
                index: 7,
                program_number: 1,
            })
        ),
        "expected DescriptorIndexOutOfRange, got {:?}",
        result
    );
}

#[test]
fn cache_composes_auto_emit_then_caller_bytes_on_klv_private() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.stream_descriptors_for_klv(
            0,
            vec![crate::mpegts::descriptors::user_private(b"KLV_LBL").expect("label within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // Stream 0 (video) — no auto-emit, no caller — empty cache entry.
    assert!(muxer.slots[0].pmt_descriptor_cache[0].is_empty());

    // Stream 1 (KLV PrivateData) — KLVA Registration (6 bytes) +
    // user_private("KLV_LBL") (9 bytes) = 15 bytes.
    let entry = &muxer.slots[0].pmt_descriptor_cache[1];
    assert_eq!(entry.len(), 15);
    assert_eq!(&entry[..6], &[0x05, 0x04, b'K', b'L', b'V', b'A']);
    assert_eq!(entry[6], 0xFF); // user_private tag
    assert_eq!(entry[7], 7); // body length
    assert_eq!(&entry[8..], b"KLV_LBL");
}

#[test]
fn cache_suppresses_klva_auto_emit_when_caller_supplies_registration() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.stream_descriptors_for_klv(
            0,
            vec![crate::mpegts::descriptors::registration(*b"KLVA", &[]).expect("within cap")],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // Cache index 0 = video (empty), index 1 = KLV.
    // Caller's Registration only — auto-emit suppressed. Total = 6 bytes.
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1].len(), 6);
}

#[test]
fn cache_sync_klv_metadata_std_declares_buffer_for_largest_pes() {
    // H.222.0 §2.6.63 / §2.12.10: BSn and Rxn are literal STD parameters; a
    // zero buffer cannot hold any access unit. The automatic descriptor
    // declares an envelope that holds the largest PES this muxer emits.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let std_tlv = &muxer.slots[0].pmt_descriptor_cache[1][17..28];
    assert_eq!(
        std_tlv,
        &crate::mpegts::descriptors::metadata_std(
            crate::mpegts::mux::state::SYNC_KLV_STD_INPUT_LEAK_RATE,
            crate::mpegts::mux::state::SYNC_KLV_STD_BUFFER_UNITS,
            0,
        )[..]
    );
    let field = |i: usize| {
        ((std_tlv[i] as u32 & 0x3F) << 16) | ((std_tlv[i + 1] as u32) << 8) | std_tlv[i + 2] as u32
    };
    assert_eq!(field(2), 13_109);
    assert_eq!(field(5), 65);
    assert_eq!(field(8), 0);
    // The constant is tied to the PES cap, not chosen by hand.
    assert!(
        crate::mpegts::mux::state::SYNC_KLV_STD_BUFFER_UNITS as usize * 1024
            >= 6 + u16::MAX as usize
    );
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1].len(), 28);
}

#[test]
fn cache_auto_emits_klva_on_sync_klv() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
        prog.stream_descriptors_for_klv(
            0,
            vec![
                crate::mpegts::descriptors::metadata_klva(0x00),
                crate::mpegts::descriptors::metadata_std(0, 0, 0),
            ],
        )
        .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    // Cache index 0 = video (empty), index 1 = KLV.
    // KLVA auto-emit (6 bytes) prepended on SynchronousMetadata too.
    // 6 (KLVA) + 11 (0x26) + 11 (0x27) = 28 bytes.
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1].len(), 28);
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][0], 0x05); // KLVA Registration
    assert_eq!(&muxer.slots[0].pmt_descriptor_cache[1][2..6], b"KLVA");
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][6], 0x26);
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][17], 0x27);
    // The caller's zero-valued 0x27 survives verbatim.
    assert_eq!(
        &muxer.slots[0].pmt_descriptor_cache[1][17..28],
        &crate::mpegts::descriptors::metadata_std(0, 0, 0)[..]
    );
}

#[test]
fn cache_auto_emits_metadata_descriptors_on_sync_klv() {
    // MISB ST 1402.2 ST 1402-15/-16/-17: a Synchronous Metadata (0x15)
    // PMT entry SHALL carry a metadata_descriptor (0x26) for each metadata
    // service plus a single metadata_std_descriptor (0x27). The muxer must
    // auto-emit both even when the caller supplies NO descriptors — the
    // distinguishing case from `cache_auto_emits_klva_on_sync_klv` above,
    // which supplies them by hand.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
        // NOTE: deliberately no stream_descriptors_for_klv call.
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    // Cache index 0 = video (empty), index 1 = KLV.
    // 6 (KLVA) + 11 (0x26 metadata_descriptor) + 11 (0x27 metadata_std) = 28.
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1].len(), 28);
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][0], 0x05); // KLVA Registration
    assert_eq!(&muxer.slots[0].pmt_descriptor_cache[1][2..6], b"KLVA");
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][6], 0x26); // metadata_descriptor
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][10], 0xFF); // metadata_format (defined by id)
    assert_eq!(&muxer.slots[0].pmt_descriptor_cache[1][11..15], b"KLVA"); // metadata_format_identifier
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][17], 0x27); // metadata_std_descriptor
}

#[test]
fn cache_async_klv_does_not_emit_metadata_descriptors() {
    // Async KLV (stream_type 0x06) carries ONLY the KLVA registration per
    // RP 217 / ST 1402.2 §9.4.2 — no metadata_descriptor / metadata_std.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1].len(), 6); // KLVA only
    assert_eq!(muxer.slots[0].pmt_descriptor_cache[1][0], 0x05);
}

/// The PMT size estimator must count the auto-emitted 5-byte AC-3
/// `audio_stream_descriptor` (tag 0x81). Without it, a config with enough
/// AC-3 streams passes `validate()` but produces an over-large PMT; it must
/// get `PmtTooLarge` from `MuxerConfig::validate` (in `config.rs`).
#[test]
fn validate_counts_ac3_audio_stream_descriptor_and_rejects_oversized_pmt() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    for i in 0..15u16 {
        prog.add_audio(0x0100 + i, AudioCodec::Ac3);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    assert!(
        matches!(b.build(), Err(MuxError::PmtTooLarge { .. })),
        "15 AC-3 streams (entry 5 + Registration 6 + audio_stream_descriptor 5 = 16 B each) \
         overflow the single-packet PMT; validate() must reject, not accept-then-panic"
    );
}

/// The PMT size estimator must NOT add subtitle
/// auto-emit bytes when the caller has already supplied a recognized
/// subtitle descriptor (tag 0x59 / 0x56 / 0x46, or GA94 / VTTC
/// registration). The old estimator always added the subtitle auto-emit
/// size regardless, causing `PmtTooLarge` for configs the emitter accepted.
///
/// Budget proof (MAX_PMT_SECTION_BYTES = 183):
///   fixed header      = 16 B (3 + 9 + CRC 4)
///   1 video ES (H264) =  5 B (stream overhead only; no auto-emit for H264)
///   1 subtitle ES     =  5 B stream overhead
///                      + 10 B caller-supplied 0x59 subtitling_descriptor
///                      + 147 B user-private padding (tag 1 + len 1 + body 145)
///                      = 162 B
///   total             = 16 + 5 + 162 = 183 B — exactly at the limit.
///
/// Old estimator: 183 + 10 (spurious DvbSubtitling auto-emit) = 193 → REJECTED.
/// New estimator: 183 (auto-emit suppressed via cache) → ACCEPTED.
#[test]
fn da_mux_2_recognized_subtitle_descriptor_suppresses_estimator_auto_emit() {
    // Caller-supplied recognized subtitle descriptor (tag 0x59, 10 bytes).
    let sub_desc_0x59 = crate::mpegts::descriptors::subtitling_descriptor(*b"eng", 0x10, 1, 1); // tag(1)+len(1)+lang(3)+type(1)+comp_id(2)+anc_id(2) = 10 bytes
    // Padding: 145-byte body → TLV is tag(1)+len(1)+body(145) = 147 bytes.
    let padding =
        crate::mpegts::descriptors::user_private(&[0u8; 145]).expect("145 B within descriptor cap");
    // caller_descs_len for subtitle = 10 + 147 = 157 bytes.
    // PMT: 16 (fixed) + 5 (video) + 5 + 157 (subtitle) = 183 B = MAX_PMT_SECTION_BYTES.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264); // PCR-eligible; no descriptor auto-emit
    prog.add_subtitle(
        0x200,
        SubtitleCodec::DvbSubtitling {
            language: *b"eng",
            subtitling_type: 0x10,
            composition_page_id: 1,
            ancillary_page_id: 1,
        },
    );
    prog.stream_descriptors_for_subtitle(0, vec![sub_desc_0x59, padding])
        .unwrap();
    let mut builder = MuxerConfig::builder();
    builder.add_program(prog.build());
    let config = builder
        .build()
        .expect("recognized subtitle descriptor suppresses auto-emit; config must fit budget");
    // Muxer::new must also succeed — the emitter validates the same budget.
    Muxer::new(config)
        .expect("Muxer::new must accept a near-budget config with suppressed subtitle auto-emit");
}

/// Guard against over-counting: an AC-3 config that genuinely fits the
/// single-packet PMT must still build and push without panicking.
#[test]
fn ac3_pmt_within_budget_builds_and_pushes() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    for i in 0..10u16 {
        prog.add_audio(0x0100 + i, AudioCodec::Ac3);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let config = b
        .build()
        .expect("10 AC-3 streams fit the single-packet PMT");
    let mut mux = Muxer::new(config).expect("Muxer::new");
    let handle = mux.audio_handles()[0];
    // The first push triggers PSI emission; must not panic.
    let _ = mux.push_audio_to(
        handle,
        crate::mpegts::common::Pts90khz::new(9000),
        &[0x0B, 0x77, 0, 0, 0, 0],
    );
}

/// Program 1 = video 0x101 + sync KLV 0x102 (no descriptors); program 2
/// (number 2, PMT 0x1100) = video 0x201 + sync KLV 0x202 carrying `descs_p2`.
fn two_sync_klv_programs(descs_p2: Vec<Vec<u8>>) -> MuxerConfigBuilder {
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x101, VideoCodec::H264);
    p1.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
    let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
    p2.add_video(0x201, VideoCodec::H264);
    p2.add_klv(0x202, KlvStreamType::SynchronousMetadata, true);
    if !descs_p2.is_empty() {
        p2.stream_descriptors_for_klv(0, descs_p2).unwrap();
    }
    let mut b = MuxerConfig::builder();
    b.add_program(p1.build());
    b.add_program(p2.build());
    b
}

#[test]
fn validate_rejects_auto_service_id_zero_on_two_programs() {
    match two_sync_klv_programs(Vec::new()).build() {
        Err(MuxError::ConfigInvalid { reason }) => {
            assert!(reason.contains("metadata_service_id 0"), "{reason}");
            assert!(
                reason.contains("0x0102") && reason.contains("0x0202"),
                "{reason}"
            );
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn validate_rejects_caller_service_id_colliding_with_auto_zero() {
    assert!(
        two_sync_klv_programs(vec![crate::mpegts::descriptors::metadata_klva(0)])
            .build()
            .is_err()
    );
}

#[test]
fn validate_accepts_distinct_caller_service_ids_across_programs() {
    let cfg = two_sync_klv_programs(vec![crate::mpegts::descriptors::metadata_klva(1)])
        .build()
        .unwrap();
    let muxer = Muxer::new(cfg).unwrap();
    // Cache = KLVA registration (6) + auto 0x27 (11) + the caller's 0x26 (11);
    // the caller's service byte is the 10th byte of that last TLV.
    assert_eq!(muxer.slots[1].pmt_descriptor_cache[1][26], 1);
}

#[test]
fn validate_rejects_same_service_id_on_two_sync_klv_pids_in_one_program() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(0x101, KlvStreamType::SynchronousMetadata, true);
    prog.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    assert!(matches!(b.build(), Err(MuxError::ConfigInvalid { .. })));
}

#[test]
fn validate_accepts_single_sync_klv_with_async_elsewhere() {
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x101, VideoCodec::H264);
    p1.add_klv(0x102, KlvStreamType::SynchronousMetadata, true);
    let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
    p2.add_video(0x201, VideoCodec::H264);
    p2.add_klv(0x202, KlvStreamType::PrivateData, false);
    let mut b = MuxerConfig::builder();
    b.add_program(p1.build());
    b.add_program(p2.build());
    let cfg = b.build().unwrap();
    Muxer::new(cfg).unwrap();
}

#[test]
fn metadata_descriptor_service_id_parses_short_and_long_forms() {
    use crate::mpegts::mux::state::metadata_descriptor_service_id as id;
    assert_eq!(id(&crate::mpegts::descriptors::metadata_klva(7)), Some(7));
    // Table 2-89 long form: application_format 0xFFFF + identifier, format 0xFF + "KLVA".
    assert_eq!(
        id(&[
            0x26, 0x0D, 0xFF, 0xFF, 0x01, 0x02, 0x03, 0x04, 0xFF, b'K', b'L', b'V', b'A', 0x09,
            0x0F
        ]),
        Some(9)
    );
    assert_eq!(id(&[0x26, 0x02, 0x01, 0x00]), None);
    assert_eq!(id(&crate::mpegts::descriptors::metadata_std(0, 0, 0)), None);
}
