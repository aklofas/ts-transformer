//! Hand-built PSI sections and single-packet TS wrapping for the demuxer's
//! unit tests (ISO/IEC 13818-1 §2.4.4). Fields are masked to their widths,
//! never range-checked, so tests may build deliberately odd sections. The
//! integration-test twin is `tests/mpegts/psi_builders.rs`.

use crate::error::DemuxError;
use crate::mpegts::demux::demuxer::Demuxer;
use crate::mpegts::demux::event::{DemuxEvent, NonConformantIssue};
use crate::mpegts::demux::types::DemuxerConfig;
use alloc::vec;
use alloc::vec::Vec;

use crate::mpegts::common::crc32::crc32_mpeg2;

/// PAT section (table_id 0x00); `programs` is `(program_number, pmt_pid)`.
pub(crate) fn build_pat_section(
    transport_stream_id: u16,
    version: u8,
    programs: &[(u16, u16)],
) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x00); // table_id = PAT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F)); // ssi=1, '0', reserved, length hi
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&transport_stream_id.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1)); // reserved | version | current_next=1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    for &(pn, pid) in programs {
        s.extend_from_slice(&pn.to_be_bytes());
        s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        s.push((pid & 0xFF) as u8);
    }
    append_crc(&mut s);
    s
}

/// PMT section (table_id 0x02). `program_info` is the raw program
/// descriptor loop; `streams` is `(stream_type, elementary_pid, es_info)`
/// with `es_info` the raw ES descriptor loop.
pub(crate) fn build_pmt_section(
    program_number: u16,
    pcr_pid: u16,
    version: u8,
    program_info: &[u8],
    streams: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let mut s = vec![0x02, 0xB0, 0x00]; // table_id = PMT; length backfilled below
    s.extend_from_slice(&program_number.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    s.push((pcr_pid & 0xFF) as u8);
    s.push(0xF0 | ((program_info.len() >> 8) as u8 & 0x0F));
    s.push((program_info.len() & 0xFF) as u8);
    s.extend_from_slice(program_info);
    for &(stream_type, pid, es_info) in streams {
        s.push(stream_type);
        s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        s.push((pid & 0xFF) as u8);
        s.push(0xF0 | ((es_info.len() >> 8) as u8 & 0x0F));
        s.push((es_info.len() & 0xFF) as u8);
        s.extend_from_slice(es_info);
    }
    // section_length counts from byte 3 to the end, CRC included.
    let section_length = s.len() - 3 + 4;
    s[1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
    s[2] = (section_length & 0xFF) as u8;
    append_crc(&mut s);
    s
}

/// Wrap `section` in one 188-byte TS packet on `pid`: PUSI set, payload
/// only, continuity counter `cc`, `pointer_field` 0, 0xFF stuffing.
pub(crate) fn psi_packet(pid: u16, section: &[u8], cc: u8) -> Vec<u8> {
    let mut pkt = vec![0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field
    let end = 5 + section.len();
    assert!(end <= 188, "section too large for one TS packet");
    pkt[5..end].copy_from_slice(section);
    pkt
}

fn append_crc(s: &mut Vec<u8>) {
    let crc = crc32_mpeg2(s);
    s.extend_from_slice(&crc.to_be_bytes());
}

/// One PAT packet (TSID 1, CC 0) declaring `programs` as
/// `(program_number, pmt_pid)`.
pub(super) fn pat_packet_with_programs(programs: &[(u16, u16)], version: u8) -> Vec<u8> {
    psi_packet(0x0000, &build_pat_section(0x0001, version, programs), 0)
}

/// One PMT packet (CC 0) on `pmt_pid`; `streams` is
/// `(stream_type, elementary_pid)`, no descriptors.
pub(super) fn pmt_packet_for_test(
    pmt_pid: u16,
    program_number: u16,
    pcr_pid: u16,
    streams: &[(u8, u16)],
    version: u8,
) -> Vec<u8> {
    let es: Vec<(u8, u16, &[u8])> = streams.iter().map(|&(t, p)| (t, p, &[][..])).collect();
    psi_packet(
        pmt_pid,
        &build_pmt_section(program_number, pcr_pid, version, &[], &es),
        0,
    )
}

// --- multi-section PSI handling regression tests ---

/// Helper: build a PAT TS packet for one section of a multi-section table.
///
/// `section_number` and `last_section_number` are written into bytes [6]
/// and [7] of the section respectively; the CRC is recomputed so the
/// section is otherwise well-formed.
pub(super) fn pat_packet_with_multi_section(
    section_number: u8,
    last_section_number: u8,
    programs: &[(u16, u16)],
) -> Vec<u8> {
    // Version 0, same transport_stream_id as pat_packet_with_programs (0x0001).
    let mut pkt = pat_packet_with_programs(programs, 0);
    // Section bytes start at offset 5 (sync + 3 TS header bytes + pointer_field).
    // Byte [6] = section_number, byte [7] = last_section_number.
    pkt[5 + 6] = section_number;
    pkt[5 + 7] = last_section_number;
    // Recompute CRC over the section sans its 4-byte trailer.
    let section_len = 3 + (((pkt[5 + 1] as usize & 0x0F) << 8) | pkt[5 + 2] as usize);
    let section_end = 5 + section_len;
    let crc = crate::mpegts::common::crc32::crc32_mpeg2(&pkt[5..section_end - 4]);
    pkt[section_end - 4..section_end].copy_from_slice(&crc.to_be_bytes());
    pkt
}

/// Helper: build a PMT TS packet whose section has `last_section_number=1`.
pub(super) fn pmt_packet_with_multi_section(
    pmt_pid: u16,
    program_number: u16,
    pcr_pid: u16,
    streams: &[(u8, u16)],
    version: u8,
) -> Vec<u8> {
    let mut pkt = pmt_packet_for_test(pmt_pid, program_number, pcr_pid, streams, version);
    pkt[5 + 7] = 0x01;
    let section_len = 3 + (((pkt[5 + 1] as usize & 0x0F) << 8) | pkt[5 + 2] as usize);
    let section_end = 5 + section_len;
    let crc = crate::mpegts::common::crc32::crc32_mpeg2(&pkt[5..section_end - 4]);
    pkt[section_end - 4..section_end].copy_from_slice(&crc.to_be_bytes());
    pkt
}

/// Like `pat_packet_with_programs` but sets the 4-bit CC field to `cc`.
///
/// Use this in tests that feed two PAT packets on the same PID: the second
/// packet must use `cc=1` (or any value != the first) so that the demuxer's
/// spec-legal duplicate suppression does not swallow it.
pub(super) fn pat_packet_with_programs_cc(programs: &[(u16, u16)], version: u8, cc: u8) -> Vec<u8> {
    let mut pkt = pat_packet_with_programs(programs, version);
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
    pkt
}

/// Like `pmt_packet_for_test` but sets the 4-bit CC field to `cc`.
///
/// Use this in tests that feed two PMT packets on the same PID.
pub(super) fn pmt_packet_for_test_cc(
    pmt_pid: u16,
    program_number: u16,
    pcr_pid: u16,
    streams: &[(u8, u16)],
    version: u8,
    cc: u8,
) -> Vec<u8> {
    let mut pkt = pmt_packet_for_test(pmt_pid, program_number, pcr_pid, streams, version);
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
    pkt
}

/// Like `pat_packet_with_multi_section` but sets the 4-bit CC field to `cc`.
///
/// Use this when feeding multiple sections on PID 0x0000 in the same test.
pub(super) fn pat_packet_with_multi_section_cc(
    section_number: u8,
    last_section_number: u8,
    programs: &[(u16, u16)],
    cc: u8,
) -> Vec<u8> {
    let mut pkt = pat_packet_with_multi_section(section_number, last_section_number, programs);
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
    pkt
}

pub(super) fn drain_all_events(d: &mut Demuxer) -> Vec<DemuxEvent> {
    let mut events = Vec::new();
    while let Some(e) = d.next_event() {
        events.push(e);
    }
    events
}

// -------------------------------------------------------------------------
// PAT cleanup on program removal
// -------------------------------------------------------------------------
//
// When PAT removes a program, per-PID state for that program's PIDs is
// unreachable and must be cleaned. White-box tests inspect the private
// per-PID maps directly via `pub(super)` field access.

/// Build a 188-byte TS packet carrying a PCR via the adaptation field.
pub(super) fn pcr_packet_for_test(pid: u16, pcr_27mhz: u64) -> [u8; 188] {
    let base: u64 = pcr_27mhz / 300;
    let ext: u64 = pcr_27mhz % 300;
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    buf[1] = (pid >> 8) as u8 & 0x1F;
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x20; // adaptation_field_control = 0b10 (af only), CC=0
    buf[4] = 183; // af_length fills the rest
    buf[5] = 0x10; // PCR_flag=1
    buf[6] = (base >> 25) as u8;
    buf[7] = (base >> 17) as u8;
    buf[8] = (base >> 9) as u8;
    buf[9] = (base >> 1) as u8;
    buf[10] = (((base & 0x01) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
    buf[11] = (ext & 0xFF) as u8;
    buf
}

/// Build a payload-only TS packet on `pid` with the given CC. Drives
/// `cc_by_pid` registration without needing a PES header.
pub(super) fn payload_packet_for_test(pid: u16, cc: u8) -> [u8; 188] {
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    buf[1] = (pid >> 8) as u8 & 0x1F;
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x10 | (cc & 0x0F); // payload-only + CC
    buf
}

/// Fields of one hand-built TS packet. `payload` longer than 184 bytes is a
/// test bug and panics; shorter payloads are padded with adaptation-field
/// stuffing, never payload stuffing.
#[derive(Default, Clone, Copy)]
pub(super) struct PktSpec<'a> {
    pub pid: u16,
    pub cc: u8,
    pub pusi: bool,
    pub discontinuity: bool,
    pub rai: bool,
    pub pcr: Option<u64>,
    pub scrambling: u8,
    pub payload: &'a [u8],
}

/// One 188-byte TS packet (H.222.0 §2.4.3.2 header, §2.4.3.4 adaptation
/// field). An adaptation field is written when a flag or PCR needs it or
/// the payload is short; an empty payload yields adaptation-field-only.
pub(super) fn ts_packet(spec: &PktSpec<'_>) -> [u8; 188] {
    assert!(spec.payload.len() <= 184, "payload exceeds one TS packet");
    let has_af = spec.discontinuity || spec.rai || spec.pcr.is_some() || spec.payload.len() < 184;
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    buf[1] = (u8::from(spec.pusi) << 6) | ((spec.pid >> 8) as u8 & 0x1F);
    buf[2] = (spec.pid & 0xFF) as u8;
    let afc: u8 = match (has_af, spec.payload.is_empty()) {
        (false, _) => 0b01,
        (true, false) => 0b11,
        (true, true) => 0b10,
    };
    buf[3] = ((spec.scrambling & 0x03) << 6) | (afc << 4) | (spec.cc & 0x0F);
    let mut payload_at = 4;
    if has_af {
        assert!(
            spec.payload.len() <= 183,
            "no room for an adaptation field: shorten the payload"
        );
        // The adaptation field fills everything the payload leaves free.
        let af_len = 183 - spec.payload.len();
        assert!(
            af_len >= 1 || !(spec.discontinuity || spec.rai),
            "no room for the adaptation-field flags: shorten the payload"
        );
        buf[4] = af_len as u8;
        if af_len >= 1 {
            let mut flags = 0u8;
            if spec.discontinuity {
                flags |= 0x80;
            }
            if spec.rai {
                flags |= 0x40;
            }
            if spec.pcr.is_some() {
                flags |= 0x10;
            }
            buf[5] = flags;
        }
        if let Some(pcr) = spec.pcr {
            assert!(af_len >= 7, "no room for a PCR");
            let base = pcr / 300;
            let ext = pcr % 300;
            buf[6] = (base >> 25) as u8;
            buf[7] = (base >> 17) as u8;
            buf[8] = (base >> 9) as u8;
            buf[9] = (base >> 1) as u8;
            buf[10] = (((base & 1) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
            buf[11] = (ext & 0xFF) as u8;
        }
        payload_at = 5 + af_len;
    }
    buf[payload_at..payload_at + spec.payload.len()].copy_from_slice(spec.payload);
    buf
}

fn pes_timestamp(prefix: u8, ts: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((ts >> 30) as u8 & 0x07) << 1) | 1,
        (ts >> 22) as u8,
        (((ts >> 15) as u8 & 0x7F) << 1) | 1,
        (ts >> 7) as u8,
        (((ts & 0x7F) as u8) << 1) | 1,
    ]
}

/// A PES packet (H.222.0 §2.4.3.6). `bounded` writes `PES_packet_length`
/// from the content; otherwise it is 0 (unbounded, video only).
pub(super) fn pes_bytes(
    stream_id: u8,
    pts: Option<u64>,
    dts: Option<u64>,
    data_alignment: bool,
    es: &[u8],
    bounded: bool,
) -> Vec<u8> {
    assert!(pts.is_some() || dts.is_none(), "a DTS needs a PTS");
    let mut header_data = Vec::new();
    if let Some(pts) = pts {
        header_data.extend_from_slice(&pes_timestamp(
            if dts.is_some() { 0b0011 } else { 0b0010 },
            pts,
        ));
    }
    if let Some(dts) = dts {
        header_data.extend_from_slice(&pes_timestamp(0b0001, dts));
    }
    let pts_dts_flags = match (pts.is_some(), dts.is_some()) {
        (false, _) => 0x00,
        (true, false) => 0x80,
        (true, true) => 0xC0,
    };
    let length = if bounded {
        3 + header_data.len() + es.len()
    } else {
        0
    };
    assert!(length <= 0xFFFF, "bounded PES too long");
    let mut v = vec![0x00, 0x00, 0x01, stream_id];
    v.extend_from_slice(&(length as u16).to_be_bytes());
    v.push(0x80 | if data_alignment { 0x04 } else { 0x00 });
    v.push(pts_dts_flags);
    v.push(header_data.len() as u8);
    v.extend_from_slice(&header_data);
    v.extend_from_slice(es);
    v
}

/// Split `pes` over TS packets on `pid`: PUSI on the first only, CC from
/// `first_cc` incrementing mod 16, the last packet stuffed via its
/// adaptation field.
pub(super) fn pes_to_packets(pid: u16, first_cc: u8, pes: &[u8]) -> Vec<[u8; 188]> {
    pes.chunks(184)
        .enumerate()
        .map(|(i, chunk)| {
            ts_packet(&PktSpec {
                pid,
                cc: first_cc.wrapping_add(i as u8) & 0x0F,
                pusi: i == 0,
                payload: chunk,
                ..Default::default()
            })
        })
        .collect()
}

/// A demuxer that has already seen a PAT (TSID 1, program 1 on PMT PID
/// 0x1000) and a PMT declaring `streams` as `(stream_type, pid, es_info)`.
pub(super) fn demuxer_with_streams(
    cfg: DemuxerConfig,
    pcr_pid: u16,
    streams: &[(u8, u16, &[u8])],
) -> Demuxer {
    let mut d = Demuxer::with_config(cfg);
    d.feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
        .expect("feed PAT");
    let pmt = psi_packet(0x1000, &build_pmt_section(1, pcr_pid, 0, &[], streams), 0);
    d.feed(&pmt).expect("feed PMT");
    d
}

pub(super) fn feed_packets(d: &mut Demuxer, pkts: &[[u8; 188]]) -> Result<(), DemuxError> {
    pkts.iter().try_for_each(|p| d.feed_aligned(p))
}

pub(super) fn issues(events: &[DemuxEvent]) -> Vec<NonConformantIssue> {
    events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::NonConformant { issue, .. } => Some(issue.clone()),
            _ => None,
        })
        .collect()
}

/// Replace a section's trailing CRC-32 with one over its current bytes.
pub(super) fn recrc(section: &mut Vec<u8>) {
    section.truncate(section.len() - 4);
    append_crc(section);
}

#[test]
fn ts_packet_carries_pcr_discontinuity_and_payload() {
    let p = ts_packet(&PktSpec {
        pid: 0x100,
        cc: 3,
        pusi: true,
        discontinuity: true,
        pcr: Some(27_000_000),
        payload: &[0xAB; 10],
        ..Default::default()
    });
    assert_eq!(&p[..4], &[0x47, 0x41, 0x00, 0x33]); // PUSI, PID 0x100, AF+payload, CC 3
    assert_eq!(p[5] & 0x80, 0x80); // discontinuity_indicator
    assert_eq!(p[5] & 0x10, 0x10); // PCR_flag
    assert_eq!(&p[178..], &[0xAB; 10]);
}

#[test]
fn ts_packet_without_payload_is_adaptation_only_and_full_payload_has_no_af() {
    let af_only = ts_packet(&PktSpec {
        pid: 0x20,
        rai: true,
        ..Default::default()
    });
    assert_eq!(af_only[3], 0x20);
    assert_eq!((af_only[4], af_only[5]), (183, 0x40));
    let full = ts_packet(&PktSpec {
        pid: 0x20,
        cc: 5,
        payload: &[1; 184],
        ..Default::default()
    });
    assert_eq!(&full[..5], &[0x47, 0x00, 0x20, 0x15, 1]);
}

#[test]
#[should_panic(expected = "no room for the adaptation-field flags")]
fn ts_packet_refuses_a_flag_it_cannot_carry() {
    let _ = ts_packet(&PktSpec {
        discontinuity: true,
        payload: &[0; 183],
        ..Default::default()
    });
}

#[test]
fn pes_bytes_bounded_length_counts_header_and_es() {
    let pes = pes_bytes(0xC0, Some(90_000), None, true, &[0u8; 20], true);
    assert_eq!(u16::from_be_bytes([pes[4], pes[5]]) as usize, pes.len() - 6);
    assert_eq!(pes[8], 5); // PES_header_data_length for PTS only
}

#[test]
fn pes_bytes_encodes_pts_and_dts_with_marker_bits() {
    let pes = pes_bytes(0xE0, Some(90_000), Some(86_400), false, &[], false);
    assert_eq!(&pes[..6], &[0, 0, 1, 0xE0, 0, 0]);
    assert_eq!((pes[6], pes[7], pes[8]), (0x80, 0xC0, 10));
    assert_eq!(&pes[9..14], &[0x31, 0x00, 0x05, 0xBF, 0x21]); // PTS 90000, prefix 0011
    assert_eq!(&pes[14..19], &[0x11, 0x00, 0x05, 0xA3, 0x01]); // DTS 86400, prefix 0001
}

#[test]
fn pes_to_packets_sets_pusi_once_and_wraps_cc() {
    let pes = pes_bytes(0xC0, Some(0), None, true, &[0x55; 500], true);
    let pkts = pes_to_packets(0x101, 14, &pes);
    assert_eq!(pkts.len(), 3);
    let ccs: Vec<u8> = pkts.iter().map(|p| p[3] & 0x0F).collect();
    assert_eq!(ccs, [14, 15, 0]);
    let pusi: Vec<bool> = pkts.iter().map(|p| p[1] & 0x40 != 0).collect();
    assert_eq!(pusi, [true, false, false]);
    // The short tail carries stuffing in its adaptation field, so the
    // payload is exactly the remaining PES bytes.
    let tail = &pkts[2];
    let rest = pes.len() - 2 * 184;
    assert_eq!(&tail[188 - rest..], &pes[2 * 184..]);
}

#[test]
fn demuxer_with_streams_announces_the_program() {
    let mut d = demuxer_with_streams(DemuxerConfig::default(), 0x100, &[(0x0F, 0x100, &[][..])]);
    let events = drain_all_events(&mut d);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DemuxEvent::ProgramMap(_)))
    );
    let pes = pes_bytes(0xC0, Some(90_000), None, true, &[0u8; 8], true);
    feed_packets(&mut d, &pes_to_packets(0x100, 0, &pes)).unwrap();
    assert!(issues(&drain_all_events(&mut d)).is_empty());
}

