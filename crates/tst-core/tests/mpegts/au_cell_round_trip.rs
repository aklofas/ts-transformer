//! End-to-end round-trip of the H.222.0 §2.12.4.2 Metadata_AU_cell wrapper
//! via the muxer's auto-wrap path and the demuxer's classify_klv recognition.
//!
//! Replaces the obsolete tests/mpegts_mux_st1910.rs + tests/mpegts_demux_st1910.rs
//! which exercised the fictional UL+BER+PTSP wrapper format.

use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, read_metadata_au_cell};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn synthetic_klv_ls() -> Vec<u8> {
    let mut v = Vec::new();
    // 16-byte ST 0601 LS UL.
    v.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    // BER short-form length = 4.
    v.push(0x04);
    // 4 body bytes (just markers; no real ST 0601 content).
    v.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    v
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

#[test]
fn sync_klv_mux_demux_round_trip() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(
            0x1031,
            KlvStreamType::SynchronousMetadata,
            /*carries_pts=*/ true,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let inner = synthetic_klv_ls();
    mux.push_klv(&inner, Pts90khz::new(90_000), 0x00).unwrap();
    let ts_buf = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts_buf).unwrap();

    let mut sync_klv_seen = 0;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::Metadata {
            kind:
                MetadataKind::KlvSyncAuCell {
                    metadata_service_id,
                    sequence_number,
                    cell_fragment_indication,
                    decoder_config_flag,
                    random_access_indicator,
                    was_reassembled: _,
                    cell_count: _,
                },
            payload,
            pts,
            ..
        } = evt
        {
            assert_eq!(payload, inner, "inner KLV must round-trip byte-for-byte");
            assert_eq!(pts.as_ticks(), 90_000, "PES PTS must surface unchanged");
            // Mux defaults:
            assert_eq!(metadata_service_id, 0x00, "ST 1402.2 App. B default");
            assert_eq!(sequence_number, 0, "first push starts at seq 0");
            assert_eq!(
                cell_fragment_indication,
                CellFragmentIndication::Complete,
                "single-cell AU"
            );
            assert!(!decoder_config_flag, "we never carry decoder config");
            assert!(random_access_indicator, "ST 0601 records are entry points");
            sync_klv_seen += 1;
        }
    }
    assert_eq!(sync_klv_seen, 1, "exactly one sync KLV event expected");
}

#[test]
fn private_data_klv_does_not_auto_wrap() {
    // PrivateData streams are caller-controlled; the muxer must NOT prepend
    // an AU cell header. Push raw bytes; demuxer must surface them as-is
    // via MetadataKind::KlvAsync (the bare SMPTE UL signal).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(
            0x1031,
            KlvStreamType::PrivateData,
            /*carries_pts=*/ false,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let inner = synthetic_klv_ls();
    mux.push_klv(&inner, Pts90khz::new(0), 0x00).unwrap();
    let ts_buf = drain(&mut mux);

    let mut dem = Demuxer::new();
    dem.feed(&ts_buf).unwrap();

    let mut async_klv_seen = 0;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::Metadata {
            kind: MetadataKind::KlvAsync,
            payload,
            ..
        } = evt
        {
            assert_eq!(payload, inner);
            async_klv_seen += 1;
        }
    }
    assert_eq!(async_klv_seen, 1, "exactly one async KLV event expected");
}

