//! End-to-end demuxer tests for the opt-in malformed-CFI tolerance mode.
//!
//! Covers the 5 scenarios that form the minimum-discriminating set for the
//! tolerance design:
//!
//! 1. Strict mode (default): orphan Middle/Last with self-consistent KLV
//!    inside still emits only `NonConformantIssue::MultiCellAu { Orphan }`
//!    and zero metadata events.
//! 2. Tolerance mode: same wire produces one
//!    `MetadataKind::KlvSyncAuCell { cell_fragment_indication: Complete,
//!    was_reassembled: false, cell_count: 1 }` event PLUS one
//!    `NonConformantIssue::CfiTolerated` diagnostic.
//! 3. Tolerance mode + invalid orphan payload (no SMPTE UL OR BER length
//!    mismatch) → still no metadata, still orphan diagnostic.
//! 4. Tolerance mode + legitimate First/Middle/Last reassembly → one
//!    reassembled metadata event, ZERO `CfiTolerated`
//!    diagnostics. The new path must not steal cells from real fragmentation.
//! 5. Tolerance mode + active buffer + sequence-gap Middle → existing
//!    `MultiCellAu { SequenceGap }` still fires (tolerance only rescues
//!    *orphan* Middle/Last, not mid-buffer corruption).
//!
//! All tests reuse the same wire-bytes patching strategy as
//! `mpegts_au_reassembly.rs`: muxer emits a baseline TS, we surgically
//! patch the 5-byte AU cell header (and optionally the inner payload) of
//! the PUSI packet on the KLV PID. The muxer always writes Complete CFI,
//! so patches are what drive the demuxer through the non-conformant paths.

use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, DemuxerConfigBuilder, MetadataKind, NonConformantIssue,
    event::MultiCellAuReason,
};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

const KLV_PID: u16 = 0x1031;

/// MISB ST 0601 UAS Datalink LS UL + BER short-form length + N value bytes.
/// Total `17 + value_len`. Self-consistent → passes the tolerance validator
/// when `value_len < 128`.
fn synth_klv_record(value_len: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(17 + value_len as usize);
    v.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    v.push(value_len);
    v.extend(std::iter::repeat_n(0x42u8, value_len as usize));
    v
}

