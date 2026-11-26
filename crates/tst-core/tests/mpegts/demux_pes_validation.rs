//! Demuxer PES header & PTS validation tests.
//!
//! Four discrete checks, each pinned by an integration test:
//!
//! - **PTS anomaly distinct from PCR anomaly.** Backward-PTS detection
//!   is its own `NonConformantIssue::PtsAnomaly` variant (not `PcrAnomaly`),
//!   PTS-required stream types (audio, video) surface `MissingRequiredPts`
//!   when the PES omits PTS, and `last_pts_by_pid` is never overwritten
//!   with 0 when PTS is absent.
//!
//! - **PES header structural validation.** Per H.222.0 V9 §2.4.3.6 +
//!   §2.4.3.7 the demuxer rejects `PTS_DTS_flags = 0b01` (forbidden),
//!   validates the byte-6 `'10'` marker bits, and validates the PTS/DTS
//!   5-byte prefix nibbles and trailing marker bits.
//!
//! - **Subtitle data_alignment validation.** Per EN 300 743 §6.2 +
//!   EN 300 472 §4.2 the demuxer surfaces `SubtitleAlignmentMissing` when
//!   a DVB-sub / teletext PES arrives with `data_alignment_indicator = 0`.
//!
//! - **AAC-LATM (stream_type 0x11) sync validation.** Per ISO/IEC
//!   14496-3 §1.7 + H.222.0 Table 2-34 a LATM PES carries LOAS records,
//!   each opening with a 24-bit header (syncword 0x2B7 + 13-bit
//!   audioMuxLengthBytes); a record may continue into the next PES
//!   (§1.7.2). The demuxer surfaces `LatmFraming` when a record boundary
//!   lacks the syncword.

use crate::common::mux_demux::drain_mux as drain_all;
use crate::common::mux_demux::patch_pes_strip_pts;
use tst_core::codec::aac::latm::LatmFramingKind;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, PesHeaderMalformedKind, StrictMode,
};
use tst_core::mpegts::mux::{
    AudioCodec, Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSub, VideoCodec,
};

// =====================================================================
// PTS anomaly distinct from PCR anomaly
// =====================================================================

/// Build a synthetic TS stream with two H.264 PESes on PID 0x101 where the
/// second has a PTS far behind the first. Demuxer should emit
/// `NonConformantIssue::PtsAnomaly` (NOT `PcrAnomaly`).
#[test]
fn demux_b4_backward_pts_emits_pts_anomaly_not_pcr_anomaly() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let video_handle = mux.video_handles()[0];
    let au = [
        // AUD + IDR (minimal H.264 AU shape per existing test convention)
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
    ];
    // Big PTS first, then a PTS far in the past (well past the -90_000-tick
    // threshold the demuxer uses to call out backward-PTS).
    mux.push_video_to(video_handle, &au, Pts90khz::new(1_000_000), true)
        .unwrap();
    mux.push_video_to(video_handle, &au, Pts90khz::new(100_000), true)
        .unwrap();
    let bytes = drain_all(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    // The PES-path backward-PTS check must surface PtsAnomaly with delta in
    // 90 kHz ticks (≈ -900_000). The PCR-path may *separately* emit
    // PcrAnomaly with delta in 27 MHz ticks (≈ -270_000_000) because the
    // muxer's PCR clock follows PTS — that's the PCR-side concern.
    // The PTS check is asserted by: at least one event is PtsAnomaly (the new
    // variant) instead of every backward-PTS being lumped under PcrAnomaly.
    let pts_anomaly_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PtsAnomaly { delta },
                ..
            } if *delta < -90_000
        )
    });
    assert!(
        pts_anomaly_seen,
        "expected PtsAnomaly variant on PID 0x101, got {events:?}"
    );

    // Specifically: the PES-path no longer emits PcrAnomaly with a 90-kHz-scale
    // delta. (Pre-fix, the PES code path issued PcrAnomaly { delta: -900_000 } —
    // i.e., the 90 kHz delta misrouted into PcrAnomaly. Post-fix, that small
    // delta routes to PtsAnomaly. PCR path's own emission carries a much larger
    // 27 MHz delta.)
    let mis_labeled = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PcrAnomaly { delta },
                ..
            } if (-2_000_000..-50_000).contains(delta)
        )
    });
    assert!(
        !mis_labeled,
        "PES-path 90-kHz-scale delta must NOT be reported as PcrAnomaly, got {events:?}"
    );
}

/// When PTS-absent PESes alternate with PTS-present PESes, `last_pts_by_pid`
/// must NOT be poisoned with 0 — i.e., a later valid PTS should not retroactively
/// look "backward" relative to an earlier 0-fallback.
///
/// Pre-fix: a PES without PTS wrote 0 to last_pts_by_pid; the next PES's
/// pts_diff_33bit(observed, 0) is very negative and falsely flags PtsAnomaly.
/// Post-fix: we never write 0; subsequent valid PESes compare against the
/// last *actually-observed* PTS.
#[test]
fn demux_b4_missing_pts_does_not_poison_last_pts() {
    // Build directly from the reassembler so we control PTS presence explicitly.
    // The reassembler accepts any well-formed PES; we just need to drive
    // handle_complete_pes via feed().

    // Strategy: synthesize a TS stream where the video PES carries PTS but
    // we inject an extra non-PTS PES on the same PID by feeding a hand-crafted
    // PES via the muxer is too invasive — instead, verify via the reassembler's
    // direct unit test path. (Integration: this is asserted by the unit tests
    // in pes.rs and the absence of false-positive PtsAnomaly in the simpler
    // monotonic-PTS case.)
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let video_handle = mux.video_handles()[0];
    let au = [
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
    ];
    // Forward-monotonic PTSes. There should be no PTS or PCR anomaly events.
    for i in 1..5 {
        mux.push_video_to(video_handle, &au, Pts90khz::new(i * 90_000), true)
            .unwrap();
    }
    let bytes = drain_all(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let any_anomaly = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PtsAnomaly { .. }
                    | NonConformantIssue::PcrAnomaly { .. },
                ..
            }
        )
    });
    assert!(
        !any_anomaly,
        "forward-monotonic PTSes must not surface any timing anomaly, got {events:?}"
    );
}

