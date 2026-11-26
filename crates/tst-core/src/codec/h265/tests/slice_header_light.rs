//! Light slice segment header tests that need an SPS context.

use super::parse_sps::{SynthSpsOpts, build_synthetic_sps_with};
use crate::codec::CodecParseError;
use crate::codec::h265::{H265Sps, parse_slice_header_light, parse_sps};
use crate::codec::test_util::escape_emulation;

/// A synthetic SPS whose `slice_pic_order_cnt_lsb` is 4 bits wide.
fn sps(separate_colour_planes: bool) -> H265Sps {
    let mut sps = parse_sps(&escape_emulation(&build_synthetic_sps_with(
        |bw| bw.write_ue(0),
        SynthSpsOpts {
            separate_colour_planes,
            ..Default::default()
        },
    )))
    .unwrap();
    assert_eq!(sps.separate_colour_plane, separate_colour_planes);
    sps.log2_max_pic_order_cnt_lsb_minus4 = 0;
    sps
}

/// H.265 §7.3.6.1: a continuation segment carries `dependent_slice_segment_flag`
/// and `slice_segment_address` (a PPS-sized field) before its POC, so the four
/// bits after `slice_pic_parameter_set_id` (`1010` here) are not a POC.
#[test]
fn continuation_segment_reports_no_poc() {
    let sh = parse_slice_header_light(&[0b0110_1010, 0x80], Some(&sps(false)), 1).unwrap();
    assert!(!sh.first_in_pic);
    assert_eq!(sh.pic_order_cnt_lsb, None);
}

/// An IDR picture's POC LSB is 0 (§8.3.1) in every one of its segments.
#[test]
fn an_idr_continuation_segment_keeps_poc_zero() {
    let sh = parse_slice_header_light(&[0b0010_1010, 0x80], Some(&sps(false)), 19).unwrap();
    assert!(!sh.first_in_pic);
    assert_eq!(sh.pic_order_cnt_lsb, Some(0));
}

/// §7.3.6.1: `colour_plane_id u(2)` precedes `slice_pic_order_cnt_lsb` when the
/// SPS codes the colour planes separately. `D4 B0` reads the first-segment flag,
/// `slice_pic_parameter_set_id = 0`, `slice_type = 1`, then `10` and `0101`:
/// POC 5 behind a colour plane id, or 9 when the next four bits are the POC.
#[test]
fn first_segment_skips_colour_plane_id_before_poc() {
    let sh = parse_slice_header_light(&[0xD4, 0xB0], Some(&sps(false)), 1).unwrap();
    assert_eq!(sh.pic_order_cnt_lsb, Some(9));
    let sh = parse_slice_header_light(&[0xD4, 0xB0], Some(&sps(true)), 1).unwrap();
    assert_eq!(sh.pic_order_cnt_lsb, Some(5));
}

/// `nal_unit_type` is a 6-bit field: a value above 63 is refused before any bit
/// of the slice header is read.
#[test]
fn a_nal_unit_type_over_63_is_refused() {
    let rbsp = [0xD8]; // first segment, pps 0, slice_type 2 (I)
    assert!(parse_slice_header_light(&rbsp, None, 63).is_ok());
    for bytes in [&rbsp[..], &[]] {
        match parse_slice_header_light(bytes, None, 64) {
            Err(CodecParseError::ReservedValue { field, value }) => {
                assert_eq!((field, value), ("nal_unit_type", 64))
            }
            other => panic!("64 must be refused, got {other:?}"),
        }
    }
}
