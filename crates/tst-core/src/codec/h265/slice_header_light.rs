//! H.265 slice segment header (light subset) parser.
//!
//! Per H.265 §7.3.6.1.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use crate::codec::h265::sps::H265Sps;
use alloc::vec::Vec;

/// Light-weight H.265 slice segment header — fields required for keyframe
/// detection and frame-type classification without walking into slice data.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct H265SliceHeaderLight {
    /// True when `first_slice_segment_in_pic_flag == 1` — this slice covers
    /// the first CTB row in the picture, marking the start of a new frame.
    pub first_in_pic: bool,
    /// Slice type (B / P / I). Set from bitstream when `first_in_pic == true`;
    /// returns [`H265SliceType::I`] as a conservative fallback for continuation
    /// slices where PPS context is required to skip `slice_segment_address`.
    pub slice_type: H265SliceType,
    /// `slice_pic_parameter_set_id` — links this slice to a PPS.
    pub pps_id: u8,
    /// `pic_order_cnt_lsb` read using the bit width from the supplied SPS
    /// (`log2_max_pic_order_cnt_lsb_minus4 + 4`). `Some(0)` for IDR slices
    /// (implicit per spec). `None` when no SPS context was supplied.
    pub pic_order_cnt_lsb: Option<u16>,
    /// True when `nal_unit_type` is IDR_W_RADL (19) or IDR_N_LP (20).
    pub idr: bool,
    /// Raw RBSP bytes as supplied by the caller — preserved for downstream
    /// parsers that need fields beyond what this light subset extracts.
    pub raw_rbsp: Vec<u8>,
}

/// H.265 slice type (B / P / I). Only three values are defined by the spec
/// (H.265 §7.4.7.1 Table 7-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum H265SliceType {
    /// B slice — bidirectionally predicted.
    B,
    /// P slice — predicted.
    P,
    /// I slice — intra-coded.
    I,
}

impl H265SliceType {
    fn from_raw(v: u32) -> Result<Self, CodecParseError> {
        match v {
            0 => Ok(Self::B),
            1 => Ok(Self::P),
            2 => Ok(Self::I),
            _ => Err(CodecParseError::ReservedValue {
                field: "slice_type",
                value: v,
            }),
        }
    }
}

/// Per H.265 §3.x: NAL types 16–23 are IRAP (intra random access point).
const fn is_irap_nal(nal_unit_type: u8) -> bool {
    matches!(nal_unit_type, 16..=23)
}

