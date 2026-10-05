//! Subtitle push paths, handles, PMT descriptor auto-emit, and auto-emit
//! suppression when the caller supplies a recognized codec descriptor.

use super::*;
use crate::mpegts::common::Pts90khz;

// ── Configuration ─────────────────────────────────────────────────────────

#[test]
fn add_subtitle_records_the_stream_in_program_order() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    // The subtitle stream is the 2nd entry in this program's streams Vec
    // (after the video at index 0).
    assert!(matches!(
        &cfg.programs[0].streams[1],
        StreamSpec::Subtitle {
            pid: 0x200,
            codec: SubtitleCodec::WebVttInTs,
        }
    ));
}

#[test]
fn stream_descriptors_for_subtitle_attaches_at_build_time() {
    // stream_identifier_descriptor: tag 0x52, len 0x01, component_tag 0x42.
    let extra: Vec<Vec<u8>> = vec![vec![0x52u8, 0x01, 0x42]];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.stream_descriptors_for_subtitle(0, extra.clone())
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    // abs_idx 1 (after video at 0).
    assert_eq!(cfg.programs[0].stream_descriptors[1], extra);
}

// ── Push behavior ─────────────────────────────────────────────────────────

#[test]
fn push_subtitle_to_emits_pes_for_configured_handle() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let handles = mux.subtitle_handles();
    assert_eq!(handles.len(), 1);

    mux.push_subtitle_to(
        handles[0],
        Pts90khz::new(90_000),
        b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n",
    )
    .unwrap();

    let mut buf = vec![0u8; 188 * 64];
    let n = mux.pull(&mut buf);
    assert!(n > 0, "expected at least one TS packet");

    // At least one TS packet was emitted on PID 0x200.
    let saw_subtitle_pid = buf[..n]
        .chunks_exact(188)
        .any(|p| p[0] == 0x47 && (((p[1] as u16 & 0x1F) << 8) | (p[2] as u16)) == 0x200);
    assert!(
        saw_subtitle_pid,
        "expected a TS packet on subtitle PID 0x200"
    );
}

#[test]
fn push_subtitle_bare_rejects_when_multiple_subtitle_streams() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.add_subtitle(
            0x201,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let err = mux.push_subtitle(Pts90khz::new(90_000), b"x").unwrap_err();
    assert!(
        matches!(
            err,
            MuxError::AmbiguousTarget {
                kind: StreamKind::Subtitle,
                count: 2,
            }
        ),
        "expected AmbiguousTarget {{ subtitle, 2 }}, got {err:?}",
    );
}

#[test]
fn push_subtitle_payload_too_large_rejected() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let too_big = vec![0u8; 70_000];
    let err = mux
        .push_subtitle(Pts90khz::new(90_000), &too_big)
        .unwrap_err();
    assert!(
        matches!(err, MuxError::SubtitleTooLarge { .. }),
        "expected SubtitleTooLarge, got {err:?}",
    );
}

// ── DVB teletext PES_packet_length boundary ───────────────────────────────
//
// Spec: EN 300 472 §4.2 — `PES_packet_length = (N * 184) - 6`, where N is
// the number of TS payload areas the PES occupies. H.222.0 V9 §2.4.3.7
// caps `PES_packet_length` to a 16-bit field (max 65535). A pre-fix bug
// allowed payloads up to ~65490 bytes to pass the cap check; the writer
// then padded to `N * 184` bytes and the resulting `pes_packet_length`
// (e.g. 65682 for N=357) silently wrapped modulo 65536 to 146, causing
// wire-format truncation that conformant demuxers cannot recover from.
//
// Boundary math (per EN 300 472 §4.2 + H.222.0 §2.4.3.7):
// - `useful = HEADER(45) + auto_id_byte + payload.len()`
// - `n = ceil(useful / 184).max(1)`
// - `total_pes_bytes = n * 184`
// - `pes_packet_length = total_pes_bytes - 6` must fit in `u16` ⇒
//   `total_pes_bytes ≤ 65541`. Because `total_pes_bytes` is `n * 184`,
//   the largest valid total is `floor(65541 / 184) * 184 = 356 * 184 =
//   65504`. Therefore max `useful` is 65504, and max payload is:
//   - 65458 when `auto_prepend=true` (auto_id_byte=1)
//   - 65459 when `auto_prepend=false` (caller supplied 0x10..=0x1F)

