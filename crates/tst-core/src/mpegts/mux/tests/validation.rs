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
    prog.stream_descriptors_for_subtitle(0, vec![sub_desc_0x59.clone(), padding])
        .unwrap();
    let mut builder = MuxerConfig::builder();
    builder.add_program(prog.build());
    let config = builder
        .build()
        .expect("recognized subtitle descriptor suppresses auto-emit; config must fit budget");
    // Muxer::new must also succeed — the emitter validates the same budget.
    let mut mux = Muxer::new(config)
        .expect("Muxer::new must accept a near-budget config with suppressed subtitle auto-emit");
    // The PMT the first push writes fills its section exactly and carries
    // the caller's subtitling descriptor once (no automatic twin).
    mux.push_video(
        &[0x00, 0x00, 0x00, 0x01, 0x65, 0x88],
        crate::mpegts::common::Pts90khz::new(9000),
        true,
    )
    .unwrap();
    let mut buf = vec![0u8; 188 * mux.pending_packets() as usize];
    let n = mux.pull(&mut buf);
    let pmt = buf[..n]
        .chunks_exact(188)
        .find(|p| u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]) == 0x1000)
        .expect("a PMT packet");
    let section_length = usize::from(pmt[6] & 0x0F) << 8 | usize::from(pmt[7]);
    assert_eq!(3 + section_length, 183, "the section uses the whole packet");
    let copies = pmt[5..]
        .windows(sub_desc_0x59.len())
        .filter(|w| *w == &sub_desc_0x59[..])
        .count();
    assert_eq!(copies, 1);
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
    mux.push_audio_to(
        handle,
        crate::mpegts::common::Pts90khz::new(9000),
        &[0x0B, 0x77, 0, 0, 0, 0],
    )
    .expect("the AC-3 push fits");
    assert!(mux.pending_packets() > 0);
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

/// The default configuration (PMT 0x1000) with `descs` as its
/// program_info loop.
fn with_program_descriptors(descs: Vec<Vec<u8>>) -> MuxerConfig {
    let mut cfg = MuxerConfig::default();
    cfg.programs[0].program_descriptors = descs;
    cfg
}

#[test]
fn rejects_malformed_program_descriptor() {
    let cases: [&[u8]; 5] = [
        &[0x05, 0x04],
        &[0x05, 0xFF],
        &[0x05],
        &[0x05, 0x02, b'K', b'L', b'V', b'A'],
        &[],
    ];
    for tlv in cases {
        match with_program_descriptors(vec![tlv.to_vec()]).validate() {
            Err(MuxError::ConfigInvalid { reason }) => assert!(
                reason.contains("program 1 program_descriptors[0]"),
                "{tlv:02X?}: {reason}"
            ),
            other => panic!("{tlv:02X?}: expected ConfigInvalid, got {other:?}"),
        }
    }
    // The index names the offending element, not the first one.
    match with_program_descriptors(vec![b"\x05\x04KLVA".to_vec(), vec![0x05]]).validate() {
        Err(MuxError::ConfigInvalid { reason }) => {
            assert!(reason.contains("program_descriptors[1]"), "{reason}")
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn a_program_descriptor_with_an_empty_body_is_well_formed() {
    let descs = vec![b"\x05\x04KLVA".to_vec(), vec![0xFF, 0x00]];
    assert!(with_program_descriptors(descs).validate().is_ok());
}

#[test]
fn valid_program_descriptor_serialises_unchanged() {
    let desc = b"\x05\x04KLVA".to_vec();
    let mut m = Muxer::new(with_program_descriptors(vec![desc.clone()])).unwrap();
    let h = m.video_stream_handle(0).unwrap();
    m.push_video_to(
        h,
        &[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00],
        crate::mpegts::common::Pts90khz::new(0),
        true,
    )
    .unwrap();
    let mut buf = [0u8; 188 * 16];
    let n = m.pull(&mut buf);
    let pmt = buf[..n]
        .chunks(188)
        .find(|p| ((u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])) == 0x1000)
        .expect("a PMT packet");
    // TS header (4) + pointer (1); program_info_length at [15..17].
    let program_info_length = (usize::from(pmt[15] & 0x0F) << 8) | usize::from(pmt[16]);
    assert_eq!(program_info_length, 6);
    assert_eq!(&pmt[17..23], &desc[..]);
}

// ─── Caller descriptors that replace automatic signalling ───────────────────

/// Video 0x100 + a KLV stream 0x101 of `stream_type` carrying `descs`.
fn klv_with(stream_type: KlvStreamType, descs: Vec<Vec<u8>>) -> Result<MuxerConfig, MuxError> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(
        0x101,
        stream_type,
        stream_type == KlvStreamType::SynchronousMetadata,
    );
    prog.stream_descriptors_for_klv(0, descs).unwrap();
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build()
}

/// Video 0x100 + a subtitle stream 0x200 of `codec` carrying `descs`.
fn subtitle_with(codec: SubtitleCodec, descs: Vec<Vec<u8>>) -> Result<MuxerConfig, MuxError> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_subtitle(0x200, codec);
    prog.stream_descriptors_for_subtitle(0, descs).unwrap();
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build()
}

