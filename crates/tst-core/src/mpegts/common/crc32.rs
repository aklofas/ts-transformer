//! CRC-32/MPEG-2 — used by MPEG-TS PSI section integrity.
//!
//! Polynomial: 0x04C11DB7, init: 0xFFFFFFFF, refin/refout: false, xorout: 0.
//!
//! Hand-rolled to keep `tst-core` dep-free; matches the no-deps style of the
//! `klv` module's checksum/imapb/length helpers.

/// Pre-computed CRC-32/MPEG-2 table. Table is generated at compile time via a
/// const fn so we don't need a build script or runtime init.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc: u32 = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Compute CRC-32/MPEG-2 over `data`.
///
/// Init = 0xFFFFFFFF, no input/output reflection, no final xor — the
/// MPEG-2 PSI variant per ITU-T H.222.0 V9 / ISO/IEC 13818-1 Annex A.1
/// "CRC decoder model" (PDF p.240).
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let idx = (((crc >> 24) ^ (byte as u32)) & 0xFF) as usize;
        crc = (crc << 8) ^ TABLE[idx];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty input — init value passes through with no transformation.
    #[test]
    fn empty_input() {
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }

    /// Standard CRC-32/MPEG-2 test vector: ASCII "123456789" -> 0x0376E6E7.
    #[test]
    fn standard_check_value() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    /// Single zero byte: CRC-32/MPEG-2 reference value 0x4E08BFB4
    /// (independently verified; not 0x4F5344CD).
    #[test]
    fn single_zero_byte() {
        assert_eq!(crc32_mpeg2(&[0x00]), 0x4E08_BFB4);
    }

    /// PAT golden vector — minimal pre-CRC PAT for program_number=1, pmt_pid=0x1000.
    ///
    /// Section bytes (table_id..last byte before CRC), 12 bytes total:
    ///   table_id=0x00,
    ///   syntax+length=0xB0_0D (section_length=13 covers the 9-byte payload + 4-byte CRC),
    ///   tsid=0x0001,
    ///   reserved+vers+curr=0xC1,
    ///   sect=0x00, last=0x00,
    ///   prog_num=0x0001,
    ///   reserved+pmt_pid=0xF0_00 (pmt_pid=0x1000, reserved=0b111).
    ///
    /// CRC value verified by an independent reference implementation.
    #[test]
    fn pat_section_golden() {
        let section = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        assert_eq!(crc32_mpeg2(&section), 0x2AB1_04B2);
    }

    /// Round-trip property: CRC over (section || CRC) is zero — standard PSI invariant.
    #[test]
    fn pat_section_round_trip_zero() {
        let section = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        let crc = crc32_mpeg2(&section);
        let mut full = Vec::with_capacity(16);
        full.extend_from_slice(&section);
        full.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(crc32_mpeg2(&full), 0);
    }

    /// Reference loop implementation — bit-by-bit, no table. Used as the
    /// cross-check for table-driven correctness.
    fn crc32_naive(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// Random byte sequences must agree between table-driven and naive.
    #[test]
    fn table_matches_naive_random() {
        // Deterministic pseudo-random sequence — no rand dep.
        let mut buf = [0u8; 1024];
        let mut x: u32 = 0xCAFE_BABE;
        for b in buf.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 16) as u8;
        }
        assert_eq!(crc32_mpeg2(&buf), crc32_naive(&buf));
    }

    /// Length sweep — 0..=128 byte buffers. Table and naive must agree.
    #[test]
    fn table_matches_naive_length_sweep() {
        for len in 0..=128 {
            let buf: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(crc32_mpeg2(&buf), crc32_naive(&buf), "len={}", len);
        }
    }
}
