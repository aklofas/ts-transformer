//! Strict-mode integration tests for `mpegts::demux::Demuxer`.
//!
//! `StrictMode` converts selected `NonConformantIssue` categories into a
//! fatal `DemuxError::StrictRejection` returned out of `Demuxer::feed`,
//! instead of just queuing a `NonConformant` event for the caller to
//! inspect. These tests cover both directions:
//!
//! * `StrictMode::Full` — every issue category rejects.
//! * `StrictMode::Off` (default) — nothing rejects; the loop survives.
//!
//! The natural shape used here is "async-typed KLV PID carrying sync-shaped
//! AU-cell-wrapped payload": configure `KlvStreamType::PrivateData` (PMT
//! stream_type 0x06; passes payload through unchanged) and push pre-wrapped
//! bytes that form a valid H.222.0 §2.12.4.2 Metadata_AU_cell. The demuxer's
//! `classify_klv` returns `KlvShape::SyncAuCell`, the PMT declared the PID
//! as async, and the linkage builder emits
//! `NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid`.
//!
//! Why this direction (sync-on-async) and not the other (async-on-sync):
//! `KlvStreamType::SynchronousMetadata` triggers automatic AU-cell wrapping
//! in `Muxer::push_klv_to`, so the wire form on a sync-declared PID always
//! matches the declaration — there's no in-API way to produce async-on-sync
//! mismatches anymore. PrivateData streams pass payload through unchanged,
//! so we can hand the muxer pre-wrapped sync-shaped bytes to drive the
//! sync-on-async mismatch path.
//!
//! Note: a 1-video + 1-KLV PMT does NOT trigger
//! `MissingMetadataDescriptor`. With a single video PID in the PMT, the
//! demuxer's linkage builder falls into the "infer from topology" arm
//! (`LinkSource::Inferred`) instead of the "no entry" arm. Triggering
//! `MissingMetadataDescriptor` requires a multi-video PMT with no metadata
//! descriptor; the demuxer's PSI-topology unit tests cover that arm.

use crate::common::mux_demux::drain_mux as drain;
use crate::psi_builders::pack_section;
use tst_core::error::DemuxError;
use tst_core::mpegts::au_cell::{AuCellHeader, CellFragmentIndication, write_metadata_au_cell};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::common::crc32::crc32_mpeg2;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, StrictMode};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

/// A minimally well-formed bare ST 0601 LS used as the inner payload of a
/// synthetic AU cell: 16-byte UAS Datalink LS UL + short-form BER length 0.
const BARE_KLV_LS: [u8; 17] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00,
];

/// Build a TS byte stream that triggers `StreamTypeMismatchSyncOnAsyncPid`:
/// PMT declares the KLV PID as async (stream_type 0x06, PrivateData) but
/// the PES payload is a sync-shaped AU-cell-wrapped KLV. Returns the muxed
/// bytes. PrivateData streams pass payload through unchanged, so the caller
/// can directly emit a sync-shaped wire form.
fn build_mismatched_stream() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    m.push_video(
        &[0x00, 0x00, 0x00, 0x01, 0x09, 0x10],
        Pts90khz::new(0),
        true,
    )
    .unwrap();
    // Pre-wrap a synthetic Metadata_AU_cell carrying a bare ST 0601 LS, then
    // push as PrivateData so the muxer passes it through as-is. Wire form
    // is `KlvShape::SyncAuCell` while PMT says async (stream_type 0x06) —
    // the mismatch the demuxer surfaces as StreamTypeMismatchSyncOnAsyncPid.
    let mut wrapped = Vec::new();
    let header = AuCellHeader {
        metadata_service_id: 0x00,
        sequence_number: 0,
        cell_fragment_indication: CellFragmentIndication::Complete,
        decoder_config_flag: false,
        random_access_indicator: true,
    };
    write_metadata_au_cell(&mut wrapped, header, &BARE_KLV_LS).unwrap();
    m.push_klv(&wrapped, Pts90khz::new(0), 0x00).unwrap();
    drain(&mut m)
}