#[test]
fn sync_klv_sequence_number_increments_across_pushes() {
    // Verifies the muxer's per-stream sequence_number counter increments
    // mod 256 across push_klv calls. Validates by reading the AU cell
    // header out of the emitted PES payload directly.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let inner = synthetic_klv_ls();

    let next_seq_num = |mux: &mut Muxer| {
        let mut pes_payload = Vec::new();
        let mut buf = vec![0u8; 1316];
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            for pkt in buf[..n].chunks_exact(188) {
                let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
                if pid != 0x1031 {
                    continue;
                }
                let payload_unit_start = (pkt[1] & 0x40) != 0;
                let adaptation_present = (pkt[3] & 0x20) != 0;
                let mut idx = 4usize;
                if adaptation_present {
                    let af_len = pkt[idx] as usize;
                    idx += 1 + af_len;
                }
                if payload_unit_start && idx + 9 <= 188 {
                    let pes_header_data_length = pkt[idx + 8] as usize;
                    idx += 9 + pes_header_data_length;
                }
                if idx < 188 {
                    pes_payload.extend_from_slice(&pkt[idx..188]);
                }
            }
        }
        let (hdr, _) = read_metadata_au_cell(&pes_payload).unwrap();
        hdr.sequence_number
    };

    mux.push_klv(&inner, Pts90khz::new(90_000), 0x00).unwrap();
    assert_eq!(next_seq_num(&mut mux), 0);

    mux.push_klv(&inner, Pts90khz::new(90_000 * 2), 0x00)
        .unwrap();
    assert_eq!(next_seq_num(&mut mux), 1);

    mux.push_klv(&inner, Pts90khz::new(90_000 * 3), 0x00)
        .unwrap();
    assert_eq!(next_seq_num(&mut mux), 2);
}

// ── MultiCellAu detect-only tests ────────────────────────────────────────────

/// Unit test: classify_klv returns Sync regardless of CFI; iter_au_cells
/// surfaces the per-cell CFI for downstream handling: `classify_klv` is a
/// SHAPE sniff, and per-cell field access lives in `iter_au_cells`.
#[test]
fn classify_klv_returns_sync_on_any_cfi_with_iter_surfacing_cfi() {
    use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
    use tst_core::mpegts::demux::low_level::{KlvShape, classify_klv};
    use tst_core::mpegts::demux::payload_test_hooks::iter_au_cells;

    for cfi in [
        CellFragmentIndication::First,
        CellFragmentIndication::Middle,
        CellFragmentIndication::Last,
    ] {
        let mut bytes = Vec::new();
        write_metadata_au_cell(
            &mut bytes,
            AuCellHeader {
                metadata_service_id: 0x00,
                sequence_number: 0,
                cell_fragment_indication: cfi,
                decoder_config_flag: false,
                random_access_indicator: false,
            },
            &[0xAA; 100],
        )
        .unwrap();

        assert_eq!(
            classify_klv(&bytes),
            KlvShape::Sync,
            "any valid AU cell header → KlvShape::Sync (CFI {cfi:?})"
        );

        // iter_au_cells surfaces the per-cell CFI + inner body.
        let cells: Vec<_> = iter_au_cells(&bytes).collect();
        assert_eq!(cells.len(), 1);
        let (header, inner) = cells[0].as_ref().unwrap();
        assert_eq!(header.cell_fragment_indication, cfi);
        assert_eq!(
            inner.len(),
            100,
            "inner length must match declared payload for CFI {cfi:?}"
        );
    }
}

/// Complete CFI returns KlvShape::Sync — iter_au_cells recovers the
/// inner KLV bytes verbatim.
#[test]
fn classify_klv_complete_cfi_returns_sync_and_iter_recovers_inner() {
    use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
    use tst_core::mpegts::demux::low_level::{KlvShape, classify_klv};
    use tst_core::mpegts::demux::payload_test_hooks::iter_au_cells;

    // Build a Complete AU cell whose inner payload starts with the SMPTE UL.
    let inner_klv: Vec<u8> = {
        let mut v = Vec::new();
        v.extend_from_slice(&[
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ]);
        v.push(0x01);
        v.push(0xAB);
        v
    };
    let mut bytes = Vec::new();
    write_metadata_au_cell(
        &mut bytes,
        AuCellHeader {
            metadata_service_id: 0x00,
            sequence_number: 0,
            cell_fragment_indication: CellFragmentIndication::Complete,
            decoder_config_flag: false,
            random_access_indicator: false,
        },
        &inner_klv,
    )
    .unwrap();

    assert_eq!(classify_klv(&bytes), KlvShape::Sync);

    let cells: Vec<_> = iter_au_cells(&bytes).collect();
    assert_eq!(cells.len(), 1);
    let (_, inner_recovered) = cells[0].as_ref().unwrap();
    assert_eq!(*inner_recovered, &inner_klv[..]);
}

