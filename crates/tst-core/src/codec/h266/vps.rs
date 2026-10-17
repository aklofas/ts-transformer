//! H.266 VPS parser. Per H.266 V4 §7.3.2.3.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use alloc::vec::Vec;

crate::binding_record! {
    /// Parsed H.266 Video Parameter Set.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct H266Vps {
        /// 4-bit `vps_id` — identifies this VPS (H.266 V4 §7.3.2.3).
        pub vps_id: u8,
        /// Maximum number of spatial layers (`vps_max_layers_minus1 + 1`).
        pub max_layers: u8,
        /// Maximum number of temporal sub-layers (`vps_max_sub_layers_minus1 +
        /// 1`).
        pub max_sub_layers: u8,
        /// The original RBSP bytes as supplied to the parser.
        pub raw_rbsp: Vec<u8>,
    }
}

/// Parse an H.266 VPS RBSP (Annex-B start codes already stripped,
/// emulation-prevention bytes preserved). Per H.266 V4 §7.3.2.3.
///
/// Current scope extracts only `vps_id`, `max_layers`, `max_sub_layers`.
/// Profile/Tier/Level loops, OLS info, DPB/HRD parameters are not
/// surfaced — `raw_rbsp` carries the full input so consumers needing
/// more can call deeper parsers later.
pub fn parse_vps(rbsp: &[u8]) -> Result<H266Vps, CodecParseError> {
    let mut br = BitReader::new(rbsp);
    let vps_id = br.read_u(4)? as u8;
    let max_layers_minus1 = br.read_u(6)? as u8;
    let max_sublayers_minus1 = br.read_u(3)? as u8;
    Ok(H266Vps {
        vps_id,
        max_layers: max_layers_minus1.saturating_add(1),
        max_sub_layers: max_sublayers_minus1.saturating_add(1),
        raw_rbsp: rbsp.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal VPS RBSP — vps_id=0, max_layers_minus1=0, max_sublayers_minus1=0,
    /// rest zeros. 4+6+3+... = enough bits to exit the current parse early.
    /// Constructed by hand: nibbles 0000 (vps_id=0) | 000000 (max_layers=0) |
    /// 000 (max_sublayers=0) | 0 (default_ptl flag=0) | 0 (all_indep=0) |
    /// trailing rbsp_trailing_bits(): 1 bit set + zero pad to byte align.
    fn minimal_vps_rbsp() -> Vec<u8> {
        // First byte:  vps_id(4)=0 | max_layers_minus1(6)[upper 4]=0 → 0b0000_0000
        // Second byte: max_layers_minus1(6)[lower 2]=0 | max_sublayers(3)=0 |
        //              default_ptl_flag=0 | all_indep=0 | trailing one bit=1
        //              → 0b0000_0010
        vec![0x00, 0x02]
    }

    #[test]
    fn parse_vps_minimal() {
        let rbsp = minimal_vps_rbsp();
        let vps = parse_vps(&rbsp).expect("minimal VPS should parse");
        assert_eq!(vps.vps_id, 0);
        assert_eq!(vps.max_layers, 1, "max_layers = max_layers_minus1 + 1");
        assert_eq!(vps.max_sub_layers, 1);
        assert_eq!(vps.raw_rbsp, rbsp);
    }

    #[test]
    fn parse_vps_truncated_returns_err() {
        let truncated = vec![];
        assert!(parse_vps(&truncated).is_err());
    }

    #[test]
    fn parse_vps_truncated_byte_returns_err() {
        // Parser needs 4+6+3 = 13 bits; one byte (8 bits) is insufficient.
        // The 3-bit max_sublayers_minus1 read should bail with TruncatedRbsp.
        assert!(parse_vps(&[0x00]).is_err());
    }

    #[test]
    fn parse_vps_max_fields() {
        // vps_id=15 (max u4), max_layers_minus1=63 (max u6), max_sublayers_minus1=7 (max u3).
        // All-ones across 13 bits:
        //   byte 0: vps_id(4)=1111 | max_layers[upper 4]=1111 → 0xFF
        //   byte 1: max_layers[lower 2]=11 | max_sublayers(3)=111 | pad=000 → 0xF8
        let rbsp = vec![0xFF, 0xF8];
        let vps = parse_vps(&rbsp).expect("max-field VPS should parse");
        assert_eq!(vps.vps_id, 15);
        assert_eq!(vps.max_layers, 64); // saturating_add(1) on 63
        assert_eq!(vps.max_sub_layers, 8);
    }
}
