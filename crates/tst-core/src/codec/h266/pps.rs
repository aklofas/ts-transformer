//! H.266 PPS parser. Per H.266 V4 §7.3.2.5.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use alloc::vec::Vec;

crate::binding_record! {
    /// Parsed H.266 Picture Parameter Set.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct H266Pps {
        /// `pps_id` (6-bit) ∈ [0, 63] — H.266 V4 §7.3.2.5.
        pub pps_id: u8,
        /// `sps_id` (4-bit) — links this PPS to an SPS.
        pub sps_id: u8,
        /// The original RBSP bytes as supplied to the parser.
        pub raw_rbsp: Vec<u8>,
    }
}

/// Parse an H.266 PPS RBSP. Per H.266 V4 §7.3.2.5.
///
/// Current scope extracts only `pps_id` and `sps_id`. All other fields
/// stay in `raw_rbsp` for consumers needing deeper info later.
pub fn parse_pps(rbsp: &[u8]) -> Result<H266Pps, CodecParseError> {
    let mut br = BitReader::new(rbsp);
    let pps_id = br.read_u(6)? as u8;
    let sps_id = br.read_u(4)? as u8;
    Ok(H266Pps {
        pps_id,
        sps_id,
        raw_rbsp: rbsp.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal PPS — pps_id=0, sps_id=0, then a trailing bit.
    /// First byte: pps_id(6)=0 | sps_id(4)[upper 2]=0 → 0b0000_0000
    /// Second byte: sps_id(4)[lower 2]=0 | trailing one + zeros → 0b0010_0000
    fn minimal_pps_rbsp() -> Vec<u8> {
        vec![0x00, 0x20]
    }

    #[test]
    fn parse_pps_minimal() {
        let pps = parse_pps(&minimal_pps_rbsp()).expect("minimal PPS should parse");
        assert_eq!(pps.pps_id, 0);
        assert_eq!(pps.sps_id, 0);
    }

    #[test]
    fn parse_pps_truncated_returns_err() {
        // Empty input — bitreader bails on the first read.
        assert!(parse_pps(&[]).is_err());
    }

    #[test]
    fn parse_pps_truncated_byte_returns_err() {
        // Parser needs 6+4 = 10 bits; one byte (8 bits) is insufficient.
        assert!(parse_pps(&[0xFF]).is_err());
    }

    #[test]
    fn parse_pps_max_ids() {
        // pps_id=63 (max u6), sps_id=15 (max u4); 10 bits all-ones.
        //   byte 0: pps_id(6)=111111 | sps_id[upper 2]=11 → 0xFF
        //   byte 1: sps_id[lower 2]=11 | pad=000000 → 0xC0
        // Trailing bits are unread by the current parser, so no rbsp_trailing_bits
        // pattern is required.
        let pps = parse_pps(&[0xFF, 0xC0]).expect("max-id PPS should parse");
        assert_eq!(pps.pps_id, 63);
        assert_eq!(pps.sps_id, 15);
    }
}