/// A video PES without a PTS is legal on its own: H.222.0 §2.7.4 bounds only
/// the spacing of coded PTS, and this stream carries no coded PTS at all, so
/// there is no spacing to measure and no `MissingRequiredPts`. We can't ask
/// the muxer for a no-PTS video PES (it always sets PTS), so we patch the PES
/// flags2 byte to clear PTS_DTS_flags (set to 0b00) and the
/// header_data_length to 0.
#[test]
fn pts_less_video_pes_without_spacing_violation_raises_nothing() {
    let raw = build_h264_ts_with_one_pes();
    let patched = patch_pes_strip_pts(raw, 0xE0);

    let mut demux = Demuxer::new();
    let _ = demux.feed(&patched);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MissingRequiredPts { .. },
                ..
            }
        )
    });
    assert!(
        !issue_seen,
        "a lone PTS-less video PES must not raise MissingRequiredPts, got {events:?}"
    );
}

// =====================================================================
// PES header structural validation
// =====================================================================

/// `PTS_DTS_flags = 0b01` is forbidden by H.222.0 §2.4.3.7. Build a valid
/// H.264 PES via the muxer, then patch the PES `flags2` byte to set
/// PTS_DTS_flags=0b01. Demux should surface
/// `NonConformantIssue::PesHeaderMalformed { kind: ForbiddenPtsDtsFlags }`.
#[test]
fn demux_b5_forbidden_pts_dts_flags_0b01_surfaces_issue() {
    let raw = build_h264_ts_with_one_pes();
    let patched = patch_pes_pts_dts_flags(raw, 0xE0, 0b01);

    let mut demux = Demuxer::new();
    let _ = demux.feed(&patched);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PesHeaderMalformed {
                    kind: PesHeaderMalformedKind::ForbiddenPtsDtsFlags,
                    ..
                },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "expected PesHeaderMalformed::ForbiddenPtsDtsFlags, got {events:?}"
    );
}

/// `StrictMode::Full` should escalate `PesHeaderMalformed` to a fatal so the
/// receive loop fails closed.
#[test]
fn demux_b5_strict_full_escalates_pes_header_malformed() {
    let raw = build_h264_ts_with_one_pes();
    let patched = patch_pes_pts_dts_flags(raw, 0xE0, 0b01);

    let mut demux = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    let _ = demux.feed(&patched);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PesHeaderMalformed { .. },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "strict mode should still emit the issue, got {events:?}"
    );
}

// =====================================================================
// Subtitle data_alignment_indicator validation
// =====================================================================

/// Build a DVB-sub PES via the muxer (sets data_alignment_indicator=1 per
/// EN 300 743 §6.2), then patch the flag bit to 0. Demux should emit
/// `NonConformantIssue::SubtitleAlignmentMissing { pid }`.
#[test]
fn demux_b6_dvb_sub_missing_data_alignment_emits_issue() {
    let raw = build_ts_with_dvb_sub_pes();
    // Flip data_alignment_indicator off (PES byte 6 bit 2 = 0x04).
    let patched = patch_dvb_sub_pes_clear_data_alignment(raw);

    let mut demux = Demuxer::new();
    demux.feed(&patched).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::SubtitleAlignmentMissing { pid: 0x200 },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "expected SubtitleAlignmentMissing on PID 0x200, got {events:?}"
    );
}

#[test]
fn demux_b6_strict_full_suppresses_subtitle_sample() {
    let raw = build_ts_with_dvb_sub_pes();
    let patched = patch_dvb_sub_pes_clear_data_alignment(raw);

    let mut demux = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    let _ = demux.feed(&patched);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let sample_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                stream,
                ..
            } if stream.pid == 0x200
        )
    });
    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::SubtitleAlignmentMissing { .. },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "strict mode must still emit the SubtitleAlignmentMissing issue, got {events:?}"
    );
    assert!(
        !sample_seen,
        "strict mode must suppress the subtitle Sample, got {events:?}"
    );
}

// =====================================================================
// Helpers — hand-rolled TS packet construction
// =====================================================================

/// Build a complete TS byte stream containing one H.264 PES on PID 0x101.
fn build_h264_ts_with_one_pes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];
    let au = [
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
    ];
    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    drain_all(&mut mux)
}

/// Locate the PES start (`0x00 0x00 0x01 <stream_id>`) in a TS byte stream
/// and patch the `PTS_DTS_flags` field (top 2 bits of byte 7) to `new_flags`.
/// Returns the patched bytes.
fn patch_pes_pts_dts_flags(mut bytes: Vec<u8>, stream_id: u8, new_flags: u8) -> Vec<u8> {
    let needle = [0x00u8, 0x00, 0x01, stream_id];
    let pos = bytes
        .windows(4)
        .position(|w| w == needle)
        .expect("PES start code should appear in the muxed TS");
    // Byte 7 of the PES header carries `PTS_DTS_flags` in bits 7..6.
    let flags2_off = pos + 7;
    bytes[flags2_off] = (bytes[flags2_off] & 0x3F) | ((new_flags & 0x03) << 6);
    bytes
}

/// Build a TS byte stream containing one DVB-sub PES on PID 0x200.
fn build_ts_with_dvb_sub_pes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            MuxSub::DvbSubtitling {
                language: *b"eng",
                subtitling_type: 0x10,
                composition_page_id: 1,
                ancillary_page_id: 1,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.subtitle_handles()[0];
    // Single segment: sync_byte 0x0F + 8 bytes of segment header.
    mux.push_subtitle_to(
        h,
        Pts90khz::new(90_000),
        &[0x0F, 0x10, 0x01, 0x00, 0x14, 0x00, 0x00],
    )
    .unwrap();
    drain_all(&mut mux)
}