fn teletext_muxer() -> Muxer {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    Muxer::new(cfg).unwrap()
}

/// Largest payload accepted when the writer auto-prepends `0x10`. Caller's
/// first byte is OUT of the `0x10..=0x1F` range → auto_id_byte=1 →
/// `useful = 45 + 1 + 65458 = 65504 = 356 * 184` → `pes_packet_length =
/// 65498` (fits in u16). One byte over this rejects.
#[test]
fn dvb_teletext_payload_65458_with_auto_prepend_accepted() {
    let mut mux = teletext_muxer();
    // First byte = 0x02 (not in 0x10..=0x1F) so the writer auto-prepends.
    let mut payload = vec![0u8; 65458];
    payload[0] = 0x02;
    mux.push_subtitle(Pts90khz::new(90_000), &payload)
        .expect("65458-byte teletext payload must be accepted with auto-prepend");
}

/// One byte beyond the auto-prepend cap rejects with `SubtitleTooLarge`.
/// `useful = 45 + 1 + 65459 = 65505 > 65504` ⇒ N=357 ⇒
/// `pes_packet_length = 65682 > u16::MAX`.
#[test]
fn dvb_teletext_payload_65459_with_auto_prepend_rejected() {
    let mut mux = teletext_muxer();
    let mut payload = vec![0u8; 65459];
    payload[0] = 0x02; // forces auto_prepend = true
    let err = mux
        .push_subtitle(Pts90khz::new(90_000), &payload)
        .unwrap_err();
    assert!(
        matches!(err, MuxError::SubtitleTooLarge { .. }),
        "expected SubtitleTooLarge for 65459-byte auto-prepend payload, got {err:?}",
    );
}

/// When the caller supplies a valid `data_identifier` (0x10..=0x1F) as the
/// first byte, no auto-prepend → 1 extra byte of headroom. `useful = 45 +
/// 0 + 65459 = 65504` exactly. Accepts.
#[test]
fn dvb_teletext_payload_65459_with_caller_supplied_id_accepted() {
    let mut mux = teletext_muxer();
    let mut payload = vec![0u8; 65459];
    payload[0] = 0x10; // EBU teletext data_identifier — suppresses auto-prepend
    mux.push_subtitle(Pts90khz::new(90_000), &payload).expect(
        "65459-byte teletext payload must be accepted when caller supplies data_identifier",
    );
}

/// One byte beyond the caller-supplied-id cap rejects. `useful = 45 + 0 +
/// 65460 = 65505 > 65504`.
#[test]
fn dvb_teletext_payload_65460_with_caller_supplied_id_rejected() {
    let mut mux = teletext_muxer();
    let mut payload = vec![0u8; 65460];
    payload[0] = 0x10;
    let err = mux
        .push_subtitle(Pts90khz::new(90_000), &payload)
        .unwrap_err();
    assert!(
        matches!(err, MuxError::SubtitleTooLarge { .. }),
        "expected SubtitleTooLarge for 65460-byte caller-id payload, got {err:?}",
    );
}

