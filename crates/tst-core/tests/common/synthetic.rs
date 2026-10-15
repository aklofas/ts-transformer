//! Synthetic access units and KLV packets shared by the `mpegts` and `codec`
//! test binaries. Payload bytes are placeholders with the right framing;
//! the tests assert on framing and carriage, never on decoded content.

/// One ST 0601 KLV packet: the 16-byte UAS Datalink universal label and a
/// zero BER length (an empty local set).
pub fn minimal_klv() -> Vec<u8> {
    vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ]
}

/// An Annex-B H.264 IDR access unit: SPS, PPS and one IDR slice NAL, each
/// with `nal_ref_idc` 3 and a short placeholder body.
pub fn synthetic_h264_au() -> Vec<u8> {
    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        // forbidden_zero(1)=0 | nal_ref_idc(2) | nal_unit_type(5)
        let nri: u8 = 0b11; // IDR/SPS/PPS all use high nal_ref_idc
        v.push((nri << 5) | nal_type);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(nal(7, &[0x42, 0xC0, 0x28, 0xD9])); // SPS
    au.extend(nal(8, &[0xCE, 0x38, 0x80])); // PPS
    au.extend(nal(5, &[0x88, 0x84, 0x0A, 0x7C, 0x11])); // IDR slice
    au
}

/// A minimal H.264 access unit: AUD plus one IDR slice.
pub fn build_minimal_h264_au() -> Vec<u8> {
    // Annex-B: AUD (nal_type=9) + IDR (nal_type=5).
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC,
    ]
}

/// One ST 0601 KLV packet whose local set holds a single all-zero Tag 2
/// (Precision Time Stamp, 8 bytes).
pub fn build_dummy_klv() -> Vec<u8> {
    let body = [2u8, 8, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut out = Vec::with_capacity(17 + body.len());
    out.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    out.push(body.len() as u8); // BER short-form length
    out.extend_from_slice(&body);
    out
}

/// One ADTS frame (MPEG-2 AAC, no CRC) for `sample_rate_index` and
/// `channel_config`, zero-padded to `total_len` bytes (at least 7).
pub fn build_adts_frame(sample_rate_index: u8, channel_config: u8, total_len: u32) -> Vec<u8> {
    let mut h = vec![0u8; 7];
    h[0] = 0xFF;
    h[1] = 0b1111_0000 | (1 << 3) | 1; // ID=MPEG-2, layer=0, no CRC
    h[2] = (1 << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_config >> 2) & 1);
    h[3] = ((channel_config & 0b11) << 6) | (((total_len >> 11) & 0b11) as u8);
    h[4] = ((total_len >> 3) & 0xFF) as u8;
    h[5] = (((total_len & 0b111) as u8) << 5) | 0b1_1111;
    h[6] = 0b11_1111 << 2;
    let pad = total_len as usize - 7;
    let mut out = h;
    out.extend(std::iter::repeat(0u8).take(pad));
    out
}

/// One ST 0601-shaped KLV packet: the 16-byte UAS Datalink universal label,
/// a BER short-form length of 4 and four marker bytes (no real ST 0601
/// content).
pub fn synthetic_klv_ls() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    v.push(0x04);
    v.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    v
}
