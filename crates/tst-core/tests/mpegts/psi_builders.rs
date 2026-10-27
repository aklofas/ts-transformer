//! Hand-built PSI sections, single- and multi-packet TS wrapping and
//! adaptation-field PCR packets, shared by the `mpegts` test binary's demux
//! tests (`pts_unwrap`, `demux_multi_program`, `demux_pcr_history`,
//! `demux_psi_program_number`, `demux_strict`, `large_async_klv`). Byte
//! layouts per ISO/IEC 13818-1 §2.4.4; every section carries a valid
//! CRC-32/MPEG-2 trailer. `demux_strict` keeps its own oversized PMT
//! section builder, whose descriptor loop spans three packets, and its own
//! PAT packet builder.

/// Build a PAT section (table_id 0x00). `programs` is `(program_number,
/// pmt_pid)`.
pub(crate) fn build_pat_section(version: u8, programs: &[(u16, u16)]) -> Vec<u8> {
    let section_length = 5 + 4 * programs.len() + 4;
    // 12-bit field: a silent truncation would still CRC-check, so refuse
    // loudly instead of handing a test a malformed section.
    assert!(
        section_length <= 0x0FFF,
        "PAT section_length {section_length} exceeds the 12-bit field"
    );
    let mut s = Vec::with_capacity(3 + section_length);
    s.push(0x00); // table_id = PAT
    s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F)); // ssi=1, reserved, length hi
    s.push((section_length & 0xFF) as u8);
    s.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
    s.push(0xC1 | ((version & 0x1F) << 1)); // reserved | version | current_next=1
    s.push(0x00); // section_number
    s.push(0x00); // last_section_number
    for &(pn, pid) in programs {
        assert!(pid <= 0x1FFF, "PMT PID {pid:#06x} exceeds the 13-bit field");
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
    assert!(
        pcr_pid <= 0x1FFF,
        "PCR PID {pcr_pid:#06x} exceeds the 13-bit field"
    );
    let stream_loop_len: usize = streams.iter().map(|(_, _, d)| 5 + d.len()).sum();
    let section_length = 9 + stream_loop_len + 4;
    assert!(
        section_length <= 0x0FFF,
        "PMT section_length {section_length} exceeds the 12-bit field"
    );
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
        assert!(
            pid <= 0x1FFF,
            "elementary PID {pid:#06x} exceeds the 13-bit field"
        );
        assert!(
            descriptors.len() <= 0x0FFF,
            "ES_info_length {} exceeds the 12-bit field",
            descriptors.len()
        );
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
    assert!(pid <= 0x1FFF, "PID {pid:#06x} exceeds the 13-bit field");
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

/// Pack a PSI section into as many 188-byte TS packets on `pid` as it needs.
///
/// First packet: PUSI=1, pointer_field 0, then up to 183 section bytes.
/// Continuation packets: PUSI=0, up to 184 section bytes each. The last
/// packet is padded with 0xFF stuffing; no packet carries an adaptation
/// field. Continuity counters start at `cc_start` and advance mod 16.
pub(crate) fn pack_section(pid: u16, section: &[u8], cc_start: u8) -> Vec<[u8; 188]> {
    assert!(pid <= 0x1FFF, "PID {pid:#06x} exceeds the 13-bit field");
    let pid_hi = (pid >> 8) as u8 & 0x1F;
    let pid_lo = (pid & 0xFF) as u8;
    let mut pkts = Vec::new();
    let mut cc = cc_start & 0x0F;
    let mut pos = 0usize;
    while pkts.is_empty() || pos < section.len() {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47; // sync byte
        pkt[2] = pid_lo;
        pkt[3] = 0x10 | cc; // payload-only + continuity counter
        let start = if pkts.is_empty() {
            pkt[1] = 0x40 | pid_hi; // PUSI + PID hi
            pkt[4] = 0x00; // pointer_field
            5
        } else {
            pkt[1] = pid_hi; // PID hi, no PUSI
            4
        };
        let chunk = (section.len() - pos).min(188 - start);
        pkt[start..start + chunk].copy_from_slice(&section[pos..pos + chunk]);
        pos += chunk;
        pkts.push(pkt);
        cc = (cc + 1) & 0x0F;
    }
    pkts
}

/// One adaptation-field-only TS packet carrying `pcr_27mhz` on `pid`
/// (ISO/IEC 13818-1 §2.4.3.4): 33-bit base (pcr/300), 6 reserved 1-bits,
/// 9-bit extension (pcr%300); CC 0, no payload.
pub(crate) fn pcr_packet(pid: u16, pcr_27mhz: u64) -> [u8; 188] {
    let base: u64 = pcr_27mhz / 300;
    let ext: u64 = pcr_27mhz % 300;
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47; // sync
    buf[1] = (pid >> 8) as u8 & 0x1F; // no PUSI, no TEI
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x20; // adaptation_field_control = 0b10 (adaptation only), CC=0
    buf[4] = 183; // af_length: fills the rest of the packet
    buf[5] = 0x10; // flags: PCR_flag=1 (bit 4), others 0
    buf[6] = (base >> 25) as u8;
    buf[7] = (base >> 17) as u8;
    buf[8] = (base >> 9) as u8;
    buf[9] = (base >> 1) as u8;
    buf[10] = (((base & 0x01) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
    buf[11] = (ext & 0xFF) as u8;
    // Bytes 12..188 are stuffing (0xFF), which matches the rest of the AF.
    buf
}