/// Opaque-inner AU cells surface as KlvShape::Sync.
///
/// Requiring `inner[0..4] == [0x06, 0x0E, 0x2B, 0x34]` (the SMPTE UL header)
/// before treating a cell as sync would misclassify legitimate non-LS sync
/// metadata (proprietary metadata payloads wrapped in an H.222.0 AU cell per
/// §2.12.4.2 — receiver classification of the inner is the consumer's
/// concern, not the demuxer's) as Other and lose the wrapper info
/// (sequence_number, service_id, RAI).
#[test]
fn classify_klv_opaque_inner_complete_cfi_returns_sync() {
    use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
    use tst_core::mpegts::demux::low_level::{KlvShape, classify_klv};
    use tst_core::mpegts::demux::payload_test_hooks::iter_au_cells;

    // Inner that does NOT start with the SMPTE UL header — opaque metadata.
    let opaque_inner = vec![0x55u8; 80];
    let mut bytes = Vec::new();
    write_metadata_au_cell(
        &mut bytes,
        AuCellHeader {
            metadata_service_id: 0x42,
            sequence_number: 7,
            cell_fragment_indication: CellFragmentIndication::Complete,
            decoder_config_flag: false,
            random_access_indicator: true,
        },
        &opaque_inner,
    )
    .unwrap();

    assert_eq!(classify_klv(&bytes), KlvShape::Sync);

    let cells: Vec<_> = iter_au_cells(&bytes).collect();
    assert_eq!(cells.len(), 1);
    let (header, klv_inner) = cells[0].as_ref().unwrap();
    // Inner is surfaced verbatim — the demuxer doesn't validate
    // KLV-LS shape on it.
    assert_eq!(*klv_inner, &opaque_inner[..]);
    // Wrapper fields preserved.
    assert_eq!(header.metadata_service_id, 0x42);
    assert_eq!(header.sequence_number, 7);
    assert!(header.random_access_indicator);
}