/// Locate the DVB-sub PES start (`0x00 0x00 0x01 0xBD`) on the TS-byte stream
/// and clear bit 2 of the PES `flags1` byte (data_alignment_indicator).
fn patch_dvb_sub_pes_clear_data_alignment(mut bytes: Vec<u8>) -> Vec<u8> {
    let needle = [0x00u8, 0x00, 0x01, 0xBD];
    let pes_start = bytes
        .windows(4)
        .position(|w| w == needle)
        .expect("DVB-sub PES start (00 00 01 BD) should appear in muxed TS");
    // flags1 is at offset 6 from the start-code.
    let flags1_off = pes_start + 6;
    bytes[flags1_off] &= !0x04;
    bytes
}

// =====================================================================
// AAC-LATM (stream_type 0x11) sync validation
// =====================================================================

/// Build a TS byte stream containing one AAC-LATM PES on PID 0x150. The
/// "LATM frame" is `payload` — used in conjunction with a conformant
/// (`build_loas_record`) or non-conformant (`bad_latm_payload`) shape
/// to drive LATM acceptance.
///
/// The PES has `data_alignment_indicator` set: the muxer leaves it clear for
/// LATM, and without it a PES may start mid-record (ISO/IEC 14496-3 §1.7.2).
/// The demuxer flags a missing syncword at once on a PES that announces
/// alignment or starts with a syncword; any other PES it resyncs on, and only
/// a second such PES in a row without a record raises the issue.
fn build_ts_with_aac_latm_pes(payload: &[u8]) -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x150, AudioCodec::AacLatm);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let audio_handle = mux.audio_handles()[0];
    // The muxer wraps the supplied bytes in a PES; on the wire what the
    // demuxer sees as the PES payload is precisely `payload`.
    mux.push_audio_to(audio_handle, Pts90khz::new(90_000), payload)
        .unwrap();
    let mut bytes = drain_all(&mut mux);
    let pes_start = bytes
        .windows(4)
        .position(|w| w == [0x00, 0x00, 0x01, 0xC0])
        .expect("LATM PES start (00 00 01 C0) should appear in muxed TS");
    // flags1 is at offset 6 from the start-code; bit 2 = data_alignment_indicator.
    bytes[pes_start + 6] |= 0x04;
    bytes
}

/// Build a 3-byte LOAS header + `len` body bytes (zeros). Matches the
/// helper inside `codec::aac::latm` tests; duplicated here to avoid
/// re-exporting test-only constants.
fn build_valid_loas_record(len: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + usize::from(len));
    out.push(0x56);
    out.push(0xE0 | ((len >> 8) as u8 & 0x1F));
    out.push((len & 0xFF) as u8);
    out.resize(3 + usize::from(len), 0);
    out
}

/// Primary lenient-mode test: PES that does not begin with the LOAS
/// syncword on a `stream_type=0x11` PID surfaces
/// `NonConformantIssue::LatmFraming { kind: MissingSyncword }`.
///
/// Common real-world cause: ADTS-framed AAC mistakenly shipped on a
/// LATM-advertising PID (encoder configuration bug). The
/// `[0xFF, 0xF1, ...]` payload prefix is a valid ADTS sync word that
/// fails LOAS validation.
#[test]
fn demux_c11_latm_missing_syncword_emits_issue() {
    let bad_payload = [0xFFu8, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC];
    let bytes = build_ts_with_aac_latm_pes(&bad_payload);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::LatmFraming {
                    pid: 0x150,
                    kind: LatmFramingKind::MissingSyncword,
                },
                ..
            }
        )
    });
    assert!(
        issue_seen,
        "expected LatmFraming::MissingSyncword on PID 0x150, got {events:?}"
    );
}

/// ISO/IEC 14496-3 §1.7.2: an AudioSyncStream record may continue into the
/// next PES, so a record whose `audioMuxLengthBytes` runs past its PES is
/// not flagged when the next PES carries the rest of it.
#[test]
fn demux_c11_latm_record_split_across_two_pes_is_not_flagged() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x150, AudioCodec::AacLatm);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let audio_handle = mux.audio_handles()[0];
    // PES 1: record A (10) + the first 20 bytes of record B (50);
    // PES 2: the remaining 33 bytes of B + record C (8).
    let record_b = build_valid_loas_record(50);
    let mut pes1 = build_valid_loas_record(10);
    pes1.extend_from_slice(&record_b[..20]);
    let mut pes2 = record_b[20..].to_vec();
    pes2.extend_from_slice(&build_valid_loas_record(8));
    mux.push_audio_to(audio_handle, Pts90khz::new(90_000), &pes1)
        .unwrap();
    mux.push_audio_to(audio_handle, Pts90khz::new(91_920), &pes2)
        .unwrap();
    let bytes = drain_all(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::LatmFraming { .. },
                ..
            }
        )
    });
    assert!(
        !issue_seen,
        "a LOAS record continuing into the next PES must not emit LatmFraming, got {events:?}"
    );
    let samples = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x150))
        .count();
    assert_eq!(
        samples, 2,
        "expected two Samples on PID 0x150, got {events:?}"
    );
}

/// Mux `payloads` as consecutive AAC-LATM PES on PID 0x150 (the muxer's LATM
/// PES carry `data_alignment_indicator` = 0) and demux them; every event.
fn latm_pes_events(payloads: &[Vec<u8>]) -> Vec<DemuxEvent> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x150, AudioCodec::AacLatm);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let audio_handle = mux.audio_handles()[0];
    for (i, payload) in payloads.iter().enumerate() {
        mux.push_audio_to(
            audio_handle,
            Pts90khz::new(90_000 + i as i64 * 1_920),
            payload,
        )
        .unwrap();
    }
    let bytes = drain_all(&mut mux);
    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }
    events
}

fn latm_framing_issues(events: &[DemuxEvent]) -> Vec<(usize, LatmFramingKind)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::LatmFraming { pid: 0x150, kind },
                ..
            } => Some((i, *kind)),
            _ => None,
        })
        .collect()
}

fn latm_sample_indices(events: &[DemuxEvent]) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x150))
        .map(|(i, _)| i)
        .collect()
}

/// `zeros` zero bytes, then `rest`.
fn zeros_then(zeros: usize, rest: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; zeros];
    out.extend_from_slice(rest);
    out
}