/// 65490 was the previous (incorrect) cap: `(u16::MAX as usize) - 45`. At
/// this size the writer would pad to N=357 TS payloads and emit
/// `pes_packet_length = 357*184 - 6 = 65682`, which truncates to 146
/// when cast to `u16`. The fix must reject this size.
#[test]
fn dvb_teletext_payload_65490_rejected_post_fix() {
    let mut mux = teletext_muxer();
    let mut payload = vec![0u8; 65490];
    payload[0] = 0x02;
    let err = mux
        .push_subtitle(Pts90khz::new(90_000), &payload)
        .unwrap_err();
    assert!(
        matches!(err, MuxError::SubtitleTooLarge { .. }),
        "expected SubtitleTooLarge for previously-accepted 65490-byte payload, got {err:?}",
    );
}

/// Wire-byte verification: an accepted near-boundary payload must produce
/// the spec-mandated `PES_packet_length = (N * 184) - 6` value, NOT a
/// silently-truncated `(N * 184 - 6) as u16` wrap. We probe the writer
/// directly so we can read the emitted bytes (TS framing in `pull` would
/// hide the PES header behind the 188-byte TS packet structure).
#[test]
fn dvb_teletext_near_boundary_pes_packet_length_matches_formula() {
    use super::pes::{SubtitlePesShape, write_subtitle_pes};

    // 65458-byte payload + auto-prepend → useful = 65504 = 356 * 184 →
    // pes_packet_length = 65498.
    let mut payload = vec![0u8; 65458];
    payload[0] = 0x02;
    let mut out = Vec::new();
    write_subtitle_pes(&mut out, 90_000, SubtitlePesShape::DvbTeletext, &payload);
    assert_eq!(
        out.len(),
        356 * 184,
        "total PES bytes must be N*184 per EN 300 472 §4.2",
    );
    let pes_packet_length = u16::from_be_bytes([out[4], out[5]]);
    assert_eq!(
        pes_packet_length, 65498,
        "PES_packet_length must be N*184 - 6 = 65498 per EN 300 472 §4.2; \
         a value of 146 would indicate u16 truncation of 65682",
    );
}

// ── Handle accessors ──────────────────────────────────────────────────────

#[test]
fn subtitle_handles_returns_one_per_configured_stream_across_programs() {
    let cfg = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x100);
        prog0.add_video(0x101, VideoCodec::H264);
        prog0.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x300);
        prog1.add_video(0x301, VideoCodec::H265);
        prog1.add_subtitle(
            0x400,
            SubtitleCodec::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        prog1.add_subtitle(
            0x401,
            SubtitleCodec::DvbTeletext {
                language: *b"spa",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build().unwrap()
    };
    let mux = Muxer::new(cfg).unwrap();
    assert_eq!(mux.subtitle_handles().len(), 3);

    let p1 = mux.subtitle_handles_for_program(1).unwrap();
    assert_eq!(p1.len(), 1);
    let p2 = mux.subtitle_handles_for_program(2).unwrap();
    assert_eq!(p2.len(), 2);
}

#[test]
fn subtitle_handles_for_unknown_program_returns_error() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mux = Muxer::new(cfg).unwrap();
    assert!(mux.subtitle_handles_for_program(99).is_err());
}

// ── MuxerConfig::validate for subtitle streams ────────────────────────────

#[test]
fn config_validate_too_many_subtitle_streams() {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, VideoCodec::H264);
    for i in 0..17 {
        prog.add_subtitle(0x200 + i, SubtitleCodec::WebVttInTs);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let err = b.build().unwrap_err();
    assert!(matches!(
        err,
        MuxError::TooManySubtitleStreams { count: 17, cap: 16 }
    ));
}

#[test]
fn config_validate_subtitle_pid_conflicts_with_video_pid() {
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x101, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    // Existing within-program PID uniqueness check.
    assert!(matches!(err, MuxError::InvalidConfig(_)));
}

#[test]
fn config_validate_rejects_subtitle_pid_as_pcr() {
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.pcr_pid(0x200); // pin PCR to the subtitle PID
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    assert!(matches!(
        err,
        MuxError::SubtitlePidUsedAsPcrPid { pid: 0x200 }
    ));
}