fn build_muxer() -> Muxer {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(KLV_PID, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    Muxer::new(cfg).unwrap()
}

fn drain(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Find byte offset of the n-th PUSI AU cell on `KLV_PID`. Same shape as
/// the helper in `mpegts_au_reassembly.rs`.
fn locate_nth_au_cell_offset(ts_bytes: &[u8], n: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (pkt_idx, pkt) in ts_bytes.chunks_exact(188).enumerate() {
        let pkt_pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pkt_pid != KLV_PID {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        if !pusi {
            continue;
        }
        let af_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if af_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        if idx + 9 > 188 {
            continue;
        }
        let pes_header_data_length = pkt[idx + 8] as usize;
        idx += 9 + pes_header_data_length;
        if seen == n {
            return Some(pkt_idx * 188 + idx);
        }
        seen += 1;
    }
    None
}

/// Rewrite the 5-byte AU cell header + N inner bytes at `offset`. Caller
/// MUST ensure `5 + inner.len()` does not exceed the original cell footprint
/// (we push back into the same byte run).
fn patch_au_cell(
    ts_bytes: &mut [u8],
    offset: usize,
    cfi: CellFragmentIndication,
    seq: u8,
    inner: &[u8],
) {
    let hdr = AuCellHeader {
        metadata_service_id: 0x00,
        sequence_number: seq,
        cell_fragment_indication: cfi,
        decoder_config_flag: false,
        random_access_indicator: true,
    };
    let mut buf = Vec::with_capacity(5 + inner.len());
    write_metadata_au_cell(&mut buf, hdr, inner).unwrap();
    ts_bytes[offset..offset + buf.len()].copy_from_slice(&buf);
}

fn collect_events_with(builder: DemuxerConfigBuilder, ts_bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut dem = Demuxer::with_config(builder.build());
    dem.feed(ts_bytes).unwrap();
    let mut events = Vec::new();
    while let Some(e) = dem.next_event() {
        events.push(e);
    }
    events
}

// Tolerance is on by default, so every caller in this file passes an
// explicit builder (`collect_events_with(DemuxerConfig::builder()
// .cfi_tolerance(false), ...)` for the strict path).

/// Pump enough video frames to advance PTS past the PSI cadence threshold
/// (~100 ms), ensuring PMT lands before the KLV PES on the demuxer.
fn pump_video(mux: &mut Muxer, frame_count: usize, base_pts_ticks: i64) {
    let nal = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    for i in 0..frame_count {
        let pts = base_pts_ticks + (i as i64) * 3_000;
        mux.push_video(&nal, Pts90khz::new(pts), false).unwrap();
    }
}

/// Build a TS containing a single PES on `KLV_PID` whose AU cell carries
/// `inner` and is patched to `cfi` / `seq`. Returns the wire bytes.
fn ts_with_patched_single_cell(
    inner_for_push: &[u8],
    patched_inner: &[u8],
    cfi: CellFragmentIndication,
    seq: u8,
) -> Vec<u8> {
    let mut mux = build_muxer();
    pump_video(&mut mux, 5, 90_000);
    mux.push_klv(inner_for_push, Pts90khz::new(90_000), 0x00)
        .unwrap();
    let mut ts = drain(&mut mux);
    let off = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 on KLV_PID");
    patch_au_cell(&mut ts, off, cfi, seq, patched_inner);
    ts
}

// ── Scenario 1: strict mode + orphan Middle with complete KLV ─────────────

#[test]
fn strict_mode_orphan_middle_with_complete_klv_stays_orphan() {
    let inner = synth_klv_record(32);
    let ts = ts_with_patched_single_cell(&inner, &inner, CellFragmentIndication::Middle, 7);

    // Default is now `cfi_tolerance: true`; this test specifically
    // exercises the strict path, so opt out explicitly.
    let builder = DemuxerConfig::builder().cfi_tolerance(false);
    let events = collect_events_with(builder, &ts);

    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    assert!(
        metas.is_empty(),
        "strict mode: orphan Middle MUST NOT emit metadata even if inner is valid KLV \
         (got {} metadata events)",
        metas.len(),
    );

    let orphans: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MultiCellAu {
                        reason: MultiCellAuReason::Orphan,
                        ..
                    },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        orphans.len(),
        1,
        "exactly one MultiCellAu{{Orphan}} expected"
    );

    let tolerated: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        tolerated.is_empty(),
        "strict mode must not emit CfiTolerated diagnostics",
    );
}

// ── Scenario 2: tolerance mode + orphan Middle with complete KLV ──────────

#[test]
fn tolerance_mode_valid_orphan_middle_emits_metadata_plus_diagnostic() {
    let inner = synth_klv_record(32);
    let ts = ts_with_patched_single_cell(&inner, &inner, CellFragmentIndication::Middle, 7);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let metas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Metadata { kind, payload, .. } => Some((kind, payload)),
            _ => None,
        })
        .collect();
    assert_eq!(
        metas.len(),
        1,
        "tolerance mode: one metadata event expected"
    );
    match metas[0].0 {
        MetadataKind::KlvSyncAuCell {
            cell_fragment_indication,
            was_reassembled,
            cell_count,
            ..
        } => {
            assert_eq!(
                *cell_fragment_indication,
                CellFragmentIndication::Complete,
                "tolerance mode collapses observed CFI to Complete on emit",
            );
            assert!(
                !was_reassembled,
                "tolerated single cell: was_reassembled=false"
            );
            assert_eq!(*cell_count, 1, "tolerated single cell: cell_count=1");
        }
        _ => panic!("expected KlvSyncAuCell metadata"),
    }
    assert_eq!(
        *metas[0].1, inner,
        "tolerated cell payload preserved verbatim"
    );

    let tolerated: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::CfiTolerated {
                        pid,
                        observed_cfi,
                        treated_as,
                    },
                ..
            } => Some((*pid, *observed_cfi, *treated_as)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tolerated.len(),
        1,
        "tolerance mode: one tolerance diagnostic expected",
    );
    assert_eq!(tolerated[0].0, KLV_PID, "diagnostic carries KLV PID");
    assert_eq!(
        tolerated[0].1,
        CellFragmentIndication::Middle,
        "diagnostic reports the wire-observed CFI",
    );
    assert_eq!(
        tolerated[0].2,
        CellFragmentIndication::Complete,
        "diagnostic reports the substituted CFI",
    );

    let orphans = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MultiCellAu { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        orphans, 0,
        "tolerance mode replaces the orphan diagnostic — must not emit BOTH",
    );
}

// ── Scenario 2b: tolerance mode also rescues orphan Last ──────────────────