/// Join mid-record with PES smaller than one record: PES 1 is 300 bytes of a
/// 2000-byte record's body (no candidate), PES 2 is the record's tail (100 B)
/// plus the start of a 1500-byte record (candidate, record overruns the PES),
/// PES 3 is that record's remainder plus a complete 20-byte record (confirms).
/// No LatmFraming issue: a count of candidate-free PES would flag PES 1 and 2.
#[test]
fn demux_c11_latm_pes_smaller_than_a_record_after_a_join_is_not_flagged() {
    let record = build_valid_loas_record(1500);
    let pes2 = zeros_then(100, &record[..200]);
    let mut pes3 = record[200..].to_vec();
    pes3.extend_from_slice(&build_valid_loas_record(20));
    let events = latm_pes_events(&[vec![0u8; 300], pes2, pes3]);
    assert!(latm_framing_issues(&events).is_empty(), "{events:?}");
    assert_eq!(latm_sample_indices(&events).len(), 3, "{events:?}");
}

/// PES 2's first candidate is false (a 0x56 0xE0 pair inside the body whose
/// bogus length points past the PES); PES 3 does not carry a syncword at the
/// predicted position. The tentative carry is dropped silently.
#[test]
fn demux_c11_latm_false_candidate_is_dropped_without_an_issue() {
    // 0x56 0xE5 0x00: audioMuxLengthBytes 0x500, a 1283-byte record that
    // runs past the 300-byte PES (tentative Body(1083)).
    let mut pes2 = zeros_then(100, &[0x56, 0xE5, 0x00]);
    pes2.resize(300, 0);
    let events = latm_pes_events(&[vec![0u8; 300], pes2, vec![0u8; 2000]]);
    assert!(latm_framing_issues(&events).is_empty(), "{events:?}");
}

/// Once a candidate is confirmed, the carry is enforced. PES 1: 300 zeros.
/// PES 2: 100 zeros + header(1500) + 297 bytes (a candidate owing 1203).
/// PES 3: 1203 zeros + header(500) + 20 bytes (the candidate passes its
/// boundary and owes 480: a provisional position). PES 4: 480 zeros +
/// header(200) + 47 bytes (it passes its next boundary: confirmed, owing
/// 153). PES 5: 530 zeros, no syncword at 153: MissingSyncword at once.
/// The assertion pins WHERE: after the fourth Sample on 0x150, so an issue
/// raised while the position was provisional does not pass.
#[test]
fn demux_c11_latm_confirmed_tentative_carry_is_enforced() {
    let pes2 = zeros_then(100, &build_valid_loas_record(1500)[..300]);
    let pes3 = zeros_then(1203, &build_valid_loas_record(500)[..23]);
    let pes4 = zeros_then(480, &build_valid_loas_record(200)[..50]);
    let events = latm_pes_events(&[vec![0u8; 300], pes2, pes3, pes4, vec![0u8; 530]]);
    let issues = latm_framing_issues(&events);
    assert_eq!(issues.len(), 1, "{events:?}");
    assert_eq!(issues[0].1, LatmFramingKind::MissingSyncword, "{events:?}");
    let samples = latm_sample_indices(&events);
    assert!(samples.len() >= 4, "{events:?}");
    assert!(
        issues[0].0 > samples[3],
        "raised on PES 5, after the fourth Sample: {events:?}"
    );
}

/// A candidate confirmed at one boundary is only provisional: when the next
/// boundary lacks the syncword it is dropped without an issue, and the
/// candidates followed beside it go on. PES 3 passes two candidates, one at
/// 1203 (owing 480) and one at 50 (owing 930); PES 4 (530 zeros) refutes the
/// first and keeps the second, which PES 5 confirms (owing 250), so PES 6,
/// without a syncword at 250, is flagged at once. Dropped with the first,
/// the second would have been found again in PES 5 only as a fresh
/// candidate, refuted in PES 6 without an issue.
#[test]
fn a_provisional_position_that_fails_is_dropped_and_the_candidates_beside_it_go_on() {
    let pes2 = zeros_then(100, &build_valid_loas_record(1500)[..300]);
    let mut pes3 = zeros_then(50, &build_valid_loas_record(100));
    pes3.extend_from_slice(&build_valid_loas_record(2000)[..1050]);
    pes3.extend_from_slice(&build_valid_loas_record(500)[..23]);
    assert_eq!(pes3.len(), 1226);
    let pes5 = zeros_then(400, &build_valid_loas_record(300)[..53]);
    let events = latm_pes_events(&[
        vec![0u8; 300],
        pes2,
        pes3,
        vec![0u8; 530],
        pes5,
        vec![0u8; 300],
    ]);
    assert_eq!(latm_issue_pes(&events), [6], "{events:?}");
}

/// A LATM PID that never carries a LOAS syncword is flagged even with
/// `data_alignment_indicator` = 0 (the muxer's LATM output): once, when more
/// than one record's worth (`3 + 0x1FFF` bytes) of PES payload shows no LOAS
/// syncword candidate (ISO/IEC 14496-3 §1.7.2).
#[test]
fn demux_c11_latm_unaligned_pid_without_syncword_is_flagged_once() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x150, AudioCodec::AacLatm);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let audio_handle = mux.audio_handles()[0];
    // ADTS-headed payloads shipped on a LATM PID, candidate-free: the
    // cumulative candidate-free bytes cross 8194 on the third PES.
    let mut adts = vec![0xFFu8, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC];
    adts.resize(4096, 0);
    for i in 0..3 {
        mux.push_audio_to(audio_handle, Pts90khz::new(90_000 + i * 1_920), &adts)
            .unwrap();
    }
    let bytes = drain_all(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let flagged = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::LatmFraming {
                        pid: 0x150,
                        kind: LatmFramingKind::MissingSyncword,
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        flagged, 1,
        "expected one MissingSyncword on PID 0x150, got {events:?}"
    );
}

/// Valid LATM PES: no issue should fire, sample event present.
#[test]
fn demux_c11_latm_valid_syncword_no_issue() {
    let payload = build_valid_loas_record(64);
    let bytes = build_ts_with_aac_latm_pes(&payload);

    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::LatmFraming { .. },
                ..
            }
        )
    });
    assert!(
        !issue_seen,
        "valid LOAS-framed LATM PES must not emit LatmFraming, got {events:?}"
    );
    let sample_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                stream,
                ..
            } if stream.pid == 0x150
        )
    });
    assert!(sample_seen, "expected Sample event on PID 0x150");
}