#[test]
fn strict_full_rejects_stream_type_mismatch() {
    let bytes = build_mismatched_stream();
    let mut d = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    let res = d.feed(&bytes);
    // The implementation queues the `NonConformant` event first, then
    // drains `fatal` at end of the packet loop and returns
    // `Err(StrictRejection)`. Both halves must hold: feed errors AND the
    // structured issue is retrievable from the queue after the error.
    let mut saw_mismatch = false;
    while let Some(ev) = d.next_event() {
        if matches!(
            ev,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid,
                ..
            }
        ) {
            saw_mismatch = true;
        }
    }
    assert!(
        matches!(res, Err(DemuxError::StrictRejection(_))),
        "strict-Full feed should return Err(StrictRejection), got: {:?}",
        res
    );
    assert!(
        saw_mismatch,
        "strict-Full feed should queue StreamTypeMismatchSyncOnAsyncPid event before erroring"
    );
}

#[test]
fn strict_off_emits_event_keeps_running() {
    let bytes = build_mismatched_stream();
    let mut d = Demuxer::with_config(DemuxerConfig::builder().build()); // default = StrictMode::Off
    d.feed(&bytes).unwrap(); // should not error in StrictMode::Off

    // The lenient contract has two halves: feed returns Ok AND the
    // non-conformance is surfaced as an event. Asserting both prevents a
    // regression where the demuxer silently swallows issues.
    let mut saw_mismatch = false;
    while let Some(ev) = d.next_event() {
        if matches!(
            ev,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid,
                ..
            }
        ) {
            saw_mismatch = true;
        }
    }
    assert!(
        saw_mismatch,
        "lenient mode should queue StreamTypeMismatchSyncOnAsyncPid event (and keep running)"
    );
}

// ── PSI CC discontinuity: strict vs lenient ────────────────────────────────

/// Build a raw PMT section large enough to span ≥3 TS packets (368+ bytes).
///
/// Layout: standard PMT header for program 1 / PCR_PID 0x0100 / one video
/// stream at 0x0100 with a large padded private descriptor (tag=0xFF,
/// length=350 bytes of 0x00). The ES descriptor loop makes the section body
/// well over 368 bytes, forcing the demuxer to reassemble across 3 packets.
///
/// Returns the raw section bytes (table_id through CRC32 inclusive), ready to
/// be sliced into TS packets by `psi_builders::pack_section`.
fn build_pmt_section_at_least_3_ts_packets() -> Vec<u8> {
    // Descriptor payload: tag=0xFF, length=0xDE (222 bytes) -- repeated twice
    // gives descriptor loop length = 2 * (2 + 222) = 448 bytes.
    // PMT section body = 9 (fixed header) + 0 (no prog descriptors)
    //                  + 5 (ES entry header) + 448 (ES descriptors)
    //                  + 4 (CRC) = 466 bytes.
    // Total section = 3 + section_length, section_length = 466 - 3 = 463.
    // 3 + 463 = 466 > 368 → requires ≥3 TS packets. ✓
    let desc_payload_len: usize = 222;
    let one_desc_len: usize = 2 + desc_payload_len; // tag + length + payload
    let num_descs: usize = 2;
    let es_info_length: usize = num_descs * one_desc_len; // 448

    // section_length = 9 + 0 + 5 + es_info_length + 4 (CRC) = 466 → 463
    let section_length: u16 = (9 + 5 + es_info_length + 4) as u16;

    let mut sec: Vec<u8> = Vec::with_capacity(3 + section_length as usize);

    // table_id = 0x02 (PMT)
    sec.push(0x02);
    // section_syntax_indicator=1 + '0' + reserved=0b11 + section_length (12 bits)
    sec.push(0xB0 | (((section_length >> 8) & 0x0F) as u8));
    sec.push((section_length & 0xFF) as u8);
    // program_number = 1
    sec.push(0x00);
    sec.push(0x01);
    // reserved(2) + version_number(5) + current_next_indicator(1) = 0xC1
    sec.push(0xC1);
    // section_number = 0
    sec.push(0x00);
    // last_section_number = 0
    sec.push(0x00);
    // reserved(3) + PCR_PID = 0x0100
    sec.push(0xE0 | 0x01);
    sec.push(0x00);
    // reserved(4) + program_info_length = 0 (no program-level descriptors)
    sec.push(0xF0);
    sec.push(0x00);

    // ES entry: stream_type=0x1B (H.264), PID=0x0100, ES_info_length=es_info_length
    sec.push(0x1B); // stream_type
    sec.push(0xE0 | 0x01); // reserved + PID high
    sec.push(0x00); // PID low
    sec.push(0xF0 | ((es_info_length >> 8) as u8 & 0x0F)); // reserved + ES_info_length high
    sec.push((es_info_length & 0xFF) as u8); // ES_info_length low

    // Descriptor loop: num_descs × private descriptor (tag=0xFF, length=222)
    for _ in 0..num_descs {
        sec.push(0xFF); // descriptor_tag (private)
        sec.push(desc_payload_len as u8); // descriptor_length
        sec.extend(std::iter::repeat_n(0x00, desc_payload_len));
    }

    // CRC32 over the section body (table_id through last descriptor byte).
    let crc = crc32_mpeg2(&sec);
    sec.push((crc >> 24) as u8);
    sec.push((crc >> 16) as u8);
    sec.push((crc >> 8) as u8);
    sec.push(crc as u8);

    sec
}