#[test]
fn validate_rejects_caller_pinned_pcr_on_klv_pid() {
    // Caller pins pcr_pid=0x101 explicitly to a KLV stream.
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x200, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.pcr_pid(0x101);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    assert!(
        matches!(err, MuxError::KlvPidUsedAsPcrPid { pid: 0x101 }),
        "expected KlvPidUsedAsPcrPid {{ pid: 0x101 }}, got {err:?}"
    );
}

#[test]
fn validate_rejects_klv_only_program_no_pcr_eligible_stream() {
    // No video, no audio — only KLV. KLV is not a PCR-eligible carrier
    // (cadence too sparse for ETSI TR 101 290 §5.6.1's 100 ms ceiling),
    // so the no-PCR-eligible-stream guard fires before PCR resolution.
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    assert!(
        matches!(err, MuxError::NoPcrEligibleStream { program_number: 1 }),
        "expected NoPcrEligibleStream, got {err:?}"
    );
}

#[test]
fn validate_accepts_pcr_pinned_to_video_with_klv_present() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x200, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.pcr_pid(0x200);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    let _ = cfg.expect("video-as-PCR is fine");
}

#[test]
fn config_validate_rejects_non_ascii_language_code() {
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbSubtitling {
                language: [0xFF, 0xFE, 0xFD],
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    assert!(matches!(err, MuxError::InvalidLanguageCode { .. }));
}

#[test]
fn config_validate_rejects_magazine_out_of_range() {
    let err = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 8, // out of range (3-bit; max 7)
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap_err()
    };
    assert!(matches!(err, MuxError::InvalidTeletextField { .. }));
}

#[test]
fn validate_rejects_subtitle_only_program() {
    // Subtitles must not carry PCR per ETSI EN 300 472 §4.0 +
    // EN 300 743 §6.1. The PCR fallback chain (caller-pinned > video >
    // audio) excludes subtitles, so a subtitle-only program has
    // no resolvable PCR PID.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    match cfg {
        Err(MuxError::NoPcrEligibleStream { program_number }) => {
            assert_eq!(program_number, 1);
        }
        other => panic!("expected NoPcrEligibleStream, got {other:?}"),
    }
}

#[test]
fn validate_accepts_video_plus_subtitle_program() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()
    };
    assert!(cfg.is_ok(), "video + subtitle program must validate");
}

// ── PMT descriptor auto-emit for subtitle codecs ─────────────────────────

#[test]
fn pmt_emits_subtitle_entry_with_subtitling_descriptor() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 0x0001,
                ancillary_page_id: 0x0001,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // Stream 0 (video) — no auto-emit, no caller — empty cache entry.
    assert!(muxer.pmt_descriptor_caches[0][0].is_empty());

    // Stream 1 (subtitle) — subtitling_descriptor: tag 0x59, len 0x08,
    // 3 bytes language + 1 type + 2 composition_page_id + 2 ancillary_page_id
    // = 10 bytes total.
    let entry = &muxer.pmt_descriptor_caches[0][1];
    assert_eq!(entry.len(), 10);
    assert_eq!(entry[0], 0x59); // subtitling_descriptor tag
    assert_eq!(entry[1], 0x08); // length
    assert_eq!(&entry[2..5], b"eng");
    assert_eq!(entry[5], 0x10);
    assert_eq!(&entry[6..8], &[0x00, 0x01]);
    assert_eq!(&entry[8..10], &[0x00, 0x01]);
}

#[test]
fn pmt_emits_subtitle_entry_with_vttc_registration_for_webvtt() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // WebVttInTs auto-emit: registration_descriptor tag 0x05, len 0x04,
    // format_identifier == "VTTC" — 6 bytes total.
    let entry = &muxer.pmt_descriptor_caches[0][1];
    assert_eq!(entry.len(), 6);
    assert_eq!(entry[0], 0x05); // registration_descriptor tag
    assert_eq!(entry[1], 0x04); // length
    assert_eq!(&entry[2..6], b"VTTC");
}