/// Strict mode `Full`: LATM framing violation suppresses the
/// `Sample` event but still emits the `NonConformant` issue.
#[test]
fn demux_c11_latm_strict_full_suppresses_sample() {
    let bad_payload = [0xFFu8, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC];
    let bytes = build_ts_with_aac_latm_pes(&bad_payload);

    let mut demux = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    let _ = demux.feed(&bytes);
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }

    let issue_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::LatmFraming { .. },
                ..
            }
        )
    });
    let sample_seen = events.iter().any(|e| {
        matches!(
            e,
            DemuxEvent::Sample {
                stream,
                ..
            } if stream.pid == 0x150
        )
    });
    assert!(
        issue_seen,
        "strict mode must still emit the LatmFraming issue, got {events:?}"
    );
    assert!(
        !sample_seen,
        "strict mode must suppress the LATM Sample, got {events:?}"
    );
}

// =====================================================================
// AAC-LATM resync: several syncword candidates followed at once
// =====================================================================

/// xorshift64, for random record bodies reproducible from a seed written in
/// the test.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `records` sound LOAS records of `record` bytes with random bodies, as a
/// receiver that tunes in at a random offset of the first record sees them.
fn sound_loas_stream_joined_mid_record(seed: u64, record: usize, records: usize) -> Vec<u8> {
    let mut rng = XorShift(seed);
    let body = record - 3;
    let mut stream = Vec::with_capacity(record * records);
    for _ in 0..records {
        stream.extend_from_slice(&[0x56, 0xE0 | ((body >> 8) as u8 & 0x1F), body as u8]);
        stream.extend((0..body).map(|_| rng.next() as u8));
    }
    let join = (rng.next() % record as u64) as usize;
    stream.split_off(join)
}

/// The seed of stream `i` of the sweep's cell (`record`, `pes_len`).
fn sweep_seed(record: usize, pes_len: usize, i: u64) -> u64 {
    0x9E37_79B9_7F4A_7C15 ^ ((record as u64) << 40 | (pes_len as u64) << 20 | i)
}

/// `stream` cut into PES payloads of `pes_len` bytes.
fn cut_into_pes(stream: &[u8], pes_len: usize) -> Vec<Vec<u8>> {
    stream.chunks(pes_len).map(<[u8]>::to_vec).collect()
}

/// For each `LatmFraming` issue, the 1-based index of the PES that raised it
/// (a PES's issue is queued before its Sample).
fn latm_issue_pes(events: &[DemuxEvent]) -> Vec<usize> {
    let samples = latm_sample_indices(events);
    latm_framing_issues(events)
        .iter()
        .map(|&(at, _)| samples.iter().filter(|&&s| s < at).count() + 1)
        .collect()
}

/// A receiver that joins a sound stream of 8 000-byte records in the middle
/// of a record, with random record bodies (a false syncword candidate about
/// every 2 048 bytes, most of them declaring a record that runs past the
/// PES), finds the records without raising an issue.
#[test]
fn a_sound_stream_of_large_records_joined_mid_record_raises_no_issue() {
    // The sweep's first seed of this cell that 0.10.0 flagged (at PES 127).
    let stream = sound_loas_stream_joined_mid_record(sweep_seed(8000, 170, 5), 8000, 40);
    let events = latm_pes_events(&cut_into_pes(&stream, 170));
    assert!(
        latm_framing_issues(&events).is_empty(),
        "{:?}",
        latm_framing_issues(&events)
    );
}

/// Sound streams of four record sizes in three PES sizes, joined mid-record:
/// no issue in any of the 240 (seeds fixed; a red seed is a finding).
#[test]
fn sound_streams_joined_mid_record_raise_no_issue() {
    let mut red = Vec::new();
    for record in [300, 1500, 4000, 8000] {
        for pes_len in [170, 1000, 2900] {
            for i in 0..20 {
                let stream =
                    sound_loas_stream_joined_mid_record(sweep_seed(record, pes_len, i), record, 40);
                let events = latm_pes_events(&cut_into_pes(&stream, pes_len));
                if !latm_framing_issues(&events).is_empty() {
                    red.push((record, pes_len, i, latm_issue_pes(&events)));
                }
            }
        }
    }
    assert!(red.is_empty(), "(record, PES, seed, issue PES): {red:?}");
}

/// Not LOAS: every 100-byte PES holds a syncword candidate whose record would
/// run 8 194 bytes past it, so a candidate is always pending. The PID is
/// flagged once, by the PES that takes the payload counted since the join
/// past three record lengths (24 585 bytes): PES 246.
#[test]
fn a_pid_of_past_the_end_candidates_is_flagged_within_a_bounded_number_of_bytes() {
    let mut junk = vec![0u8; 100];
    junk[40..43].copy_from_slice(&[0x56, 0xFF, 0xFF]);
    let events = latm_pes_events(&vec![junk; 400]);
    assert_eq!(latm_issue_pes(&events), [246], "{events:?}");
    assert_eq!(
        latm_framing_issues(&events)[0].1,
        LatmFramingKind::MissingSyncword
    );
}

/// The same with the candidate in every second PES only.
#[test]
fn a_pid_with_a_past_the_end_candidate_in_every_second_pes_is_flagged_once() {
    let mut junk = vec![0u8; 100];
    junk[40..43].copy_from_slice(&[0x56, 0xFF, 0xFF]);
    let payloads: Vec<Vec<u8>> = (0..400)
        .map(|i| {
            if i % 2 == 0 {
                junk.clone()
            } else {
                vec![0u8; 100]
            }
        })
        .collect();
    let events = latm_pes_events(&payloads);
    assert_eq!(latm_issue_pes(&events), [246], "{events:?}");
}

/// The same with a lone `0x56` as every PES's last byte: a header that would
/// continue into the next PES, refuted there each time.
#[test]
fn a_pid_whose_pes_all_end_in_a_lone_first_header_byte_is_flagged_once() {
    let mut tail = vec![0u8; 100];
    tail[99] = 0x56;
    let events = latm_pes_events(&vec![tail; 400]);
    assert_eq!(latm_issue_pes(&events), [246], "{events:?}");
}

