//! Hand-built PSI sections and single-packet TS wrapping for the demuxer's
//! unit tests (ISO/IEC 13818-1 §2.4.4). Fields are masked to their widths,
//! never range-checked, so tests may build deliberately odd sections. The
//! integration-test twin is `tests/mpegts/psi_builders.rs`.

use crate::mpegts::demux::demuxer::Demuxer;
use crate::mpegts::demux::event::DemuxEvent;
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
