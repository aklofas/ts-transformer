//! H.264 slice header (light subset) parser.
//!
//! Light = `first_mb_in_slice`, `slice_type` (mod 5), `pic_parameter_set_id`,
//! optionally `frame_num` if SPS context is supplied, plus the `idr` flag
//! derived from the NAL header. Sufficient for keyframe detection / frame-type
//! classification in receivers + analytics tools without parsing through to
//! the slice data offset.
//!
//! Per H.264 §7.3.3.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use crate::codec::h264::model::H264Sps;
use alloc::vec::Vec;

crate::binding_record! {
    /// Light-weight H.264 slice header — fields required for keyframe detection
    /// and frame-type classification without walking into slice data.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct H264SliceHeaderLight {
        /// True when `first_mb_in_slice == 0` — i.e., this slice covers the first
        /// macroblock in the picture, marking the start of a new frame.
        pub first_in_pic: bool,
        /// Slice type — normalised via `slice_type % 5` per H.264 §7.4.3.
        pub slice_type: H264SliceType,
        /// `pic_parameter_set_id` — links this slice to a PPS.
        pub pps_id: u8,
        /// `frame_num` as read from the bitstream using the bit width
        /// `log2_max_frame_num_minus4 + 4` from the referenced SPS; absent when
        /// no SPS context was supplied to the parser.
        @[custom] pub frame_num: Option<u32>,
        /// True when `nal_unit_type == 5` (IDR slice).
        pub idr: bool,
        /// The raw RBSP bytes as supplied by the caller — preserved for
        /// downstream parsers that need fields beyond what this light subset
        /// extracts.
        pub raw_rbsp: Vec<u8>,
    }
}

/// H.264 slice type, normalised via `slice_type % 5` per H.264 §7.4.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum H264SliceType {
    /// P slice — predicted.
    P,
    /// B slice — bidirectionally predicted.
    B,
    /// I slice — intra-coded.
    I,
    /// SP slice — switching P.
    Sp,
    /// SI slice — switching I.
    Si,
}

impl H264SliceType {
    fn from_raw(v: u32) -> Self {
        match v % 5 {
            0 => Self::P,
            1 => Self::B,
            2 => Self::I,
            3 => Self::Sp,
            4 => Self::Si,
            _ => unreachable!("v % 5 is bounded to 0..=4; covered by prior arms"),
        }
    }
}

/// Parse a light H.264 slice header from a RBSP byte slice.
///
/// # Input contract
///
/// `rbsp` is the raw RBSP body of a slice NAL — Annex B start code stripped,
/// NAL header byte stripped, emulation-prevention bytes preserved (matches
/// `NalUnit::H264 { payload }`). The light parser reads only
/// `first_mb_in_slice`, `slice_type`, and `pic_parameter_set_id` at minimum,
/// then optionally `frame_num` when an SPS context is available.
///
/// `nal_unit_type` is the 5-bit NAL type from the NAL header (`& 0x1F`) — used
/// to derive `idr` (`== 5`) without re-parsing.
///
/// # Errors
///
/// Returns [`CodecParseError::TruncatedRbsp`] when `rbsp` is too short to hold
/// even the mandatory ue(v) fields. Returns [`CodecParseError::InvalidGolomb`]
/// on a malformed Exp-Golomb codeword. Returns [`CodecParseError::ReservedValue`]
/// when `pic_parameter_set_id` is out of u8 range.
pub fn parse_slice_header_light(
    rbsp: &[u8],
    sps: Option<&H264Sps>,
    nal_unit_type: u8,
) -> Result<H264SliceHeaderLight, CodecParseError> {
    let mut br = BitReader::new(rbsp);
    let first_mb_in_slice = br.read_ue()?;
    let slice_type_raw = br.read_ue()?;
    let slice_type = H264SliceType::from_raw(slice_type_raw);
    let pps_id_u32 = br.read_ue()?;
    let pps_id: u8 = u8::try_from(pps_id_u32).map_err(|_| CodecParseError::ReservedValue {
        field: "pic_parameter_set_id",
        value: pps_id_u32,
    })?;

    let frame_num = if let Some(sps) = sps {
        // H.264 §7.4.3: frame_num is u(v) where the bit width equals
        // log2_max_frame_num_minus4 + 4 (from the referenced SPS).
        let bits = (sps.log2_max_frame_num_minus4 as u32) + 4;
        Some(br.read_u(bits)?)
    } else {
        None
    };

    let idr = nal_unit_type == 5;

    Ok(H264SliceHeaderLight {
        first_in_pic: first_mb_in_slice == 0,
        slice_type,
        pps_id,
        frame_num,
        idr,
        raw_rbsp: rbsp.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_idr_slice_header_no_sps() -> Vec<u8> {
        // first_mb_in_slice = 0 (ue: '1')               → 1 bit
        // slice_type = 7 (mod 5 = 2 = I) (ue: '0001000') → 7 bits
        // pic_parameter_set_id = 0 (ue: '1')             → 1 bit
        // Total 9 bits; pad to 2 bytes:
        //   byte 0: 1_0001000 = 0x88
        //   byte 1: 1_xxxxxxx = 0x80
        vec![0x88, 0x80]
    }

    #[test]
    fn parse_minimal_idr_slice_header_no_sps() {
        let rbsp = synth_idr_slice_header_no_sps();
        let slice = parse_slice_header_light(&rbsp, None, 5).unwrap();
        assert!(slice.first_in_pic);
        assert_eq!(slice.slice_type, H264SliceType::I);
        assert_eq!(slice.pps_id, 0);
        assert_eq!(slice.frame_num, None);
        assert!(slice.idr);
    }

    #[test]
    fn parse_non_idr_marks_idr_false() {
        let rbsp = synth_idr_slice_header_no_sps();
        let slice = parse_slice_header_light(&rbsp, None, 1).unwrap();
        assert!(!slice.idr);
    }

    #[test]
    fn slice_type_modulo_5_normalization() {
        // slice_type = 5 → mod 5 = 0 → P
        // ue(5) = '00110' (2 leading zeros + marker bit + 2-bit suffix '10')
        // first_mb_in_slice = 0 ('1'), slice_type = 5 ('00110'), pps_id = 0 ('1')
        // 7 bits total: 1_00110_1 = 1001_1010 = 0x9A (final bit is alignment pad)
        let rbsp = vec![0x9A];
        let slice = parse_slice_header_light(&rbsp, None, 1).unwrap();
        assert_eq!(slice.slice_type, H264SliceType::P);
    }

    #[test]
    fn truncated_rbsp_returns_error() {
        let rbsp = vec![]; // empty — TruncatedRbsp on first read_ue()
        let err = parse_slice_header_light(&rbsp, None, 1).unwrap_err();
        assert!(matches!(err, CodecParseError::TruncatedRbsp { .. }));
    }
}
