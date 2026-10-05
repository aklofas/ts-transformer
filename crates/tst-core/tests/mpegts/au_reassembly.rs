//! End-to-end demuxer tests for multi-cell Metadata AU cell reassembly.
//!
//! Wires every state-table row from the per-PID `AuCellReassembler`
//! through the full `Demuxer::feed` path:
//!
//! 1. Single Complete AU → one `Sample`, `was_reassembled=false`, `cell_count=1`.
//! 2. 3-cell First/Middle/Last across 3 PESes → one `Sample`,
//!    `was_reassembled=true`, `cell_count=3`.
//! 3. 3 Complete cells back-to-back in ONE PES → three `Sample` events
//!    (regression for the pre-reassembly classify_klv shape where only the
//!    first cell of a multi-cell PES surfaced).
//! 4. Orphan continuation cell (Middle without prior First) → no `Sample`,
//!    `NonConformant(MultiCellAu { reason: Orphan, .. })`.
//! 5. SequenceGap mid-buffer → no `Sample`,
//!    `NonConformant(MultiCellAu { reason: SequenceGap, .. })`.
//! 6. ConcurrentFirst (First then First) followed by Last → one
//!    `NonConformant(ConcurrentFirst)` then one `Sample` for the new AU.
//! 7. Overflow → no `Sample`,
//!    `NonConformant(MultiCellAu { reason: Overflow, .. })`,
//!    `dropped_bytes = buffered + incoming`.
//! 8. `Demuxer::reset_sync` mid-buffering → buffer cleared silently (no
//!    NonConformant), and a post-reset orphan demonstrates the buffer
//!    really is empty.
//!
//! Helper strategy: use the muxer to produce a valid PAT + PMT + one PES
//! per `push_klv` call, then surgically patch the 5-byte AU cell header
//! and (where needed) the inner payload of the PUSI packet for the KLV PID.
//! The muxer always emits Complete-CFI cells; patches override CFI and
//! the cell sequence_number to drive the reassembler through every
//! state-table row. PSI/CC + PES headers + TS framing remain valid.

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

/// Minimal H.222.0-shaped raw KLV LS bytes (ST 0601 UL + BER short-form
/// length = 1 + 1 payload byte). The reassembler does not care about
/// the inner shape; the muxer's auto-wrap path doesn't validate inner
/// bytes either. Any non-empty slice works.
fn synthetic_klv_ls() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    v.push(0x01); // BER length = 1
    v.push(0xAB); // one payload byte
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

/// Find the byte offset (within `ts_bytes`) of the AU cell header bytes
/// for the n-th (0-indexed) PUSI packet on `KLV_PID`. Returns the offset
/// of the first AU cell header byte (`metadata_service_id`); the
/// subsequent 4 bytes are sequence_number, flags, length_hi, length_lo,
/// followed by the inner payload.
///
/// Returns `None` if fewer than `n+1` PUSI packets on `KLV_PID` exist.
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
        // PES header skip: start_code(3) + stream_id(1) + length(2) +
        // flags1(1) + flags2(1) + header_data_length(1) + optional fields.
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