/// Build a single 188-byte TS packet carrying a PAT that references `pmt_pid`
/// as the PMT for program 1. PUSI=1, PID=0, CC=0.
fn build_pat_packet(pmt_pid: u16) -> [u8; 188] {
    // PAT section body (without the 4-byte CRC yet):
    //   table_id(1)=0x00 + section_syntax+length(2) + tsid(2) + ver/cni(1)
    //   + sect(1) + last_sect(1) + program_number(2) + reserved+pmt_pid(2)
    // section_length = 5 (fixed header) + 4 (one program entry) + 4 (CRC) = 13
    let section_length: u16 = 13;
    let mut sec: Vec<u8> = Vec::with_capacity(16);
    sec.push(0x00); // table_id = PAT
    sec.push(0xB0 | (((section_length >> 8) & 0x0F) as u8));
    sec.push((section_length & 0xFF) as u8);
    sec.push(0x00); // transport_stream_id high
    sec.push(0x01); // transport_stream_id low
    sec.push(0xC1); // reserved + version_number=0 + current_next_indicator=1
    sec.push(0x00); // section_number
    sec.push(0x00); // last_section_number
    // Program 1 entry:
    sec.push(0x00); // program_number high
    sec.push(0x01); // program_number low
    sec.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F)); // reserved + pmt_pid high
    sec.push((pmt_pid & 0xFF) as u8); // pmt_pid low
    let crc = crc32_mpeg2(&sec);
    sec.push((crc >> 24) as u8);
    sec.push((crc >> 16) as u8);
    sec.push((crc >> 8) as u8);
    sec.push(crc as u8);

    // Pack into a 188-byte TS packet: PUSI=1, PID=0, CC=0, pointer_field=0x00.
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40; // PUSI=1, TEI=0, priority=0, PID high=0
    pkt[2] = 0x00; // PID low
    pkt[3] = 0x10; // payload-only, CC=0
    pkt[4] = 0x00; // pointer_field
    let chunk = sec.len().min(183);
    pkt[5..5 + chunk].copy_from_slice(&sec[..chunk]);
    // Rest stays 0xFF stuffing.
    pkt
}

