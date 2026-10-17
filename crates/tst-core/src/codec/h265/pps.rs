//! PPS parser per H.265 §7.3.2.3. Only `pps_pic_parameter_set_id` and
//! `pps_seq_parameter_set_id` are exposed; everything else is
//! decoder-internal.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use alloc::vec::Vec;

crate::binding_record! {
    /// Parsed H.265 Picture Parameter Set.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct H265Pps {
        /// `pps_pic_parameter_set_id` ∈ [0, 63].
        pub pps_pic_parameter_set_id: u8,
        /// `pps_seq_parameter_set_id` — links this PPS to an SPS. ∈ [0, 15].
        pub pps_seq_parameter_set_id: u8,
        /// The original RBSP bytes as supplied to the parser.
        pub raw_rbsp: Vec<u8>,
    }
}

pub fn parse_pps(rbsp: &[u8]) -> Result<H265Pps, CodecParseError> {
    if rbsp.is_empty() {
        return Err(CodecParseError::TruncatedRbsp {
            offset_bits: 0,
            needed_bits: 8,
        });
    }
    let mut br = BitReader::new(rbsp);
    let pps_pic_parameter_set_id = br.read_ue_max("pps_pic_parameter_set_id", 63)? as u8;
    let pps_seq_parameter_set_id = br.read_ue_max("pps_seq_parameter_set_id", 15)? as u8;
    Ok(H265Pps {
        pps_pic_parameter_set_id,
        pps_seq_parameter_set_id,
        raw_rbsp: rbsp.to_vec(),
    })
}