/// Patch the AU cell at `offset` to express `(cfi, seq)` and replace the
/// inner payload with `inner`. Caller MUST ensure the cell's original
/// length is ≥ `inner.len()` — we patch in-place and resize via the
/// 16-bit `AU_cell_data_length` field; the surrounding TS packet bytes
/// past the cell footprint are not touched. The muxer auto-wraps with a
/// payload of `inner.len()` bytes at write time, so passing the same
/// length back is safe.
fn patch_au_cell(
    ts_bytes: &mut [u8],
    offset: usize,
    cfi: CellFragmentIndication,
    seq: u8,
    inner: &[u8],
) {
    // Rebuild the 5-byte header + N payload bytes in place. Reserved
    // 4-bit field emitted as 0b1111 to match the muxer's convention.
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

/// Run a TS byte stream through a fresh `Demuxer` and collect every
/// `DemuxEvent` it emits.
fn collect_events(ts_bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut dem = Demuxer::new();
    dem.feed(ts_bytes).unwrap();
    let mut events = Vec::new();
    while let Some(e) = dem.next_event() {
        events.push(e);
    }
    events
}

/// Same as `collect_events` but uses a custom-configured demuxer (for
/// the Overflow test, which lowers `au_cell_cap_per_pid`).
fn collect_events_with(builder: DemuxerConfigBuilder, ts_bytes: &[u8]) -> Vec<DemuxEvent> {
    let mut dem = Demuxer::with_config(builder.build());
    dem.feed(ts_bytes).unwrap();
    let mut events = Vec::new();
    while let Some(e) = dem.next_event() {
        events.push(e);
    }
    events
}

/// Pump enough video frames into the muxer to advance PTS past the PSI
/// cadence threshold (~100 ms). Without this, the early KLV PES may
/// arrive at the demuxer before the PMT, which classifies the PID as
/// `Unknown` and skips AU cell parsing entirely.
fn pump_video(mux: &mut Muxer, frame_count: usize, base_pts_ticks: i64) {
    let nal = [0x00, 0x00, 0x00, 0x01, 0x09, 0x10]; // AUD NAL
    for i in 0..frame_count {
        let pts = base_pts_ticks + (i as i64) * 3_000;
        mux.push_video(&nal, Pts90khz::new(pts), false).unwrap();
    }
}

// ── Test 1: single Complete AU ──────────────────────────────────────────

#[test]
fn single_complete_cell_emits_sample_with_was_reassembled_false() {
    let mut mux = build_muxer();
    pump_video(&mut mux, 5, 90_000);
    let inner = synthetic_klv_ls();
    mux.push_klv(&inner, Pts90khz::new(90_000), 0x00).unwrap();
    let ts = drain(&mut mux);

    let events = collect_events(&ts);
    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    assert_eq!(metas.len(), 1, "exactly one metadata event expected");
    match metas[0] {
        DemuxEvent::Metadata {
            kind:
                MetadataKind::KlvSyncAuCell {
                    was_reassembled,
                    cell_count,
                    ..
                },
            payload,
            ..
        } => {
            assert!(
                !was_reassembled,
                "single Complete cell: was_reassembled=false"
            );
            assert_eq!(*cell_count, 1, "single Complete cell: cell_count=1");
            assert_eq!(*payload, inner, "inner KLV bytes preserved");
        }
        _ => unreachable!(),
    }
    // No MultiCellAu NonConformant on the happy path.
    assert!(
        !events.iter().any(|e| matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MultiCellAu { .. },
                ..
            }
        )),
        "no MultiCellAu NonConformant on single-cell happy path",
    );
}

// ── Test 2: 3-cell AU across 3 PESes ────────────────────────────────────