#[test]
fn pmt_emits_subtitle_entry_with_ga94_registration_for_cea708_standalone() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::Cea708Standalone);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // Cea708Standalone auto-emit: registration_descriptor "GA94".
    let entry = &muxer.pmt_descriptor_caches[0][1];
    assert_eq!(entry.len(), 6);
    assert_eq!(entry[0], 0x05);
    assert_eq!(entry[1], 0x04);
    assert_eq!(&entry[2..6], b"GA94");
}

#[test]
fn pmt_emits_subtitle_entry_with_teletext_descriptor() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 1,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // teletext_descriptor: tag 0x56, len 0x05 — 7 bytes total.
    let entry = &muxer.pmt_descriptor_caches[0][1];
    assert_eq!(entry.len(), 7);
    assert_eq!(entry[0], 0x56);
    assert_eq!(entry[1], 0x05);
    assert_eq!(&entry[2..5], b"eng");
    // teletext_type (5 bits) << 3 | magazine_number (3 bits) = 0x02<<3 | 1 = 0x11
    assert_eq!(entry[5], (0x02 << 3) | 0x01);
    assert_eq!(entry[6], 0x88);
}

#[test]
fn pmt_appends_caller_supplied_descriptors_after_auto_emit() {
    // Caller-supplied stream_identifier_descriptor (tag 0x52, len 0x01,
    // component_tag 0x42 — 3 bytes) appends AFTER the VTTC auto-emit.
    // The stream_identifier_descriptor is not a recognized subtitle codec
    // marker, so it does not suppress the auto-emit; the auto-emit fires
    // and the caller's bytes append afterwards. (Caller-supplied codec
    // markers — subtitling/teletext/VBI-teletext/VTTC/GA94 — do suppress
    // the auto-emit; see the `subtitle_auto_emit_suppressed_*` tests.)
    let extra: Vec<Vec<u8>> = vec![vec![0x52u8, 0x01, 0x42]];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.stream_descriptors_for_subtitle(0, extra).unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // VTTC auto-emit (6 bytes) + stream_identifier (3 bytes) = 9 bytes.
    let entry = &muxer.pmt_descriptor_caches[0][1];
    assert_eq!(entry.len(), 9);
    // Auto-emit first.
    assert_eq!(&entry[..6], &[0x05, 0x04, b'V', b'T', b'T', b'C']);
    // Caller's stream_identifier_descriptor after.
    assert_eq!(&entry[6..9], &[0x52, 0x01, 0x42]);
}

// ── AV1 PMT descriptor auto-emit ─────────────────────────────────────────

#[test]
fn pmt_emits_av1_with_av01_registration_first() {
    // VideoCodec::Av1 must auto-emit the AV01 registration_descriptor as
    // the FIRST descriptor in the per-stream PMT loop (binding §2.1).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let entry = &muxer.pmt_descriptor_caches[0][0];
    // AV01 Registration: 0x05 0x04 'A' 'V' '0' '1' = 6 bytes.
    assert!(
        entry.len() >= 6,
        "expected AV01 auto-emit (≥6 bytes), got {}",
        entry.len()
    );
    assert_eq!(&entry[..6], &[0x05, 0x04, b'A', b'V', b'0', b'1']);
}