#[test]
fn tolerance_mode_valid_orphan_last_emits_metadata_plus_diagnostic() {
    let inner = synth_klv_record(8);
    let ts = ts_with_patched_single_cell(&inner, &inner, CellFragmentIndication::Last, 3);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let meta_count = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .count();
    assert_eq!(
        meta_count, 1,
        "orphan Last with valid KLV: 1 metadata event"
    );

    let tolerated_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated {
                        observed_cfi: CellFragmentIndication::Last,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(tolerated_count, 1, "diagnostic reports observed_cfi: Last");
}

// ── Scenario 3: tolerance mode + invalid orphan payload ───────────────────

#[test]
fn tolerance_mode_invalid_payload_stays_orphan() {
    // 32 bytes of 0xFF — does not match SMPTE UL prefix, so the validator
    // rejects it and we fall through to the existing orphan path.
    let inner = vec![0xFFu8; 32];
    let ts = ts_with_patched_single_cell(&inner, &inner, CellFragmentIndication::Middle, 5);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let meta_count = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .count();
    assert_eq!(
        meta_count, 0,
        "tolerance mode + non-KLV payload: no metadata emitted",
    );

    let orphan_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MultiCellAu {
                        reason: MultiCellAuReason::Orphan,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        orphan_count, 1,
        "tolerance mode + non-KLV payload: existing Orphan diagnostic fires",
    );

    let tolerated_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        tolerated_count, 0,
        "tolerance diagnostic must NOT fire when the payload validator rejected",
    );
}

// ── Scenario 3b: tolerance mode + UL-prefixed but BER-length-wrong ────────

#[test]
fn tolerance_mode_ber_length_mismatch_stays_orphan() {
    // 32-byte payload that starts with the SMPTE UL but whose BER length
    // byte (position 16) declares the wrong size.
    let mut inner = synth_klv_record(15);
    inner[16] = 64; // declares 64 bytes but only 15 follow
    let ts = ts_with_patched_single_cell(&inner, &inner, CellFragmentIndication::Middle, 8);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let meta_count = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .count();
    let tolerated_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(meta_count, 0, "BER length mismatch: no metadata");
    assert_eq!(
        tolerated_count, 0,
        "BER length mismatch: no tolerance event"
    );
}

// ── Scenario 4: tolerance mode + legitimate First/Middle/Last reassembly ──

#[test]
fn tolerance_mode_legitimate_fragmentation_unaffected() {
    // 3 PESes on KLV_PID — patched to First/Middle/Last so the reassembler's
    // normal multi-cell path runs to completion. With tolerance mode ON,
    // the tolerance branch must NOT also fire (no orphans involved).
    let part_a = b"AAA";
    let part_b = b"BBB";
    let part_c = b"CCC";
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);
    mux.push_klv(part_a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(part_b, Pts90khz::new(91_000), 0x00).unwrap();
    mux.push_klv(part_c, Pts90khz::new(92_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1");
    let off2 = locate_nth_au_cell_offset(&ts, 2).expect("PUSI 2");
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, part_a);
    patch_au_cell(&mut ts, off1, CellFragmentIndication::Middle, 11, part_b);
    patch_au_cell(&mut ts, off2, CellFragmentIndication::Last, 12, part_c);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let metas: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Metadata { kind, payload, .. } => Some((kind, payload)),
            _ => None,
        })
        .collect();
    assert_eq!(
        metas.len(),
        1,
        "legitimate F/M/L: 1 reassembled metadata event"
    );
    match metas[0].0 {
        MetadataKind::KlvSyncAuCell {
            was_reassembled,
            cell_count,
            ..
        } => {
            assert!(*was_reassembled, "real fragmentation: was_reassembled=true");
            assert_eq!(*cell_count, 3, "3 cells contributed");
        }
        _ => panic!("expected KlvSyncAuCell"),
    }
    assert_eq!(metas[0].1.as_slice(), b"AAABBBCCC");

    let tolerated = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        tolerated, 0,
        "tolerance must not fire when the reassembler successfully handles real fragments",
    );
}

// ── Scenario 5: tolerance mode + active buffer + sequence gap ─────────────

#[test]
fn tolerance_mode_active_buffer_sequence_gap_still_emits_existing_failure() {
    // First cell opens the buffer; then a Middle with the WRONG seq number
    // triggers SequenceGap. Tolerance must NOT rescue this — it only
    // touches Orphan continuations on an EMPTY buffer.
    let part_a = b"DDD";
    let part_b = b"EEE";
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);
    mux.push_klv(part_a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(part_b, Pts90khz::new(91_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1");
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, part_a);
    // Skip seq 11 — go straight to 12 to trigger SequenceGap.
    patch_au_cell(&mut ts, off1, CellFragmentIndication::Middle, 12, part_b);

    let builder = DemuxerConfig::builder().cfi_tolerance(true);
    let events = collect_events_with(builder, &ts);

    let meta_count = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .count();
    assert_eq!(
        meta_count, 0,
        "SequenceGap: no metadata (the AU was dropped, not rescued)",
    );

    let seq_gap_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MultiCellAu {
                        reason: MultiCellAuReason::SequenceGap,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        seq_gap_count, 1,
        "SequenceGap: existing diagnostic fires regardless of tolerance flag",
    );

    let tolerated_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::CfiTolerated { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        tolerated_count, 0,
        "tolerance must not rescue mid-buffer SequenceGap failures",
    );
}