#[test]
fn three_cell_au_across_three_pes_reassembles() {
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);

    // Three pushes → three PES packets on KLV_PID. Each carries one
    // Complete cell wrapped by the muxer. After draining we patch the
    // 1st PES's cell to First (seq 10), 2nd to Middle (seq 11), 3rd to
    // Last (seq 12). The inner payloads are 3 bytes each ("AAA", "BBB",
    // "CCC") so the reassembled inner is "AAABBBCCC".
    let part_a = b"AAA";
    let part_b = b"BBB";
    let part_c = b"CCC";
    // Muxer's push_klv expects an inner length matching what we'll patch
    // back, so push with the same lengths.
    mux.push_klv(part_a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(part_b, Pts90khz::new(91_000), 0x00).unwrap();
    mux.push_klv(part_c, Pts90khz::new(92_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1 found");
    let off2 = locate_nth_au_cell_offset(&ts, 2).expect("PUSI 2 found");
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, part_a);
    patch_au_cell(&mut ts, off1, CellFragmentIndication::Middle, 11, part_b);
    patch_au_cell(&mut ts, off2, CellFragmentIndication::Last, 12, part_c);

    let events = collect_events(&ts);
    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    assert_eq!(metas.len(), 1, "exactly one reassembled metadata event");
    match metas[0] {
        DemuxEvent::Metadata {
            kind:
                MetadataKind::KlvSyncAuCell {
                    was_reassembled,
                    cell_count,
                    sequence_number,
                    cell_fragment_indication,
                    ..
                },
            payload,
            ..
        } => {
            assert!(*was_reassembled, "multi-cell AU: was_reassembled=true");
            assert_eq!(*cell_count, 3, "3 cells contributed");
            assert_eq!(*sequence_number, 10, "First cell's seq surfaces on emit");
            assert_eq!(
                *cell_fragment_indication,
                CellFragmentIndication::Complete,
                "reassembled emit collapses CFI to Complete",
            );
            assert_eq!(payload.as_slice(), b"AAABBBCCC");
        }
        _ => unreachable!(),
    }
    assert!(
        !events.iter().any(|e| matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MultiCellAu { .. },
                ..
            }
        )),
        "no MultiCellAu NonConformant on multi-cell happy path",
    );
}

// ── Test 3: 3 Complete cells back-to-back in ONE PES ────────────────────

#[test]
fn three_complete_cells_back_to_back_in_one_pes_emit_three_samples() {
    // Push one large blob that the muxer wraps in a single Complete AU
    // cell. We then OVERWRITE that single 5+N-byte cell with three
    // back-to-back 5+M-byte Complete cells. Total bytes are preserved
    // so the surrounding PES header's PES_packet_length stays valid.
    //
    // Pre-Task-4 the demuxer's classify_klv only walked the FIRST cell
    // — three back-to-back Completes emitted only one Sample. Post-
    // Task-4 (via the reassembler's empty-state row 1) every Complete
    // cell emits its own Sample.
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);

    // Wrap 21 bytes ("zzzz...") which the muxer will package as one
    // Complete cell. We then overwrite the inner payload region with
    // three back-to-back 5+2 = 7-byte Complete cells (3 × 7 = 21).
    // The outer envelope (5-byte header originally written by the muxer)
    // is also overwritten — total payload length 26 = 5 + 21 bytes.
    let original_inner = vec![0u8; 21];
    mux.push_klv(&original_inner, Pts90khz::new(90_000), 0x00)
        .unwrap();
    let mut ts = drain(&mut mux);

    // We want the PES payload region to contain three 7-byte Complete
    // cells (header 5 + inner 2). The PES payload region today is
    // header(5) + inner(21) = 26 bytes. Overwrite with 7+7+7+5 = 26.
    // The trailing 5 bytes look like a 4th cell header with
    // AU_cell_data_length = 0 (5-byte header, no payload) so the
    // reassembler emits exactly 3 samples for the 7-byte cells and one
    // more for the empty cell — adjust to 4 total samples.
    // Simpler: make 3 × 7 = 21 bytes of patched payload, then a
    // 5-byte ZERO-PAYLOAD trailing Complete cell to consume exactly
    // the remaining 5 bytes (so the total still adds to 26).
    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    // Build the multi-cell payload region: 3 × (5-byte hdr + 2-byte payload)
    //                                    + 1 × (5-byte hdr + 0-byte payload)
    // = 21 + 5 = 26 bytes total, matching original `5 + 21`.
    let mut multi = Vec::with_capacity(26);
    let cell_a = AuCellHeader {
        metadata_service_id: 0,
        sequence_number: 100,
        cell_fragment_indication: CellFragmentIndication::Complete,
        decoder_config_flag: false,
        random_access_indicator: true,
    };
    let cell_b = AuCellHeader {
        sequence_number: 101,
        ..cell_a
    };
    let cell_c = AuCellHeader {
        sequence_number: 102,
        ..cell_a
    };
    let cell_d = AuCellHeader {
        sequence_number: 103,
        ..cell_a
    };
    write_metadata_au_cell(&mut multi, cell_a, b"AA").unwrap();
    write_metadata_au_cell(&mut multi, cell_b, b"BB").unwrap();
    write_metadata_au_cell(&mut multi, cell_c, b"CC").unwrap();
    write_metadata_au_cell(&mut multi, cell_d, b"").unwrap();
    assert_eq!(multi.len(), 26, "multi-cell payload must be 26 bytes");
    ts[off0..off0 + 26].copy_from_slice(&multi);

    let events = collect_events(&ts);
    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    // 4 Complete cells in one PES → 4 Metadata events.
    assert_eq!(
        metas.len(),
        4,
        "each Complete cell in a multi-cell PES emits its own Sample",
    );
    for (i, evt) in metas.iter().enumerate() {
        match evt {
            DemuxEvent::Metadata {
                kind:
                    MetadataKind::KlvSyncAuCell {
                        was_reassembled,
                        cell_count,
                        sequence_number,
                        ..
                    },
                ..
            } => {
                assert!(!was_reassembled, "Complete cell {i}: was_reassembled=false");
                assert_eq!(*cell_count, 1, "Complete cell {i}: cell_count=1");
                assert_eq!(
                    *sequence_number,
                    100 + i as u8,
                    "Complete cell {i}: sequence_number propagates",
                );
            }
            _ => unreachable!(),
        }
    }
}