/// Video 0x100 of AV1 carrying `descs`.
fn av1_with(descs: Vec<Vec<u8>>) -> Result<MuxerConfig, MuxError> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::Av1);
    prog.stream_descriptors_for_video(0, descs).unwrap();
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build()
}

#[track_caller]
fn assert_refused(result: Result<MuxerConfig, MuxError>, pid: &str) {
    match result {
        Err(MuxError::ConfigInvalid { reason }) => assert!(reason.contains(pid), "{reason}"),
        other => panic!("expected ConfigInvalid naming {pid}, got {other:?}"),
    }
}

const KLVA: &[u8] = b"\x05\x04KLVA";

fn dvb_subtitling() -> SubtitleCodec {
    SubtitleCodec::DvbSubtitling {
        language: *b"eng",
        subtitling_type: 0x10,
        composition_page_id: 1,
        ancillary_page_id: 1,
    }
}

fn dvb_teletext() -> SubtitleCodec {
    SubtitleCodec::DvbTeletext {
        language: *b"eng",
        teletext_type: 0x02,
        magazine_number: 1,
        page_number: 0x00,
    }
}

/// A well-formed subtitling_descriptor entry for `lang`.
fn subtitling_entry(lang: &[u8; 3]) -> Vec<u8> {
    vec![0x59, 0x08, lang[0], lang[1], lang[2], 0x10, 0, 1, 0, 1]
}

/// A well-formed teletext_descriptor entry for `lang` under `tag`.
fn teletext_entry(tag: u8, lang: &[u8; 3]) -> Vec<u8> {
    vec![tag, 0x05, lang[0], lang[1], lang[2], 0x10, 0x00]
}

/// H.222.0 lets a registration carry additional identification info behind
/// its format identifier, and the demuxer classifies such a stream as KLV.
#[test]
fn a_klva_registration_with_additional_identification_info_builds() {
    for st in [
        KlvStreamType::PrivateData,
        KlvStreamType::SynchronousMetadata,
    ] {
        let _ = klv_with(st, vec![b"\x05\x05KLVA\x00".to_vec()])
            .expect("KLVA followed by one byte of additional identification info");
        let _ = klv_with(st, vec![b"\x05\x08KLVA\x01\x02\x03\x04".to_vec()])
            .expect("KLVA followed by four bytes of additional identification info");
    }
}

/// The descriptor loop real synchronous-KLV sources carry: a KLVA
/// registration, a metadata_descriptor for service 0 and a
/// metadata_std_descriptor. It builds, and a push for service 0 goes out.
#[test]
fn a_klva_registration_with_metadata_descriptors_on_sync_klv_builds_and_pushes() {
    let descs = vec![
        KLVA.to_vec(),
        crate::mpegts::descriptors::metadata_klva(0),
        crate::mpegts::descriptors::metadata_std(0, 0, 0),
    ];
    let cfg = klv_with(KlvStreamType::SynchronousMetadata, descs).expect("the capture shape");
    let mut m = Muxer::new(cfg).unwrap();
    m.push_klv(
        &[0x06, 0x0E, 0x2B, 0x34],
        crate::mpegts::common::Pts90khz::new(90_000),
        0,
    )
    .expect("service 0 is declared");
    assert!(m.pending_packets() > 0);
}

