//! H.266 / VVC slice header (light subset) parser.
//!
//! Per H.266 V4 §7.3.7.1.

use crate::codec::bitreader::BitReader;
use crate::codec::h266::sps::H266Sps;
use crate::codec::nal::is_idr;
use crate::codec::{CodecParseError, VideoCodec};
use alloc::vec::Vec;

crate::binding_record! {
    /// Light-weight H.266 slice header — fields required for keyframe detection
    /// and frame-type classification, extracted without walking into slice data.
    ///
    /// Known limitations: H.266 slice headers embed `picture_header_rbsp()`
    /// inline when `sh_picture_header_in_slice_header_flag == 1`. The length of
    /// `picture_header_rbsp()` is determined by SPS / PPS context fields
    /// (`sps_subpic_info_present_flag`, `pps_log2_ctu_size_minus5`, etc.) that
    /// this light parser does not carry. As a result `slice_type` is always the
    /// intra slice type and `pps_id` is always 0, both as sentinels; accurate
    /// extraction needs SPS-driven walking through `picture_header_rbsp()` and is
    /// deferred. `idr` is derived solely from the NAL unit type (7 = IDR_W_RADL,
    /// 8 = IDR_N_LP). `first_in_pic` is `sh_picture_header_in_slice_header_flag`,
    /// the first bit of the slice header. `pic_order_cnt_lsb` is always absent:
    /// VVC codes the POC in the picture header for every picture, IDR included
    /// (H.266 §7.3.2.8), and this parser does not read the picture header.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct H266SliceHeaderLight {
        /// `sh_picture_header_in_slice_header_flag`. `true`: the picture header
        /// is in this slice header, so the picture is this one VCL NAL unit.
        /// `false`: a separate picture header NAL unit carries the header; the
        /// first slice after it reads `false` too, so `false` does not mean
        /// "not the first slice of the picture".
        pub first_in_pic: bool,
        /// Slice type. Always the intra slice type, as a sentinel — see the
        /// struct-level limitations. Accurate extraction requires walking through
        /// `picture_header_rbsp()`, whose length depends on SPS / PPS context
        /// fields that this light parser does not carry; deferred pending a
        /// context-tracking H.266 parser.
        pub slice_type: H266SliceType,
        /// PPS id. Always 0, as a sentinel — see `slice_type` for the same reason
        /// and deferral note.
        pub pps_id: u8,
        /// Always absent: VVC codes `ph_pic_order_cnt_lsb` in the picture header
        /// for every picture, IDR included (H.266 §7.3.2.8), and this parser
        /// does not read the picture header.
        pub pic_order_cnt_lsb: Option<u16>,
        /// True when `nal_unit_type` is IDR_W_RADL (7) or IDR_N_LP (8) per H.266
        /// V4 Table 5. This field is accurate.
        pub idr: bool,
        /// The raw RBSP bytes as supplied to the parser — preserved for
        /// downstream parsers that need fields beyond what this light subset
        /// extracts.
        pub raw_rbsp: Vec<u8>,
    }
}

/// H.266 slice type (B / P / I). Only three values are defined by the spec
/// (H.266 V4 §7.4.8 Table 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum H266SliceType {
    /// B slice — bidirectionally predicted.
    B,
    /// P slice — predicted.
    P,
    /// I slice — intra-coded.
    I,
}