#[test]
fn recrc_makes_an_edited_section_valid_again() {
    let mut s = build_pat_section(1, 0, &[(1, 0x1000)]);
    let good = s.clone();
    s[8] = 0x02; // program_number low byte
    recrc(&mut s);
    assert_ne!(s, good);
    let body = s.len() - 4;
    assert_eq!(s[body..], crc32_mpeg2(&s[..body]).to_be_bytes());
}

#[test]
fn pat_section_matches_hand_computed_bytes() {
    // TSID 1, version 0, program 1 -> PMT PID 0x1000. CRC-32/MPEG-2 of the
    // first 12 bytes is 0x2AB104B2 (the PAT most muxers emit by default).
    assert_eq!(
        build_pat_section(0x0001, 0, &[(1, 0x1000)]),
        [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x2A, 0xB1,
            0x04, 0xB2
        ]
    );
}

#[test]
fn pmt_section_matches_hand_computed_bytes() {
    // Program 1, PCR PID 0x0100, version 0, no program descriptors, one
    // H.264 stream (type 0x1B) on PID 0x0100 with no ES descriptors:
    // section_length 18, CRC-32/MPEG-2 0x15BD4D56.
    assert_eq!(
        build_pmt_section(1, 0x0100, 0, &[], &[(0x1B, 0x0100, &[][..])]),
        [
            0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE1,
            0x00, 0xF0, 0x00, 0x15, 0xBD, 0x4D, 0x56
        ]
    );
}

/// One bounded sync-metadata PES (stream_id 0xFC, PTS 90 000) on `pid`
/// carrying a single `Metadata_AU_cell` (H.222.0 §2.12.4.2), as TS
/// packets starting at continuity counter `cc`.
pub(super) fn au_cell_pes_packets(
    pid: u16,
    cc: u8,
    header: crate::mpegts::au_cell::AuCellHeader,
    payload: &[u8],
) -> Vec<[u8; 188]> {
    let mut cell = Vec::new();
    crate::mpegts::au_cell::write_metadata_au_cell(&mut cell, header, payload)
        .expect("AU cell fits");
    pes_to_packets(
        pid,
        cc,
        &pes_bytes(0xFC, Some(90_000), None, true, &cell, true),
    )
}