/// Parse a light H.265 slice segment header from a RBSP byte slice.
///
/// # Input contract
///
/// `rbsp` is the raw RBSP body of a slice NAL — Annex B start code stripped,
/// NAL header (2 bytes for H.265) stripped, emulation-prevention bytes
/// preserved (matches `NalUnit::H265 { payload }`). The light parser reads
/// `first_slice_segment_in_pic_flag`, optionally `no_output_of_prior_pics_flag`
/// (IRAP only), `slice_pic_parameter_set_id`, and `slice_type` (first-segment
/// slices only), then optionally `pic_order_cnt_lsb` when an SPS context is
/// available.
///
/// `nal_unit_type` is the 6-bit NAL unit type from the NAL header byte pair
/// — used to derive `idr` and to gate IRAP-specific fields.
///
/// # Known limitation
///
/// This parser assumes `num_extra_slice_header_bits == 0` (the PPS default).
/// Streams where the PPS overrides this will be silently mis-parsed. Full
/// slice header support requires PPS context and is deferred.
///
/// # Errors
///
/// Returns [`CodecParseError::TruncatedRbsp`] when `rbsp` is too short.
/// Returns [`CodecParseError::InvalidGolomb`] on a malformed Exp-Golomb
/// codeword. Returns [`CodecParseError::ReservedValue`] when `slice_type`
/// or `slice_pic_parameter_set_id` is out of range.
pub fn parse_slice_header_light(
    rbsp: &[u8],
    sps: Option<&H265Sps>,
    nal_unit_type: u8,
) -> Result<H265SliceHeaderLight, CodecParseError> {
    let mut br = BitReader::new(rbsp);
    let first_slice_segment_in_pic_flag = br.read_u(1)? == 1;
    if is_irap_nal(nal_unit_type) {
        // no_output_of_prior_pics_flag (u(1)) — present for all IRAP NALs.
        let _no_output_of_prior_pics_flag = br.read_u(1)?;
    }
    let pps_id_u32 = br.read_ue()?;
    let pps_id: u8 = u8::try_from(pps_id_u32).map_err(|_| CodecParseError::ReservedValue {
        field: "slice_pic_parameter_set_id",
        value: pps_id_u32,
    })?;

    // For continuation slices (first_in_pic == false), the next fields are
    // dependent_slice_segment_flag (u(1)) and slice_segment_address (u(v))
    // whose bit width comes from the PPS CtbAddrInSliceSegmentToRs[] size —
    // context we don't have here. We can still extract slice_type when
    // first_in_pic == true; for continuation slices return H265SliceType::I
    // as a benign fallback. The IDR case (which we care about most) is
    // always first_in_pic = true in well-formed bitstreams.
    let slice_type = if first_slice_segment_in_pic_flag {
        // num_extra_slice_header_bits slice_reserved_flag[i] bits would
        // appear here if PPS signalled a non-zero count; we assume the
        // default (0) which is correct for most real streams.
        let slice_type_raw = br.read_ue()?;
        H265SliceType::from_raw(slice_type_raw)?
    } else {
        // Conservative fallback — continuation slice without PPS context.
        H265SliceType::I
    };

    let pic_order_cnt_lsb = if let Some(sps) = sps {
        // pic_order_cnt_lsb is present in all non-IDR slice headers.
        // For IDR slices it is implicitly 0 per H.265 §8.3.1.
        if crate::codec::nal::is_idr(crate::codec::VideoCodec::H265, nal_unit_type) {
            Some(0)
        } else {
            let bits = sps.log2_max_pic_order_cnt_lsb_minus4 as u32 + 4;
            Some(br.read_u(bits)? as u16)
        }
    } else {
        None
    };

    Ok(H265SliceHeaderLight {
        first_in_pic: first_slice_segment_in_pic_flag,
        slice_type,
        pps_id,
        pic_order_cnt_lsb,
        idr: crate::codec::nal::is_idr(crate::codec::VideoCodec::H265, nal_unit_type),
        raw_rbsp: rbsp.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_first_slice_idr() {
        // Build synthetic RBSP for an IDR slice (nal_unit_type=19, IRAP):
        //   first_slice_segment_in_pic_flag = 1  → u(1): '1'        1 bit
        //   no_output_of_prior_pics_flag = 0     → u(1): '0'        1 bit  (IRAP)
        //   slice_pic_parameter_set_id = 0       → ue(v): '1'       1 bit
        //   slice_type = 2 (I)                   → ue(v): '011'     3 bits
        // Total: 6 bits → pack into 1 byte + stop/padding:
        //   1 0 1 011 = 0b_10_1011_xx → byte 0 = 0b_1010_1100 = 0xAC
        //   stop-bit byte: 0x80 (1000_0000)
        let rbsp = vec![0xAC, 0x80];
        let slice = parse_slice_header_light(&rbsp, None, 19).unwrap();
        assert!(slice.first_in_pic);
        assert_eq!(slice.slice_type, H265SliceType::I);
        assert_eq!(slice.pps_id, 0);
        assert_eq!(slice.pic_order_cnt_lsb, None); // no SPS supplied
        assert!(slice.idr);
    }

    #[test]
    fn non_irap_nal_does_not_eat_irap_flag() {
        // NAL type 1 (TRAIL_N) — not IRAP, so no_output_of_prior_pics_flag
        // is NOT present.  Bit layout:
        //   first_slice_segment_in_pic_flag = 1  → '1'    1 bit
        //   slice_pic_parameter_set_id = 0       → '1'    1 bit
        //   slice_type = 2 (I)                   → '011'  3 bits
        // Total: 5 bits → 1_1_011_xxx → byte 0 = 0b_1101_1000 = 0xD8
        let rbsp = vec![0xD8];
        let slice = parse_slice_header_light(&rbsp, None, 1).unwrap();
        assert!(slice.first_in_pic);
        assert_eq!(slice.slice_type, H265SliceType::I);
        assert!(!slice.idr);
    }

    #[test]
    fn idr_nal_marks_idr_true_for_w_radl_and_n_lp() {
        // Both IDR_W_RADL (19) and IDR_N_LP (20) must set idr=true.
        // Same RBSP as the first test (both are IRAP, same bit layout).
        let rbsp = vec![0xAC, 0x80];
        for nal in [19_u8, 20] {
            let slice = parse_slice_header_light(&rbsp, None, nal).unwrap();
            assert!(slice.idr, "nal_unit_type {nal} should be IDR");
        }
    }
}