#[test]
fn pmt_av1_with_caller_supplied_av01_suppresses_auto_emit() {
    // When caller has already supplied an AV01 Registration, suppress the
    // auto-emit — mirrors KLVA suppression. Result is exactly the caller's
    // bytes.
    let custom_av01 = vec![0x05, 0x04, b'A', b'V', b'0', b'1'];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::Av1);
        prog.stream_descriptors_for_video(0, vec![custom_av01.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let entry = &muxer.pmt_descriptor_caches[0][0];
    assert_eq!(
        entry.len(),
        6,
        "auto-emit should suppress when caller provides AV01"
    );
    assert_eq!(&entry[..], &custom_av01[..]);
}

// ── Subtitle auto-emit suppression on caller-supplied descriptors ─────────

#[test]
fn subtitle_auto_emit_suppressed_on_caller_supplied_subtitling() {
    // Caller supplies a 2-entry subtitling_descriptor; the muxer must
    // NOT also auto-emit the single-entry one for this PID — caller's
    // takes precedence. Mirrors the KLV/AV1 caller-supplied-Registration
    // suppression rule.
    let caller_desc = crate::mpegts::descriptors::subtitling_descriptor_multi(&[
        (*b"eng", 0x10, 1, 1),
        (*b"spa", 0x10, 2, 2),
    ])
    .expect("non-empty entries");
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        prog.stream_descriptors_for_subtitle(0, vec![caller_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();

    // Inspect the per-stream descriptor cache for the subtitle stream's
    // PMT entry. Stream index 1 = subtitle (after video at index 0).
    // There must be exactly one 0x59 descriptor — the caller's
    // multi-entry one — and no auto-emitted single-entry one.
    let cache = &muxer.pmt_descriptor_caches[0][1];
    let mut count_0x59 = 0;
    let mut idx = 0;
    while idx + 1 < cache.len() {
        let tag = cache[idx];
        let len = cache[idx + 1] as usize;
        if tag == 0x59 {
            count_0x59 += 1;
            assert_eq!(&cache[idx..idx + 2 + len], &caller_desc[..]);
        }
        idx += 2 + len;
    }
    assert_eq!(
        count_0x59, 1,
        "auto-emit must suppress when caller supplies subtitling_descriptor"
    );
}

#[test]
fn subtitle_auto_emit_suppressed_on_caller_supplied_teletext() {
    // Caller supplies a teletext_descriptor (tag 0x56); auto-emit must
    // suppress.
    let caller_desc = crate::mpegts::descriptors::teletext_descriptor(*b"eng", 0x02, 1, 0x88);
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"fra",
                teletext_type: 0x02,
                magazine_number: 2,
                page_number: 0x77,
            },
        );
        prog.stream_descriptors_for_subtitle(0, vec![caller_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let cache = &muxer.pmt_descriptor_caches[0][1];
    // Exactly the caller's bytes — no auto-emit prepended.
    assert_eq!(cache, &caller_desc);
}

#[test]
fn subtitle_auto_emit_suppressed_on_caller_supplied_vttc_registration() {
    // Caller supplies a VTTC registration_descriptor; suppress auto-emit.
    let caller_desc = vec![0x05u8, 0x04, b'V', b'T', b'T', b'C'];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
        prog.stream_descriptors_for_subtitle(0, vec![caller_desc.clone()])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let cache = &muxer.pmt_descriptor_caches[0][1];
    // Exactly the caller's bytes — no double VTTC.
    assert_eq!(cache, &caller_desc);
}

#[test]
fn subtitle_auto_emit_fires_when_caller_supplies_unrelated_descriptors() {
    // Caller supplies stream_identifier_descriptor (tag 0x52) — not a
    // subtitle codec marker — so auto-emit must still fire.
    let unrelated = vec![0x52u8, 0x01, 0x42];
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        prog.stream_descriptors_for_subtitle(0, vec![unrelated])
            .unwrap();
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let muxer = Muxer::new(cfg).unwrap();
    let cache = &muxer.pmt_descriptor_caches[0][1];
    // Walk descriptors; expect exactly one 0x59 (the auto-emit).
    let mut count_0x59 = 0;
    let mut idx = 0;
    while idx + 1 < cache.len() {
        let tag = cache[idx];
        let len = cache[idx + 1] as usize;
        if tag == 0x59 {
            count_0x59 += 1;
        }
        idx += 2 + len;
    }
    assert_eq!(
        count_0x59, 1,
        "auto-emit must fire when caller-supplied descriptors don't include a subtitle codec marker"
    );
}