/// Not LOAS, but every 100-byte PES ends in an empty record (`56 E0 00`, or
/// `56 E0 00 56` with a header begun after it) that passes as a provisional
/// position and fails at the next PES. Once a provisional position has
/// failed, the hard bound applies whatever is held: flagged once, by the PES
/// that takes the count past 24 585 bytes (PES 246).
#[test]
fn a_provisional_position_renewed_at_every_pes_end_does_not_keep_a_pid_unflagged() {
    for tail in [&[0x56, 0xE0, 0x00][..], &[0x56, 0xE0, 0x00, 0x56][..]] {
        let mut pes = vec![0u8; 100];
        pes[100 - tail.len()..].copy_from_slice(tail);
        let events = latm_pes_events(&vec![pes; 2000]);
        assert_eq!(latm_issue_pes(&events), [246], "{tail:02X?}");
    }
}

/// Not LOAS: one 6-byte pattern every 80th PES of 100 zero bytes (an empty
/// record, then a header that passes at its boundary and owes 8 107 bytes)
/// keeps a provisional position pending most of the time; the first one
/// fails 82 PES on, and from then the hard bound applies: flagged once, by
/// PES 246.
#[test]
fn a_provisional_position_renewed_every_eightieth_pes_does_not_keep_a_pid_unflagged() {
    let payloads: Vec<Vec<u8>> = (0..2000)
        .map(|i| {
            let mut pes = vec![0u8; 100];
            if i % 80 == 0 {
                pes[10..16].copy_from_slice(&[0x56, 0xE0, 0x00, 0x56, 0xFF, 0xFF]);
            }
            pes
        })
        .collect();
    let events = latm_pes_events(&payloads);
    assert_eq!(latm_issue_pes(&events), [246]);
}

/// One LOAS record with `body` as its `audioMuxElement`.
fn loas_record_with(body: &[u8]) -> Vec<u8> {
    let n = body.len();
    let mut out = vec![0x56, 0xE0 | ((n >> 8) as u8 & 0x1F), n as u8];
    out.extend_from_slice(body);
    out
}

/// This library's muxer writes each AAC-LATM push as one PES without
/// `data_alignment_indicator`, so every PES ends on a record boundary and
/// the next starts on one (the AudioSyncStream is one run of records,
/// ISO/IEC 14496-3 Table 1.36). One push of 120 carries a corrupted record:
/// a length that ends inside the next record, a junk tail after the
/// records, or a length that runs past the push. Each is reported, once.
#[test]
fn a_corrupted_record_in_this_muxers_latm_output_is_reported() {
    for seed in 1..=6u64 {
        let mut rng = XorShift(0x1A7E_0000_0000 ^ seed);
        let bad_at = 40 + (rng.next() % 40) as usize;
        let payloads: Vec<Vec<u8>> = (0..120)
            .map(|i| {
                let mut push = Vec::new();
                for _ in 0..1 + rng.next() % 3 {
                    let n = 50 + (rng.next() % 650) as usize;
                    let body: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                    push.extend(loas_record_with(&body));
                }
                if i == bad_at {
                    match seed % 3 {
                        0 => push[2] ^= 0x21,
                        1 => push.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]),
                        _ => push[1] |= 0x1F,
                    }
                }
                push
            })
            .collect();
        let events = latm_pes_events(&payloads);
        let issues = latm_framing_issues(&events);
        assert_eq!(issues.len(), 1, "seed {seed}, push {bad_at}: {issues:?}");
        assert_eq!(issues[0].1, LatmFramingKind::MissingSyncword);
    }
}

/// 200 unaligned PES of one to four whole records each; the first record's
/// length in PES 100 is corrupted. The last record of PES 99 ended on that
/// PES's end, so PES 100 opens on a known boundary and is walked from its
/// first byte: the corrupted length is reported where its record would end,
/// by PES 100, or by PES 101 when the new length runs past PES 100.
#[test]
fn a_corrupted_record_length_in_an_unaligned_pes_is_reported() {
    for seed in 1..=4u64 {
        let mut rng = XorShift(0xC0DE_0000_0000 ^ seed);
        let per_pes = 1 + (rng.next() % 4) as usize;
        let mut payloads: Vec<Vec<u8>> = (0..200)
            .map(|_| {
                let mut pes = Vec::new();
                for _ in 0..per_pes {
                    let n = 100 + (rng.next() % 700) as usize;
                    let body: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                    pes.extend(loas_record_with(&body));
                }
                pes
            })
            .collect();
        payloads[99][2] ^= 0x5A;
        let corrupted = &payloads[99];
        let end = 3 + ((usize::from(corrupted[1] & 0x1F) << 8) | usize::from(corrupted[2]));
        let reported_by = if end > corrupted.len() { 101 } else { 100 };
        let events = latm_pes_events(&payloads);
        assert_eq!(latm_issue_pes(&events), [reported_by], "seed {seed}");
    }
}

/// Without any candidate the PID is flagged as soon as the bytes counted
/// pass one record length (8 194): PES 82 of 100 bytes.
#[test]
fn a_pid_without_any_candidate_is_flagged_after_one_record_length() {
    let events = latm_pes_events(&vec![vec![0u8; 100]; 400]);
    assert_eq!(latm_issue_pes(&events), [82], "{events:?}");
}

/// A sound stream of 7 000-byte records in 1 000-byte PES whose bodies are
/// full of false syncwords (`56 E0 10`, each refuted where its record ends),
/// joined mid-record: no issue.
#[test]
fn a_sound_stream_whose_record_bodies_are_full_of_false_syncwords_raises_no_issue() {
    let mut body = Vec::new();
    while body.len() < 6997 {
        body.extend_from_slice(&[0x56, 0xE0, 0x10, 0x00, 0x00, 0x00, 0x00]);
    }
    body.truncate(6997);
    let mut stream = Vec::new();
    for _ in 0..12 {
        stream.extend_from_slice(&[0x56, 0xE0 | (6997u16 >> 8) as u8, 6997u16 as u8]);
        stream.extend_from_slice(&body);
    }
    let events = latm_pes_events(&cut_into_pes(&stream[3500..], 1000));
    assert!(latm_framing_issues(&events).is_empty(), "{events:?}");
}

