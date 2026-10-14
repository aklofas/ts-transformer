//! Hand-built PSI sections and single-packet TS wrapping for the demuxer's
//! unit tests (ISO/IEC 13818-1 §2.4.4). Fields are masked to their widths,
//! never range-checked, so tests may build deliberately odd sections. The
//! integration-test twin is `tests/mpegts/psi_builders.rs`.

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