// ── Test 4: orphan Middle → NonConformant, no Sample ────────────────────

#[test]
fn orphan_middle_emits_nonconformant_no_sample() {
    let mut mux = build_muxer();
    pump_video(&mut mux, 5, 90_000);
    let inner = b"XXX";
    mux.push_klv(inner, Pts90khz::new(90_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);
    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    // Patch to Middle (orphan — no prior First).
    patch_au_cell(&mut ts, off0, CellFragmentIndication::Middle, 5, inner);

    let events = collect_events(&ts);
    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    assert!(metas.is_empty(), "orphan Middle must NOT emit a Sample",);
    let nc: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::MultiCellAu {
                        pid,
                        dropped_bytes,
                        reason,
                    },
                ..
            } => Some((*pid, *dropped_bytes, *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(nc.len(), 1, "exactly one MultiCellAu NonConformant");
    assert_eq!(nc[0].0, KLV_PID);
    assert_eq!(nc[0].1, inner.len(), "dropped_bytes = orphan cell's inner");
    assert_eq!(nc[0].2, MultiCellAuReason::Orphan);
}

// ── Test 5: sequence gap mid-buffer → NonConformant, no Sample ──────────

#[test]
fn sequence_gap_emits_nonconformant_no_sample() {
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);
    let a = b"AAA";
    let b = b"BBB";
    mux.push_klv(a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(b, Pts90khz::new(91_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1 found");
    // First cell at seq=10; expected next = 11. Patch second to seq=13 → gap.
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, a);
    patch_au_cell(&mut ts, off1, CellFragmentIndication::Middle, 13, b);

    let events = collect_events(&ts);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Metadata { .. })),
        "no Sample emitted on sequence gap",
    );
    let mut saw_gap = false;
    for evt in &events {
        if let DemuxEvent::NonConformant {
            issue:
                NonConformantIssue::MultiCellAu {
                    pid,
                    dropped_bytes,
                    reason,
                },
            ..
        } = evt
        {
            assert_eq!(*pid, KLV_PID);
            // buffered (a.len()) + incoming (b.len()) = 3 + 3 = 6.
            assert_eq!(
                *dropped_bytes,
                a.len() + b.len(),
                "dropped_bytes = buffered + incoming",
            );
            assert_eq!(*reason, MultiCellAuReason::SequenceGap);
            saw_gap = true;
        }
    }
    assert!(saw_gap, "MultiCellAu SequenceGap NonConformant expected");
}