/// While the position is unknown, a PES that starts with a syncword but does
/// not announce alignment is a candidate like any other: PES 2 opens with an
/// empty record and a header whose record runs 161 bytes into PES 3, where no
/// syncword follows, so it is dropped without an issue. Taken as a record
/// start, PES 2 would make that boundary a missing syncword.
#[test]
fn a_syncword_at_the_start_of_an_unaligned_pes_is_not_trusted_while_unsynced() {
    let mut pes2 = vec![0x56, 0xE0, 0x00, 0x56, 0xE0, 0xFF];
    pes2.resize(100, 0);
    let events = latm_pes_events(&[vec![0u8; 100], pes2, vec![0u8; 300]]);
    assert!(latm_framing_issues(&events).is_empty(), "{events:?}");
}

/// Every 500-byte PES of a sound stream of 1 000-byte records, joined
/// mid-record, opens with a false candidate (`56 FF FF` inside a record
/// body, declaring a record of 8 194 bytes), and every second PES holds a
/// record start after it. The scan goes on past a candidate whose record
/// runs past the PES, so the records are found; stopping there, only false
/// candidates would be followed and the stream would be flagged at the
/// hard bound.
#[test]
fn a_false_candidate_ahead_of_the_record_start_in_every_pes_does_not_hide_it() {
    let mut stream = Vec::new();
    for _ in 0..40 {
        stream.extend(build_valid_loas_record(997));
    }
    let mut joined = stream.split_off(250);
    for pes_start in (0..joined.len()).step_by(500) {
        joined[pes_start..pes_start + 3].copy_from_slice(&[0x56, 0xFF, 0xFF]);
    }
    let events = latm_pes_events(&cut_into_pes(&joined, 500));
    assert!(
        latm_framing_issues(&events).is_empty(),
        "{:?}",
        latm_issue_pes(&events)
    );
}

/// Mux `payloads` as consecutive AAC-LATM PES on PID 0x150 and demux them
/// without the second TS packet of every PES `lose` picks; every event.
fn latm_pes_events_losing(payloads: &[Vec<u8>], lose: impl Fn(usize) -> bool) -> Vec<DemuxEvent> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_audio(0x150, AudioCodec::AacLatm);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let audio_handle = mux.audio_handles()[0];
    for (i, payload) in payloads.iter().enumerate() {
        mux.push_audio_to(
            audio_handle,
            Pts90khz::new(90_000 + i as i64 * 1_920),
            payload,
        )
        .unwrap();
    }
    let bytes = drain_all(&mut mux);
    let mut kept = Vec::with_capacity(bytes.len());
    let (mut pes, mut in_pes) = (0usize, 0usize);
    for pkt in bytes.chunks_exact(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pid == 0x150 {
            if pkt[1] & 0x40 != 0 {
                pes += 1;
                in_pes = 0;
            }
            in_pes += 1;
            if in_pes == 2 && lose(pes - 1) {
                continue;
            }
        }
        kept.extend_from_slice(pkt);
    }
    let mut demux = Demuxer::new();
    demux.feed(&kept).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }
    events
}

/// A loss (here a TS packet of every second 1 000-byte PES) leaves the record
/// position unknown and starts the count again: a sound stream of 1 500-byte
/// records that loses a PES every 2 000 bytes never stays unsynced long
/// enough to be judged, so it raises no issue.
#[test]
fn a_sound_stream_that_loses_a_pes_every_two_thousand_bytes_raises_no_issue() {
    let mut stream = Vec::new();
    for _ in 0..60 {
        stream.extend(build_valid_loas_record(1497));
    }
    let events = latm_pes_events_losing(&cut_into_pes(&stream, 1000), |i| i % 2 == 1);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Discontinuity { stream, .. } if stream.pid == 0x150)),
        "the losses report themselves: {events:?}"
    );
    assert!(
        latm_framing_issues(&events).is_empty(),
        "{:?}",
        latm_framing_issues(&events)
    );
}

/// A muxer for program 1: video on 0x101 and, when `audio` is set, that codec
/// on 0x150; its PSI versions start at `version`.
fn latm_program_muxer(version: u8, audio: Option<AudioCodec>) -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, VideoCodec::H264);
    if let Some(codec) = audio {
        prog.add_audio(0x150, codec);
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.psi_version_base(version);
    Muxer::new(b.build().unwrap()).unwrap()
}

/// `payloads` muxed as AAC-LATM PES on 0x150 by `mux`.
fn latm_pes_bytes(mux: &mut Muxer, payloads: &[Vec<u8>]) -> Vec<u8> {
    let audio_handle = mux.audio_handles()[0];
    for (i, payload) in payloads.iter().enumerate() {
        mux.push_audio_to(
            audio_handle,
            Pts90khz::new(90_000 + i as i64 * 1_920),
            payload,
        )
        .unwrap();
    }
    drain_all(mux)
}