#[test]
fn a_non_klva_registration_on_a_klv_stream_is_refused() {
    for st in [
        KlvStreamType::PrivateData,
        KlvStreamType::SynchronousMetadata,
    ] {
        assert_refused(klv_with(st, vec![b"\x05\x04ABCD".to_vec()]), "0x0101");
        assert_refused(klv_with(st, vec![b"\x05\x04KLVB".to_vec()]), "0x0101");
        assert_refused(klv_with(st, vec![b"\x05\x02KL".to_vec()]), "0x0101");
        // A second registration is refused in either order.
        assert_refused(
            klv_with(st, vec![KLVA.to_vec(), b"\x05\x04ABCD".to_vec()]),
            "0x0101",
        );
        assert_refused(
            klv_with(st, vec![b"\x05\x04ABCD".to_vec(), KLVA.to_vec()]),
            "0x0101",
        );
        let _ = klv_with(st, vec![KLVA.to_vec()]).expect("a KLVA registration");
        let _ = klv_with(
            st,
            vec![crate::mpegts::descriptors::user_private(b"label").unwrap()],
        )
        .expect("a user-private label");
    }
    // KLVA twice is not refused: the PMT then carries both, as given.
    let cfg = klv_with(KlvStreamType::PrivateData, vec![KLVA.to_vec(); 2]).unwrap();
    let muxer = Muxer::new(cfg).unwrap();
    assert_eq!(
        muxer.slots[0].pmt_descriptor_cache[1],
        [KLVA, KLVA].concat()
    );
}

#[test]
fn a_truncated_metadata_descriptor_on_sync_klv_is_refused() {
    let sync = KlvStreamType::SynchronousMetadata;
    // metadata_service_id is TLV byte 9 after a 2-byte application format
    // and the KLVA format identifier; byte 13 when the application format is
    // 0xFFFF plus its 4-byte identifier. Every cut before it is refused.
    let short = crate::mpegts::descriptors::metadata_klva(3);
    let long: &[u8] = &[
        0x26, 0x0D, 0xFF, 0xFF, 0x01, 0x02, 0x03, 0x04, 0xFF, b'K', b'L', b'V', b'A', 0x09, 0x0F,
    ];
    for (full, service_at) in [(&short[..], 9usize), (long, 13)] {
        for cut in 2..=service_at {
            let mut d = full[..cut].to_vec();
            d[1] = (cut - 2) as u8;
            assert_refused(klv_with(sync, vec![d]), "0x0101");
        }
        let mut whole = full[..=service_at].to_vec();
        whole[1] = (service_at - 1) as u8;
        let _ =
            klv_with(sync, vec![whole]).expect("a metadata_descriptor that reaches its service id");
    }
    let _ = klv_with(sync, vec![short.clone()]).expect("metadata_klva(3)");
    let _ = klv_with(
        sync,
        vec![
            crate::mpegts::descriptors::metadata_klva(3),
            crate::mpegts::descriptors::metadata_klva(4),
        ],
    )
    .expect("one metadata_descriptor per service");
    // Asynchronous KLV writes no metadata_descriptor of its own.
    let _ = klv_with(KlvStreamType::PrivateData, vec![vec![0x26, 0x00]]).expect("async KLV");
}

#[test]
fn a_second_or_malformed_metadata_std_descriptor_is_refused() {
    let sync = KlvStreamType::SynchronousMetadata;
    let std = crate::mpegts::descriptors::metadata_std(0, 0, 0);
    assert_eq!(std.len(), 11);
    assert_refused(klv_with(sync, vec![std.clone(), std.clone()]), "0x0101");
    let mut short = std[..10].to_vec();
    short[1] = 8;
    assert_refused(klv_with(sync, vec![short]), "0x0101");
    let mut long = std.clone();
    long.push(0);
    long[1] = 10;
    assert_refused(klv_with(sync, vec![long]), "0x0101");
    let _ = klv_with(sync, vec![std]).expect("exactly one well-formed metadata_std_descriptor");
}

#[test]
fn an_over_long_av01_registration_is_refused() {
    let av01 = crate::mpegts::descriptors::format_identifier_av01();
    assert_refused(av1_with(vec![b"\x05\x05AV01\x00".to_vec()]), "0x0100");
    assert_refused(
        av1_with(vec![
            crate::mpegts::descriptors::user_private(b"x").unwrap(),
            b"\x05\x06AV01\x00\x00".to_vec(),
        ]),
        "0x0100",
    );
    let label = crate::mpegts::descriptors::user_private(b"x").unwrap();
    let cfg = av1_with(vec![label.clone(), av01.clone()]).expect("the 6-byte AV01 anywhere");
    let muxer = Muxer::new(cfg).unwrap();
    assert_eq!(
        muxer.slots[0].pmt_descriptor_cache[0],
        [&av01[..], &label[..]].concat(),
        "the caller's AV01 still goes first"
    );
    let _ = av1_with(vec![b"\x05\x04ABCD".to_vec()]).expect("another registration on AV1");
}