// ── Test 6: ConcurrentFirst then Last → NonConformant + Sample ──────────

#[test]
fn concurrent_first_emits_nonconformant_then_buffers_new() {
    let mut mux = build_muxer();
    pump_video(&mut mux, 12, 90_000);
    let a = b"AAA";
    let b = b"BBB";
    let c = b"CCC";
    mux.push_klv(a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(b, Pts90khz::new(91_000), 0x00).unwrap();
    mux.push_klv(c, Pts90khz::new(92_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1 found");
    let off2 = locate_nth_au_cell_offset(&ts, 2).expect("PUSI 2 found");
    // First A (seq 10) → buffer. New First B (seq 20) → ConcurrentFirst
    //   (drops A, re-buffers B). Last C (seq 21) → emit B+C, cell_count=2.
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, a);
    patch_au_cell(&mut ts, off1, CellFragmentIndication::First, 20, b);
    patch_au_cell(&mut ts, off2, CellFragmentIndication::Last, 21, c);

    let events = collect_events(&ts);

    // Expect exactly one ConcurrentFirst NonConformant and one Sample.
    let cf_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MultiCellAu {
                        reason: MultiCellAuReason::ConcurrentFirst,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(cf_count, 1, "exactly one ConcurrentFirst issue");

    // The dropped_bytes on the ConcurrentFirst event should equal A's
    // inner length (the buffered partial that got dropped).
    for evt in &events {
        if let DemuxEvent::NonConformant {
            issue:
                NonConformantIssue::MultiCellAu {
                    dropped_bytes,
                    reason: MultiCellAuReason::ConcurrentFirst,
                    ..
                },
            ..
        } = evt
        {
            assert_eq!(*dropped_bytes, a.len(), "dropped buffer = A's inner len");
        }
    }

    let metas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
        .collect();
    assert_eq!(metas.len(), 1, "exactly one Sample for the second AU (B+C)");
    match metas[0] {
        DemuxEvent::Metadata {
            kind:
                MetadataKind::KlvSyncAuCell {
                    was_reassembled,
                    cell_count,
                    sequence_number,
                    ..
                },
            payload,
            ..
        } => {
            assert!(*was_reassembled, "second AU is 2-cell reassembly");
            assert_eq!(*cell_count, 2, "B + C contributed");
            assert_eq!(*sequence_number, 20, "second AU's First seq surfaces");
            assert_eq!(payload.as_slice(), b"BBBCCC", "inner = B + C concatenated");
        }
        _ => unreachable!(),
    }
}

// ── Test 7: overflow → NonConformant, no Sample, dropped_bytes correct ──

#[test]
fn overflow_drops_buffer_emits_nonconformant() {
    // Lower au_cell_cap_per_pid via DemuxerConfigBuilder so the second
    // (Middle) cell overflows.
    let mut mux = build_muxer();
    pump_video(&mut mux, 8, 90_000);
    let a = vec![0xAAu8; 5];
    let b = vec![0xBBu8; 10];
    mux.push_klv(&a, Pts90khz::new(90_000), 0x00).unwrap();
    mux.push_klv(&b, Pts90khz::new(91_000), 0x00).unwrap();
    let mut ts = drain(&mut mux);

    let off0 = locate_nth_au_cell_offset(&ts, 0).expect("PUSI 0 found");
    let off1 = locate_nth_au_cell_offset(&ts, 1).expect("PUSI 1 found");
    patch_au_cell(&mut ts, off0, CellFragmentIndication::First, 10, &a);
    patch_au_cell(&mut ts, off1, CellFragmentIndication::Middle, 11, &b);

    let builder = DemuxerConfig::builder().au_cell_cap_per_pid(10); // 5 + 10 = 15 > 10
    let events = collect_events_with(builder, &ts);

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Metadata { .. })),
        "no Sample emitted on overflow",
    );

    let mut saw_overflow = false;
    for evt in &events {
        if let DemuxEvent::NonConformant {
            issue:
                NonConformantIssue::MultiCellAu {
                    pid,
                    dropped_bytes,
                    reason,
                },
            ..
        } = evt
        {
            assert_eq!(*pid, KLV_PID);
            assert_eq!(*reason, MultiCellAuReason::Overflow);
            // dropped_bytes = buffered (5) + incoming (10) = 15.
            assert_eq!(
                *dropped_bytes,
                a.len() + b.len(),
                "dropped_bytes = buffered + incoming",
            );
            saw_overflow = true;
        }
    }
    assert!(saw_overflow, "MultiCellAu Overflow NonConformant expected");
}