/// Integration test: MultiCellAu NonConformantIssue surfaces through the
/// demuxer when a sync KLV PES carries an orphan continuation cell.
///
/// Approach: mux a normal sync KLV stream so we get a valid PAT + PMT +
/// PES on PID 0x1031. Then locate the AU cell flags byte in the emitted
/// TS bytes and patch the CFI bits to "Last" (0b01 in the two MSBs of
/// the flags byte). With no prior `First` buffered for this PID, the
/// reassembler reports `MultiCellAuReason::Orphan` and the demuxer
/// surfaces a `MultiCellAu` NonConformantIssue.
///
/// Non-Complete CFI (First/Middle/Last) is reassembled, so we test
/// the orphan case (Last with empty state) rather than First — a First
/// cell with no continuation simply buffers and emits nothing.
///
/// AU cell flags byte layout (H.222.0 V9 §2.12.4.2 Table 2-156):
///   [7:6] cell_fragment_indication  (Complete = 0b11)
///   [5]   decoder_config_flag
///   [4]   random_access_indicator
///   [3:0] reserved (all 1s in auto-wrap output)
///
/// The muxer writes CFI=Complete (0b11) with dcf=0 and rai=1 and reserved=0b1111
/// → flags byte = 0b11_0_1_1111 = 0xDF.
/// Patching to CFI=Last (0b01): 0b01_0_1_1111 = 0x5F.
#[test]
fn multi_cell_au_emits_non_conformant_issue_through_demuxer() {
    use tst_core::mpegts::demux::{DemuxEvent, NonConformantIssue};
    use tst_core::mpegts::mux::{KlvStreamType, Muxer, MuxerConfig, VideoCodec};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let inner = synthetic_klv_ls();
    mux.push_klv(&inner, Pts90khz::new(90_000), 0x00).unwrap();
    let mut ts_bytes = drain(&mut mux);

    // Find the AU cell flags byte in the emitted TS stream. The muxer
    // emits Complete (0xDF) at a fixed offset inside the PES payload.
    // Walk TS packets on PID 0x1031, find the PUSI packet, skip the PES
    // header, then patch byte 2 of the AU cell (the flags byte).
    let klv_pid: u16 = 0x1031;
    let mut patched = false;
    'outer: for pkt in ts_bytes.chunks_mut(188) {
        let pkt_pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pkt_pid != klv_pid {
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
        // PES header: start_code(3) + stream_id(1) + length(2) + flags1(1) +
        //             flags2(1) + header_data_length(1) + optional fields
        if idx + 9 > 188 {
            continue;
        }
        let pes_header_data_length = pkt[idx + 8] as usize;
        idx += 9 + pes_header_data_length;
        // AU cell layout: [0] metadata_service_id, [1] sequence_number,
        //                 [2] flags byte, [3..4] AU_cell_data_length
        let flags_offset = idx + 2;
        if flags_offset < 188 {
            // CFI Complete (0b11) lives in bits [7:6]. Patch to Last
            // (0b01) — orphan continuation with no prior First in the
            // reassembler triggers MultiCellAuReason::Orphan.
            pkt[flags_offset] = (pkt[flags_offset] & 0x3F) | 0x40;
            patched = true;
            break 'outer;
        }
    }
    assert!(patched, "AU cell flags byte not found in emitted TS");

    // Default is now `cfi_tolerance: true`. This test exercises the
    // spec-strict orphan path, so opt out explicitly. The muxer's inner
    // payload is a valid KLV record, so under tolerance the orphan Last
    // would otherwise be rescued + emit `CfiTolerated` instead.
    use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
    let mut dem = Demuxer::with_config(DemuxerConfig::builder().cfi_tolerance(false).build());
    dem.feed(&ts_bytes).unwrap();

    let mut multi_cell_seen = false;
    while let Some(evt) = dem.next_event() {
        if let DemuxEvent::NonConformant {
            issue:
                NonConformantIssue::MultiCellAu {
                    pid,
                    dropped_bytes,
                    reason: _,
                },
            ..
        } = evt
        {
            assert_eq!(pid, klv_pid, "pid must match KLV PID");
            assert!(dropped_bytes > 0, "dropped_bytes must be > 0");
            multi_cell_seen = true;
        }
    }
    assert!(
        multi_cell_seen,
        "MultiCellAu NonConformantIssue must surface for orphan Last cell"
    );
}

// ── extract_pes_payload helper ────────────────────────────────────────────────
//
// Walk the TS byte stream, find the PUSI (payload_unit_start_indicator) packet
// on `pid`, skip the TS header + adaptation field + PES header, and return the
// raw PES payload bytes.
//
// PES header layout for a KLV sync stream (PTS-only carry):
//   start_code(3) + stream_id(1) + packet_length(2) = 6 bytes fixed prefix
//   flags1(1) + flags2(1) + header_data_length(1) = 3 bytes
//   PTS field(5 bytes) when flags2 bit 7 is set
//   → 14-byte total PES header when PTS is present (no DTS)
//
// For the no-PTS case (flags2 bit 7 clear), header_data_length = 0, so
// the header is only 9 bytes.
fn extract_pes_payload(ts_bytes: &[u8], pid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut found_pusi = false;

    for pkt in ts_bytes.chunks_exact(188) {
        let pkt_pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if pkt_pid != pid {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        let af_present = (pkt[3] & 0x20) != 0;
        let payload_present = (pkt[3] & 0x10) != 0;
        if !payload_present {
            continue;
        }

        let mut idx = 4usize;
        if af_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }

        if pusi {
            // PES header: 3-byte start_code + 1-byte stream_id + 2-byte length
            // + 1-byte flags1 + 1-byte flags2 + 1-byte header_data_length
            // + header_data_length bytes of optional fields (PTS etc.)
            if idx + 9 > 188 {
                continue;
            }
            let pes_header_data_length = pkt[idx + 8] as usize;
            idx += 9 + pes_header_data_length;
            found_pusi = true;
        }

        if found_pusi && idx < 188 {
            payload.extend_from_slice(&pkt[idx..188]);
        }
    }
    payload
}