/// Feed a synthetic PAT packet into the demuxer so it seeds the PMT PID.
fn install_synthetic_pat_pointing_to(demux: &mut tst_core::mpegts::demux::Demuxer, pmt_pid: u16) {
    let pat = build_pat_packet(pmt_pid);
    demux.feed(&pat).unwrap();
    // Drain any queued events (ProgramMap is not emitted yet — that waits for PMT).
    while demux.next_event().is_some() {}
}

/// Drain all queued events and return those that are `NonConformant` issues.
fn drain_non_conformant_issues(
    demux: &mut tst_core::mpegts::demux::Demuxer,
) -> Vec<NonConformantIssue> {
    let mut issues = Vec::new();
    while let Some(ev) = demux.next_event() {
        if let DemuxEvent::NonConformant { issue, .. } = ev {
            issues.push(issue);
        }
    }
    issues
}

#[test]
fn split_pmt_with_dropped_continuation_drops_section_in_strict_mode() {
    // Build a 3-TS-packet PMT (section large enough to require 3 packets) and
    // feed packets 1 + 3 (drop packet 2). Strict mode (lenient_psi_reassembly=false,
    // the default) must:
    //   1. emit NonConformantIssue::PsiCcDiscontinuity on packet 3
    //   2. NOT emit a parsed PMT (ProgramMap) event — partial section was dropped
    let pmt_bytes = build_pmt_section_at_least_3_ts_packets();
    let pmt_pid: u16 = 0x0FFF;
    let pkts = pack_section(pmt_pid, &pmt_bytes, 0x0);
    assert!(
        pkts.len() >= 3,
        "section must span ≥3 packets, got {}",
        pkts.len()
    );

    let mut demux = tst_core::mpegts::demux::Demuxer::with_config(DemuxerConfig::default());
    install_synthetic_pat_pointing_to(&mut demux, pmt_pid);

    demux.feed(&pkts[0]).unwrap();
    // Skip pkts[1] — simulates a dropped TS packet in the middle.
    demux.feed(&pkts[2]).unwrap();

    // One drain: both checks read the same events, so neither can be satisfied
    // by a queue an earlier drain already emptied.
    let events: Vec<DemuxEvent> = std::iter::from_fn(|| demux.next_event()).collect();
    assert!(
        events.iter().any(|e| matches!(
            e,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PsiCcDiscontinuity { pid: 0x0FFF, .. },
                ..
            }
        )),
        "expected PsiCcDiscontinuity on PMT PID 0x{pmt_pid:04X}, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, DemuxEvent::ProgramMap(_))),
        "PMT section must NOT parse — partial reassembly was dropped on CC jump, got {events:?}"
    );
}

#[test]
fn split_pmt_with_dropped_continuation_keeps_section_in_lenient_mode() {
    // Same packet sequence as the strict-mode test, but with lenient_psi_reassembly=true.
    // Lenient mode must NOT emit PsiCcDiscontinuity — it feeds the bytes through
    // and lets the section either pass CRC by luck or surface as PsiChecksumMismatch.
    let pmt_bytes = build_pmt_section_at_least_3_ts_packets();
    let pmt_pid: u16 = 0x0FFF;
    let pkts = pack_section(pmt_pid, &pmt_bytes, 0x0);

    let mut demux = tst_core::mpegts::demux::Demuxer::with_config({
        let mut cfg = DemuxerConfig::default();
        cfg.lenient_psi_reassembly = true;
        cfg
    });
    install_synthetic_pat_pointing_to(&mut demux, pmt_pid);

    demux.feed(&pkts[0]).unwrap();
    demux.feed(&pkts[2]).unwrap();

    let issues = drain_non_conformant_issues(&mut demux);
    assert!(
        !issues
            .iter()
            .any(|i| matches!(i, NonConformantIssue::PsiCcDiscontinuity { .. })),
        "lenient mode must not emit PsiCcDiscontinuity, got {issues:?}"
    );
    // No assertion about PMT parse outcome — lenient mode either passes by luck
    // (bytes happen to form valid section data) or surfaces PsiChecksumMismatch;
    // both are legitimate outcomes.
}