#[test]
fn a_subtitle_marker_of_another_codec_is_refused() {
    let ga94 = b"\x05\x04GA94".to_vec();
    let vttc = b"\x05\x04VTTC".to_vec();
    assert_refused(
        subtitle_with(dvb_teletext(), vec![subtitling_entry(b"eng")]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_subtitling(), vec![teletext_entry(0x56, b"eng")]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_subtitling(), vec![teletext_entry(0x46, b"eng")]),
        "0x0200",
    );
    for codec in [dvb_subtitling(), dvb_teletext()] {
        assert_refused(
            subtitle_with(
                codec,
                vec![subtitling_entry(b"eng"), teletext_entry(0x56, b"eng")],
            ),
            "0x0200",
        );
    }
    assert_refused(
        subtitle_with(SubtitleCodec::WebVttInTs, vec![ga94.clone()]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(SubtitleCodec::Cea708Standalone, vec![vttc.clone()]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_subtitling(), vec![ga94.clone()]),
        "0x0200",
    );
    // The configured codec's own marker stays accepted.
    let _ = subtitle_with(SubtitleCodec::Cea708Standalone, vec![ga94]).expect("GA94 on CEA-708");
    let _ = subtitle_with(SubtitleCodec::WebVttInTs, vec![vttc]).expect("VTTC on WebVTT");
}

#[test]
fn a_malformed_subtitle_marker_is_refused() {
    assert_refused(
        subtitle_with(dvb_subtitling(), vec![vec![0x59, 0x01, 0x00]]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_subtitling(), vec![vec![0x59, 0x00]]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_teletext(), vec![vec![0x56, 0x01, 0x00]]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_teletext(), vec![vec![0x56, 0x00]]),
        "0x0200",
    );
    assert_refused(
        subtitle_with(dvb_teletext(), vec![vec![0x46, 0x00]]),
        "0x0200",
    );
    let mut nine = subtitling_entry(b"eng");
    nine.push(0);
    nine[1] = 9;
    assert_refused(subtitle_with(dvb_subtitling(), vec![nine]), "0x0200");
    // Whole entries for other languages are accepted.
    let mut two = subtitling_entry(b"eng");
    two.extend_from_slice(&subtitling_entry(b"deu")[2..]);
    two[1] = 16;
    let _ = subtitle_with(dvb_subtitling(), vec![two]).expect("two subtitling entries");
    let mut tt = teletext_entry(0x56, b"fra");
    tt.extend_from_slice(&teletext_entry(0x56, b"deu")[2..]);
    tt[1] = 10;
    let _ = subtitle_with(dvb_teletext(), vec![tt]).expect("two teletext entries");
    let _ =
        subtitle_with(dvb_teletext(), vec![teletext_entry(0x46, b"eng")]).expect("VBI teletext");
}

#[test]
fn reconfigure_with_a_refused_override_changes_nothing() {
    let cfg = klv_with(KlvStreamType::PrivateData, vec![]).unwrap();
    let mut m = Muxer::new(cfg.clone()).unwrap();
    let h = m.video_stream_handle(0).unwrap();
    let au = [0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    m.push_video_to(h, &au, crate::mpegts::common::Pts90khz::new(0), true)
        .unwrap();
    let mut buf = [0u8; 188 * 32];
    let n = m.pull(&mut buf);
    let pmt_before: Vec<u8> = buf[..n]
        .chunks(188)
        .find(|p| ((u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])) == 0x1000)
        .expect("a PMT")[4..]
        .to_vec();

    let mut bad = cfg;
    bad.programs[0].stream_descriptors[1] = vec![b"\x05\x04ABCD".to_vec()];
    assert!(matches!(
        m.reconfigure(bad),
        Err(MuxError::ConfigInvalid { .. })
    ));
    m.request_psi();
    m.push_video_to(h, &au, crate::mpegts::common::Pts90khz::new(3_600), true)
        .unwrap();
    let n = m.pull(&mut buf);
    let pmt_after = buf[..n]
        .chunks(188)
        .find(|p| ((u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])) == 0x1000)
        .expect("a PMT")[4..]
        .to_vec();
    // Same table at the same version: only the continuity counter moved.
    assert_eq!(pmt_after, pmt_before);
}

#[test]
fn a_too_large_pmt_with_well_formed_descriptors_still_reports_pmt_too_large() {
    let mut descs = vec![
        crate::mpegts::descriptors::metadata_klva(0),
        crate::mpegts::descriptors::metadata_std(0, 0, 0),
    ];
    descs.extend((0..8).map(|_| crate::mpegts::descriptors::user_private(&[0u8; 20]).unwrap()));
    assert!(matches!(
        klv_with(KlvStreamType::SynchronousMetadata, descs),
        Err(MuxError::PmtTooLarge { .. })
    ));
}