#[test]
fn metadata_service_id_propagates_from_push_klv_to_au_cell() {
    // Verifies that the `metadata_service_id` parameter passed to
    // `Muxer::push_klv` flows all the way through to the 5-byte AU cell
    // header in the emitted PES payload, rather than being silently
    // overwritten with the former hardcoded 0x00 default.
    use tst_core::mpegts::au_cell::read_metadata_au_cell;
    use tst_core::mpegts::mux::{KlvStreamType, Muxer, MuxerConfig, VideoCodec};

    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        // SynchronousMetadata (stream_type 0x15) triggers the AU cell wrap;
        // carries_pts=true so the PES header has a 5-byte PTS field.
        prog.add_klv(
            0x102,
            KlvStreamType::SynchronousMetadata,
            /*carries_pts=*/ true,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Minimal H.222.0-shaped raw KLV LS bytes (16-byte ST 0601 UL + BER
    // short-form length=1 + 1 payload byte). The AU cell wrap in push_klv_to
    // doesn't validate the inner bytes; any non-empty slice works.
    let raw_klv: Vec<u8> = {
        let mut v = Vec::new();
        v.extend_from_slice(&[
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ]);
        v.push(0x01); // BER length = 1
        v.push(0xAB); // one payload byte
        v
    };

    // Push enough video frames to advance PTS beyond the PSI threshold
    // (~100 ms = 9000 ticks), so packets make it into the TS output.
    for i in 0..5i64 {
        let pts = 90_000 + i * 3_000;
        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10]; // AUD NAL
        mux.push_video(&nal, Pts90khz::new(pts), false).unwrap();
    }

    // Use a non-zero service_id (0x42) so the test can distinguish between
    // "value was plumbed" and "value was coincidentally 0x00 from the old
    // hardcoded default".
    let service_id: u8 = 0x42;
    mux.push_klv(&raw_klv, Pts90khz::new(9000), service_id)
        .unwrap();

    // Drain TS bytes.
    let ts_buf = drain(&mut mux);

    // Extract PES payload from the KLV PID 0x102.
    let pes_payload = extract_pes_payload(&ts_buf, 0x102);
    assert!(
        !pes_payload.is_empty(),
        "expected at least one TS packet on KLV PID 0x102"
    );

    // The first 5 bytes of the PES payload are the AU cell header.
    let (header, inner) = read_metadata_au_cell(&pes_payload).expect("AU cell must parse");

    // Primary assertion: service_id round-trips.
    assert_eq!(
        header.metadata_service_id, service_id,
        "caller-supplied service_id 0x{service_id:02X} must propagate to the AU cell header"
    );

    // Sanity check: the inner bytes are our raw KLV.
    assert_eq!(inner, &raw_klv[..], "inner KLV must pass through verbatim");
}

// ── iter_au_cells tests ───────────────────────────────────────────────────────

/// Helper: synthesize a PES payload containing N back-to-back AU cells.
fn make_pes_payload(cells: &[(AuCellHeader, &[u8])]) -> Vec<u8> {
    use tst_core::mpegts::au_cell::write_metadata_au_cell;
    let mut out = Vec::new();
    for (hdr, body) in cells {
        write_metadata_au_cell(&mut out, *hdr, body).unwrap();
    }
    out
}

fn cell_header(cfi: tst_core::mpegts::au_cell::CellFragmentIndication, seq: u8) -> AuCellHeader {
    AuCellHeader {
        metadata_service_id: 0,
        sequence_number: seq,
        cell_fragment_indication: cfi,
        decoder_config_flag: false,
        random_access_indicator: true,
    }
}

#[test]
fn iter_au_cells_walks_three_back_to_back_complete_cells() {
    let payload = make_pes_payload(&[
        (
            cell_header(CellFragmentIndication::Complete, 1),
            &[0xAA, 0xBB],
        ),
        (
            cell_header(CellFragmentIndication::Complete, 2),
            &[0xCC, 0xDD, 0xEE],
        ),
        (cell_header(CellFragmentIndication::Complete, 3), &[0xFF]),
    ]);
    let cells: Vec<_> =
        tst_core::mpegts::demux::payload_test_hooks::iter_au_cells(&payload).collect();
    assert_eq!(cells.len(), 3);
    let (h1, body1) = cells[0].as_ref().unwrap();
    assert_eq!(h1.sequence_number, 1);
    assert_eq!(*body1, &[0xAA, 0xBB][..]);
    let (h2, body2) = cells[1].as_ref().unwrap();
    assert_eq!(h2.sequence_number, 2);
    assert_eq!(*body2, &[0xCC, 0xDD, 0xEE][..]);
    let (h3, body3) = cells[2].as_ref().unwrap();
    assert_eq!(h3.sequence_number, 3);
    assert_eq!(*body3, &[0xFF][..]);
}

#[test]
fn iter_au_cells_walks_first_middle_last_in_one_pes() {
    let payload = make_pes_payload(&[
        (cell_header(CellFragmentIndication::First, 10), &[0x11; 100]),
        (
            cell_header(CellFragmentIndication::Middle, 11),
            &[0x22; 100],
        ),
        (cell_header(CellFragmentIndication::Last, 12), &[0x33; 50]),
    ]);
    let cells: Vec<_> = tst_core::mpegts::demux::payload_test_hooks::iter_au_cells(&payload)
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(cells.len(), 3);
    assert_eq!(
        cells[0].0.cell_fragment_indication,
        CellFragmentIndication::First
    );
    assert_eq!(
        cells[1].0.cell_fragment_indication,
        CellFragmentIndication::Middle
    );
    assert_eq!(
        cells[2].0.cell_fragment_indication,
        CellFragmentIndication::Last
    );
}

#[test]
fn iter_au_cells_empty_payload_yields_nothing() {
    let cells: Vec<_> = tst_core::mpegts::demux::payload_test_hooks::iter_au_cells(&[]).collect();
    assert!(cells.is_empty());
}

#[test]
fn iter_au_cells_stops_at_truncated_trailing_cell() {
    let mut payload = make_pes_payload(&[(
        cell_header(CellFragmentIndication::Complete, 1),
        &[0xAA, 0xBB],
    )]);
    payload.extend_from_slice(&[0x00, 0x02, 0xDF]);
    let cells: Vec<_> =
        tst_core::mpegts::demux::payload_test_hooks::iter_au_cells(&payload).collect();
    assert_eq!(cells.len(), 2);
    assert!(cells[0].is_ok());
    assert!(cells[1].is_err());
}

#[test]
fn iter_au_cells_stops_at_declared_length_overrun() {
    let mut payload = vec![0x00, 0x00, 0xDF, 0x00, 0x64];
    payload.extend_from_slice(&[0xAA; 10]);
    let cells: Vec<_> =
        tst_core::mpegts::demux::payload_test_hooks::iter_au_cells(&payload).collect();
    assert_eq!(cells.len(), 1);
    assert!(cells[0].is_err());
}