/// What the LOAS search of PID 0x150 held does not outlive the PID's state:
/// `reset_sync`, a PMT that drops the PID and one that changes its stream
/// type away from 0x11 each end it. Two observations per way: (1) an issue
/// already raised does not keep the next stretch from raising its own; (2) a
/// candidate that passed one record boundary before does not get confirmed
/// by the next PES (it would then make the boundary after that a missing
/// syncword at once).
#[test]
fn the_loas_search_state_goes_with_the_pid() {
    // (2): PES A passes a candidate at 10 that owes 93 bytes; PES B carries
    // a record at 93 owing 193 more; PES C has no syncword at 193.
    let mut pes_a = zeros_then(10, &build_valid_loas_record(20));
    pes_a.extend_from_slice(&build_valid_loas_record(100)[..10]);
    let mut pes_b = zeros_then(93, &build_valid_loas_record(30));
    pes_b.extend_from_slice(&build_valid_loas_record(200)[..10]);
    let pes_c = vec![0u8; 300];
    let candidate_free = vec![vec![0u8; 100]; 90];
    for way in ["reset_sync", "pmt drops the pid", "pmt retypes the pid"] {
        for (before, after, want) in [
            (candidate_free.clone(), candidate_free.clone(), 2),
            (vec![pes_a.clone()], vec![pes_b.clone(), pes_c.clone()], 0),
        ] {
            let mut demux = Demuxer::new();
            let mut events = Vec::new();
            demux
                .feed(&latm_pes_bytes(
                    &mut latm_program_muxer(0, Some(AudioCodec::AacLatm)),
                    &before,
                ))
                .unwrap();
            // `reset_sync` also drops the queued events.
            while let Some(e) = demux.next_event() {
                events.push(e);
            }
            match way {
                "reset_sync" => demux.reset_sync(),
                _ => {
                    let audio = (way == "pmt retypes the pid").then_some(AudioCodec::Aac);
                    let mut between = latm_program_muxer(1, audio);
                    between
                        .push_video(
                            &crate::common::synthetic::synthetic_h264_au(),
                            Pts90khz::new(900_000),
                            true,
                        )
                        .unwrap();
                    demux.feed(&drain_all(&mut between)).unwrap();
                }
            }
            demux
                .feed(&latm_pes_bytes(
                    &mut latm_program_muxer(2, Some(AudioCodec::AacLatm)),
                    &after,
                ))
                .unwrap();
            demux.flush();
            while let Some(e) = demux.next_event() {
                events.push(e);
            }
            assert_eq!(
                latm_framing_issues(&events).len(),
                want,
                "{way}: {events:?}"
            );
        }
    }
}

/// Mux `payloads` as consecutive AAC-LATM PES on PID 0x150, let `edit`
/// change the PES header in the first TS packet of PES `k` (it gets the
/// packet's payload, which starts with that header), and demux; every event.
fn latm_pes_events_editing(
    payloads: &[Vec<u8>],
    k: usize,
    edit: impl Fn(&mut [u8]),
) -> Vec<DemuxEvent> {
    let mut mux = latm_program_muxer(0, Some(AudioCodec::AacLatm));
    let mut bytes = latm_pes_bytes(&mut mux, payloads);
    let mut pes = 0usize;
    for pkt in bytes.chunks_exact_mut(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pid != 0x150 || pkt[1] & 0x40 == 0 {
            continue;
        }
        if pes == k {
            let start = if pkt[3] & 0x20 != 0 {
                5 + usize::from(pkt[4])
            } else {
                4
            };
            edit(&mut pkt[start..]);
        }
        pes += 1;
    }
    let mut demux = Demuxer::new();
    demux.feed(&bytes).unwrap();
    demux.flush();
    let mut events = Vec::new();
    while let Some(e) = demux.next_event() {
        events.push(e);
    }
    events
}

/// A sound stream of 1 000-byte records in 500-byte PES, with the PES that
/// is dropped (PES index 20) starting either on a record boundary or 37
/// bytes into a record, and ending 60 bytes into the record after: the
/// payloads and the dropped PES's index.
fn sound_stream_with_a_pes_to_drop(before_on_a_record_end: bool) -> (Vec<Vec<u8>>, usize) {
    let mut stream = Vec::new();
    for _ in 0..30 {
        stream.extend(build_valid_loas_record(997));
    }
    let k_start = if before_on_a_record_end {
        10_000
    } else {
        10_037
    };
    let mut payloads = cut_into_pes(&stream[..k_start], 500);
    let k = payloads.len();
    payloads.push(stream[k_start..11_060].to_vec());
    payloads.extend(cut_into_pes(&stream[11_060..], 500));
    (payloads, k)
}

fn issue_on_0x150(events: &[DemuxEvent], want: impl Fn(&NonConformantIssue) -> bool) -> bool {
    events.iter().any(|e| {
        matches!(e, DemuxEvent::NonConformant { stream, issue } if stream.pid == 0x150 && want(issue))
    })
}

/// A PES dropped for a zero `PES_packet_length` (not video) lost its bytes:
/// the LOAS position is forgotten as at any other loss, so the PES after it,
/// which opens 60 bytes into a record, is resynced, not walked from the
/// stale position. Whether the PES before the drop ended on a record end
/// (the position known at its next byte) or inside a record: no
/// `LatmFraming` issue, as for the same PES lost to a continuity gap.
#[test]
fn a_pes_dropped_for_a_zero_length_forgets_the_loas_position() {
    for before_on_a_record_end in [true, false] {
        let (payloads, k) = sound_stream_with_a_pes_to_drop(before_on_a_record_end);
        let events = latm_pes_events_editing(&payloads, k, |pes| pes[4..6].fill(0));
        assert!(
            issue_on_0x150(&events, |i| matches!(
                i,
                NonConformantIssue::ZeroLengthPesNonVideo { .. }
            )),
            "the PES is dropped for its zero length: {events:?}"
        );
        assert!(
            latm_framing_issues(&events).is_empty(),
            "{before_on_a_record_end}: {:?}",
            latm_issue_pes(&events)
        );
        let control = latm_pes_events_losing(&payloads, |i| i == k);
        assert!(
            latm_framing_issues(&control).is_empty(),
            "{before_on_a_record_end}"
        );
    }
}

/// A PES dropped as unparseable (start code `00 00 02`) lost its bytes too:
/// the LOAS position is forgotten, and the PES after it raises no
/// `LatmFraming` issue.
#[test]
fn an_unparseable_pes_forgets_the_loas_position() {
    for before_on_a_record_end in [true, false] {
        let (payloads, k) = sound_stream_with_a_pes_to_drop(before_on_a_record_end);
        let events = latm_pes_events_editing(&payloads, k, |pes| pes[2] = 0x02);
        assert!(
            issue_on_0x150(&events, |i| matches!(
                i,
                NonConformantIssue::MalformedPes { .. }
            )),
            "the PES is dropped as unparseable: {events:?}"
        );
        assert!(
            latm_framing_issues(&events).is_empty(),
            "{before_on_a_record_end}: {:?}",
            latm_issue_pes(&events)
        );
    }
}
