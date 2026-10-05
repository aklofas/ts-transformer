//! H.266 / VVC slice header (light subset) parser.
//!
//! Per H.266 V4 §7.3.7.1.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use crate::codec::h266::sps::H266Sps;
use alloc::vec::Vec;

/// Light-weight H.266 slice header — fields required for keyframe detection
/// and frame-type classification, extracted without walking into slice data.
///
/// # Known limitations
///
/// H.266 slice headers embed `picture_header_rbsp()` inline when
/// `picture_header_in_slice_header_flag == 1`. The length of
/// `picture_header_rbsp()` is determined by SPS / PPS context fields
/// (`sps_subpic_info_present_flag`, `pps_log2_ctu_size_minus5`, etc.) that
/// this light parser does not carry. As a result:
///
/// - **`slice_type`** always returns [`H266SliceType::I`] as a sentinel.
///   Accurate extraction requires SPS-driven walking through
///   `picture_header_rbsp()` — deferred.
/// - **`pps_id`** always returns `0` as a sentinel for the same reason.
/// - **`idr`** is accurate: derived solely from the NAL unit type (7 =
///   IDR_W_RADL, 8 = IDR_N_LP).
/// - **`first_in_pic`** is accurate: derived from
///   `picture_header_in_slice_header_flag` (the first bit of the slice
///   header), which is present before the deferred region.
/// - **`pic_order_cnt_lsb`** is `Some(0)` for IDR slices (implicit per spec)
///   and `None` otherwise (accurate extraction requires the
///   `sps_log2_max_pic_order_cnt_lsb_minus4` field, which is deferred).
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct H266SliceHeaderLight {
    /// True when `picture_header_in_slice_header_flag == 1` — the picture
    /// header is embedded in this slice, marking the start of a new picture.
    pub first_in_pic: bool,
    /// Slice type. **Always returns [`H266SliceType::I`] as a sentinel** —
    /// see struct-level "Known limitations".
    pub slice_type: H266SliceType,
    /// PPS id. **Always returns `0` as a sentinel** — see struct-level
    /// "Known limitations".
    pub pps_id: u8,
    /// `slice_pic_order_cnt_lsb`. `Some(0)` for IDR slices (implicit per
    /// spec); `None` for non-IDR slices where SPS context is required to
    /// determine the bit width.
    pub pic_order_cnt_lsb: Option<u16>,
    /// True when `nal_unit_type` is IDR_W_RADL (7) or IDR_N_LP (8).
    pub idr: bool,
    /// Raw RBSP bytes as supplied by the caller — preserved for downstream
    /// parsers that need fields beyond what this light subset extracts.
    pub raw_rbsp: Vec<u8>,
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

/// Returns `true` when `nal_unit_type` is an IDR NAL: IDR_W_RADL (7) or
/// IDR_N_LP (8) per H.266 V4 Table 5.
const fn is_idr_nal(nal_unit_type: u8) -> bool {
    matches!(nal_unit_type, 7 | 8)
}

/// Parse a light H.266 slice header from a RBSP byte slice.
///
/// # Input contract
///
/// `rbsp` is the raw RBSP body of a slice NAL — Annex B start code stripped,
/// NAL header (2 bytes for H.266) stripped, emulation-prevention bytes
/// preserved (matches `NalUnit::H266 { payload }`). The light parser reads
/// only `picture_header_in_slice_header_flag` (the first bit) and derives
/// `idr` from `nal_unit_type`.
///
/// `nal_unit_type` is the 6-bit NAL unit type from the NAL header byte pair
/// — used to derive `idr` and `pic_order_cnt_lsb` for IDR slices.
///
/// # Known limitations
///
/// `slice_type` and `pps_id` are returned as **sentinels** (always `I` and
/// `0` respectively). Accurate extraction requires walking through
/// `picture_header_rbsp()`, whose length is governed by SPS / PPS context
/// fields that this light parser does not carry.
///
/// `pic_order_cnt_lsb` is `Some(0)` for IDR slices (implicit per H.266 spec)
/// and `None` for non-IDR slices; the SPS-driven bit-width extraction is part
/// of the deferred work above.
///
/// # Errors
///
/// Returns [`CodecParseError::TruncatedRbsp`] when `rbsp` is empty (the first
/// bit cannot be read).
pub fn parse_slice_header_light(
    rbsp: &[u8],
    _sps: Option<&H266Sps>,
    nal_unit_type: u8,
) -> Result<H266SliceHeaderLight, CodecParseError> {
    let mut br = BitReader::new(rbsp);

    // picture_header_in_slice_header_flag u(1) — H.266 §7.3.7.1.
    // When 1, a picture_header_rbsp() follows whose length is SPS-driven.
    // For light parsing we only need this single flag to determine first_in_pic.
    let picture_header_in_slice_header_flag = br.read_u(1)? == 1;
    let first_in_pic = picture_header_in_slice_header_flag;

    // slice_type and pps_id require walking past picture_header_rbsp() whose
    // length depends on SPS/PPS context we don't carry. Return sentinels.
    // See struct-level "Known limitations" for details.
    let slice_type = H266SliceType::I;
    let pps_id = 0u8;

    // pic_order_cnt_lsb: for IDR slices the value is implicitly 0 per spec.
    // For non-IDR slices the bit width comes from
    // sps_log2_max_pic_order_cnt_lsb_minus4, which requires the deferred
    // SPS-driven parser path — return None.
    let pic_order_cnt_lsb = if is_idr_nal(nal_unit_type) {
        Some(0)
    } else {
        None
    };

    Ok(H266SliceHeaderLight {
        first_in_pic,
        slice_type,
        pps_id,
        pic_order_cnt_lsb,
        idr: is_idr_nal(nal_unit_type),
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
        assert_eq!(slice.pic_order_cnt_lsb, Some(0), "IDR implicit POC=0");
    }

    #[test]
    fn idr_n_lp_marks_idr_true() {
        // NAL type 8 = IDR_N_LP.
        let rbsp = vec![0x80];
        let slice = parse_slice_header_light(&rbsp, None, 8).unwrap();
        assert!(slice.idr, "IDR_N_LP (8) should be IDR");
        assert_eq!(slice.pic_order_cnt_lsb, Some(0));
    }

    #[test]
    fn trail_nal_does_not_mark_idr() {
        // NAL type 0 = TRAIL — not IDR.
        let rbsp = vec![0x80];
        let slice = parse_slice_header_light(&rbsp, None, 0).unwrap();
        assert!(!slice.idr, "TRAIL (0) should not be IDR");
        assert_eq!(slice.pic_order_cnt_lsb, None, "non-IDR POC requires SPS");
    }
}
