//! Hand-built PSI sections and single-packet TS wrapping, shared by the
//! `mpegts` test binary's demux tests (`pts_unwrap`, `demux_multi_program`,
//! `demux_psi_program_number`). Byte layouts per ISO/IEC 13818-1 §2.4.4;
//! every section carries a valid CRC-32/MPEG-2 trailer. `demux_strict`
//! keeps its own builders on purpose — it needs oversized and deliberately
//! split sections these single-packet shapes cannot express.

/// Build a PAT section (table_id 0x00). `programs` is `(program_number,
/// pmt_pid)`.
pub(crate) fn build_pat_section(version: u8, programs: &[(u16, u16)]) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x00); // table_id = PAT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F)); // ssi=1, reserved, length hi
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
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

/// Build a PMT section (table_id 0x02). `streams` is `(stream_type,
/// elementary_pid, es_info_descriptor_bytes)`.
pub(crate) fn build_pmt_section(
    program_number: u16,
    pcr_pid: u16,
    version: u8,
    streams: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let stream_loop_len: usize = streams.iter().map(|(_, _, d)| 5 + d.len()).sum();
    let section_length = 9 + stream_loop_len + 4;
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x02); // table_id = PMT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&program_number.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    s.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    s.push((pcr_pid & 0xFF) as u8);
    s.push(0xF0); // reserved | program_info_length hi
    s.push(0x00); // program_info_length lo (no program descriptors)
    for &(stream_type, pid, descriptors) in streams {
        s.push(stream_type);
        s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
        s.push((pid & 0xFF) as u8);
        s.push(0xF0 | ((descriptors.len() >> 8) as u8 & 0x0F));
        s.push((descriptors.len() & 0xFF) as u8);
        s.extend_from_slice(descriptors);
    }
    append_crc(&mut s);
    s
}

/// Append the CRC-32/MPEG-2 trailer over everything written so far.
pub(crate) fn append_crc(section: &mut Vec<u8>) {
    let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(section);
    section.extend_from_slice(&crc.to_be_bytes());
}

/// Wrap a PSI section into one 188-byte TS packet (PUSI, payload-only).
///
/// `cc` must advance across successive packets on the same PID or the
/// demuxer's duplicate suppression swallows the second one.
pub(crate) fn psi_packet(pid: u16, section: &[u8], cc: u8) -> Vec<u8> {
    let mut pkt = vec![0xFFu8; 188];
    pkt[0] = 0x47; // sync byte
    pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID hi
    pkt[2] = (pid & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only + continuity counter
    pkt[4] = 0x00; // pointer_field
    let end = 5 + section.len();
    assert!(end <= 188, "section too large for one TS packet");
    pkt[5..end].copy_from_slice(section);
    pkt
}