// ── Test 8: reset_sync mid-buffer clears state silently ─────────────────

#[test]
fn reset_sync_clears_buffer_no_nonconformant() {
    // Feed a First cell, then reset_sync (no NonConformant emitted),
    // then feed an orphan Middle from a fresh TS byte stream. The
    // orphan Middle should fire MultiCellAu(Orphan) — proving the
    // reset really cleared the prior First.
    let mut mux1 = build_muxer();
    pump_video(&mut mux1, 5, 90_000);
    let a = b"AAA";
    mux1.push_klv(a, Pts90khz::new(90_000), 0x00).unwrap();
    let mut ts1 = drain(&mut mux1);
    let off0 = locate_nth_au_cell_offset(&ts1, 0).expect("PUSI 0 found");
    patch_au_cell(&mut ts1, off0, CellFragmentIndication::First, 10, a);

    let mut dem = Demuxer::new();
    dem.feed(&ts1).unwrap();
    let mid_events: Vec<_> = std::iter::from_fn(|| dem.next_event()).collect();
    // Before reset, the First cell silently buffers — no Sample, no
    // NonConformant. (PAT/PMT ProgramMap events may fire.)
    assert!(
        !mid_events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Metadata { .. })),
        "First cell alone must not emit a Sample",
    );
    assert!(
        !mid_events.iter().any(|e| matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::MultiCellAu { .. },
                ..
            }
        )),
        "First-only buffering must not surface a MultiCellAu issue",
    );

    // Operational reset — clears AU reassembly buffers silently
    // (reset_sync calls au_reassembler.reset_all()).
    dem.reset_sync();
    let post_reset_events: Vec<_> = std::iter::from_fn(|| dem.next_event()).collect();
    assert!(
        post_reset_events.is_empty(),
        "reset_sync emits no events on its own",
    );

    // Feed a fresh stream containing only a Middle/Last cell. Because
    // reset_sync drops PAT/PMT/CC state too, we feed a NEW full stream
    // from a fresh muxer that opens with PAT+PMT. Without the prior
    // First (cleared by reset_sync), the Middle should be an Orphan.
    let mut mux2 = build_muxer();
    pump_video(&mut mux2, 5, 90_000);
    let b = b"BBB";
    mux2.push_klv(b, Pts90khz::new(90_000), 0x00).unwrap();
    let mut ts2 = drain(&mut mux2);
    let off0_2 = locate_nth_au_cell_offset(&ts2, 0).expect("PUSI 0 found");
    patch_au_cell(&mut ts2, off0_2, CellFragmentIndication::Middle, 50, b);

    dem.feed(&ts2).unwrap();
    let final_events: Vec<_> = std::iter::from_fn(|| dem.next_event()).collect();
    // No metadata sample — orphan continuation.
    assert!(
        !final_events
            .iter()
            .any(|e| matches!(e, DemuxEvent::Metadata { .. })),
        "orphan Middle must not emit Sample",
    );
    let saw_orphan = final_events.iter().any(|e| {
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
    });
    assert!(
        saw_orphan,
        "post-reset Middle is Orphan — reset_sync really cleared the buffer",
    );
}
