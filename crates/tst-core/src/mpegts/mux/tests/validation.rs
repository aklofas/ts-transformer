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
                kind: StreamKind::Video,
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
    assert!(muxer.pmt_descriptor_caches[0][0].is_empty());

    // Stream 1 (KLV PrivateData) — KLVA Registration (6 bytes) +
    // user_private("KLV_LBL") (9 bytes) = 15 bytes.
    let entry = &muxer.pmt_descriptor_caches[0][1];
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
    assert_eq!(muxer.pmt_descriptor_caches[0][1].len(), 6);
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
    assert_eq!(muxer.pmt_descriptor_caches[0][1].len(), 28);
    assert_eq!(muxer.pmt_descriptor_caches[0][1][0], 0x05); // KLVA Registration
    assert_eq!(&muxer.pmt_descriptor_caches[0][1][2..6], b"KLVA");
    assert_eq!(muxer.pmt_descriptor_caches[0][1][6], 0x26);
    assert_eq!(muxer.pmt_descriptor_caches[0][1][17], 0x27);
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
    assert_eq!(muxer.pmt_descriptor_caches[0][1].len(), 28);
    assert_eq!(muxer.pmt_descriptor_caches[0][1][0], 0x05); // KLVA Registration
    assert_eq!(&muxer.pmt_descriptor_caches[0][1][2..6], b"KLVA");
    assert_eq!(muxer.pmt_descriptor_caches[0][1][6], 0x26); // metadata_descriptor
    assert_eq!(muxer.pmt_descriptor_caches[0][1][10], 0xFF); // metadata_format (defined by id)
    assert_eq!(&muxer.pmt_descriptor_caches[0][1][11..15], b"KLVA"); // metadata_format_identifier
    assert_eq!(muxer.pmt_descriptor_caches[0][1][17], 0x27); // metadata_std_descriptor
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
    assert_eq!(muxer.pmt_descriptor_caches[0][1].len(), 6); // KLVA only
    assert_eq!(muxer.pmt_descriptor_caches[0][1][0], 0x05);
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