/// Parse a light H.266 slice header from a RBSP byte slice.
///
/// # Input contract
///
/// `rbsp` is the raw RBSP body of a slice NAL — Annex B start code stripped,
/// NAL header (2 bytes for H.266) stripped, emulation-prevention bytes
/// preserved (matches `NalUnit::H266 { payload }`). The light parser reads
/// only `sh_picture_header_in_slice_header_flag` (the first bit) and derives
/// `idr` from `nal_unit_type`.
///
/// `_sps` is accepted and ignored: the light parser reads no SPS field.
///
/// `nal_unit_type` is the 5-bit NAL unit type from the NAL header byte pair
/// — used to derive `idr`.
///
/// # Known limitations
///
/// `slice_type` and `pps_id` are returned as **sentinels** (always `I` and
/// `0` respectively). Accurate extraction requires walking through
/// `picture_header_rbsp()`, whose length is governed by SPS / PPS context
/// fields that this light parser does not carry.
///
/// `pic_order_cnt_lsb` is always `None`: the POC is coded in the picture
/// header (H.266 §7.3.2.8), for IDR pictures too.
///
/// # Errors
///
/// Returns [`CodecParseError::ReservedValue`] (`field: "nal_unit_type"`) for a
/// `nal_unit_type` above 31, before any bit is read, and
/// [`CodecParseError::TruncatedRbsp`] when `rbsp` is empty (the first bit
/// cannot be read).
pub fn parse_slice_header_light(
    rbsp: &[u8],
    _sps: Option<&H266Sps>,
    nal_unit_type: u8,
) -> Result<H266SliceHeaderLight, CodecParseError> {
    // nal_unit_type is u(5) (H.266 §7.3.1.2).
    if nal_unit_type > 31 {
        return Err(CodecParseError::ReservedValue {
            field: "nal_unit_type",
            value: nal_unit_type.into(),
        });
    }
    let mut br = BitReader::new(rbsp);

    // sh_picture_header_in_slice_header_flag u(1) — H.266 §7.3.7.
    // When 1, a picture_header_rbsp() follows whose length is SPS-driven.
    // For light parsing we only need this single flag to determine first_in_pic.
    let picture_header_in_slice_header_flag = br.read_u(1)? == 1;
    let first_in_pic = picture_header_in_slice_header_flag;

    // slice_type and pps_id require walking past picture_header_rbsp() whose
    // length depends on SPS/PPS context we don't carry. Return sentinels.
    // See struct-level "Known limitations" for details.
    let slice_type = H266SliceType::I;
    let pps_id = 0u8;

    // ph_pic_order_cnt_lsb is in the picture header for every picture, IDR
    // included (H.266 §7.3.2.8); this parser does not read it.
    let pic_order_cnt_lsb = None;

    Ok(H266SliceHeaderLight {
        first_in_pic,
        slice_type,
        pps_id,
        pic_order_cnt_lsb,
        idr: is_idr(VideoCodec::H266, nal_unit_type),
        raw_rbsp: rbsp.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_w_radl_marks_idr_true() {
        // NAL type 7 = IDR_W_RADL.
        // RBSP byte 0x80 = 0b1000_0000 → picture_header_in_slice_header_flag=1.
        let rbsp = vec![0x80];
        let slice = parse_slice_header_light(&rbsp, None, 7).unwrap();
        assert!(slice.idr, "IDR_W_RADL (7) should be IDR");
        assert!(slice.first_in_pic, "picture_header_in_slice_header_flag=1");
        assert_eq!(slice.slice_type, H266SliceType::I, "sentinel value");
        assert_eq!(slice.pps_id, 0, "sentinel value");
        assert_eq!(
            slice.pic_order_cnt_lsb, None,
            "the POC is in the picture header"
        );
    }

    #[test]
    fn idr_n_lp_marks_idr_true() {
        // NAL type 8 = IDR_N_LP.
        let rbsp = vec![0x80];
        let slice = parse_slice_header_light(&rbsp, None, 8).unwrap();
        assert!(slice.idr, "IDR_N_LP (8) should be IDR");
        assert_eq!(slice.pic_order_cnt_lsb, None);
    }

    #[test]
    fn trail_nal_does_not_mark_idr() {
        // NAL type 0 = TRAIL — not IDR.
        let rbsp = vec![0x80];
        let slice = parse_slice_header_light(&rbsp, None, 0).unwrap();
        assert!(!slice.idr, "TRAIL (0) should not be IDR");
        assert_eq!(
            slice.pic_order_cnt_lsb, None,
            "the POC is in the picture header"
        );
    }

    /// VVC codes the POC in the picture header for every picture, IDR
    /// included (H.266 §7.3.2.8): no slice reports one.
    #[test]
    fn idr_slice_reports_no_implicit_poc() {
        for nal_unit_type in [7u8, 8] {
            let slice = parse_slice_header_light(&[0x80], None, nal_unit_type).unwrap();
            assert_eq!(slice.pic_order_cnt_lsb, None, "type {nal_unit_type}");
        }
    }

    /// `first_in_pic` is `sh_picture_header_in_slice_header_flag`: a picture
    /// whose header is a separate PH NAL unit reads `false` on its first slice.
    #[test]
    fn separate_ph_first_slice_reports_flag_zero() {
        let slice = parse_slice_header_light(&[0x00, 0x80], None, 1).unwrap();
        assert!(!slice.first_in_pic);
        let slice = parse_slice_header_light(&[0x80], None, 1).unwrap();
        assert!(slice.first_in_pic);
    }

    /// `nal_unit_type` is `u(5)` (H.266 §7.3.1.2): 32 and above are refused
    /// before any bit is read.
    #[test]
    fn a_nal_unit_type_over_31_is_refused() {
        assert!(parse_slice_header_light(&[0x80], None, 31).is_ok());
        let empty: &[u8] = &[];
        for (rbsp, nal_unit_type) in [(&[0x80][..], 32u8), (empty, 255)] {
            match parse_slice_header_light(rbsp, None, nal_unit_type) {
                Err(CodecParseError::ReservedValue {
                    field: "nal_unit_type",
                    value,
                }) => assert_eq!(value, u32::from(nal_unit_type)),
                other => panic!("type {nal_unit_type}: {other:?}"),
            }
        }
    }
}
