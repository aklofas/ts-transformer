//! `vui_parameters()` parser per H.266 V4 §7.3.2.5 / §E.2.1.
//!
//! H.266 VUI differs from H.265 in two key ways:
//!   - Four source flags precede aspect_ratio_info (H.265 has none).
//!   - Timing (num_units_in_tick / time_scale) is in §7.3.5.1
//!     general_timing_hrd_parameters(), NOT in VUI. So this function
//!     returns only `Option<ColorInfo>`.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use crate::codec::{
    ColorInfo, ColourPrimaries, MatrixCoefficients, Rational, TransferCharacteristics,
    aspect_ratio_idc_to_sar, read_h273_colour,
};

/// Parse H.274 V4 §7.2 `vui_parameters( payloadSize )`, returning `ColorInfo`
/// when at least one color/SAR field was present. Frame rate is NOT recovered
/// here — it lives in `general_timing_hrd_parameters()` (H.266 V4 §7.3.5.1).
///
/// Despite the spec name's `payloadSize` argument, H.274 §7.2 has no
/// payloadSize-dependent fields. The tail (`vui_reserved_payload_extension_data`,
/// `vui_payload_bit_equal_to_one`, zero-pad-to-byte-align) belongs to the
/// `vui_payload(payloadSize)` wrapper in H.266 §7.3.2.21 — that's a caller
/// concern (handled in `sps.rs`), not a `vui_parameters()` concern.
pub(super) fn parse_h266_vui(br: &mut BitReader<'_>) -> Result<Option<ColorInfo>, CodecParseError> {
    // §7.3.2.5 — four source flags (H.266-specific; precede aspect_ratio).
    let vui_progressive_source_flag = br.read_bool()?; // u(1)
    let vui_interlaced_source_flag = br.read_bool()?; // u(1)
    let _vui_non_packed_constraint_flag = br.read_bool()?; // u(1)
    let _vui_non_projected_constraint_flag = br.read_bool()?; // u(1)

    // §7.3.2.5 — aspect ratio.
    let vui_aspect_ratio_info_present_flag = br.read_bool()?; // u(1)
    let mut sample_aspect_ratio = None;
    if vui_aspect_ratio_info_present_flag {
        // H.266 adds vui_aspect_ratio_constant_flag before aspect_ratio_idc.
        let _vui_aspect_ratio_constant_flag = br.read_bool()?; // u(1)
        let aspect_ratio_idc = br.read_u(8)? as u8; // u(8)
        sample_aspect_ratio = aspect_ratio_idc_to_sar(aspect_ratio_idc);
        if aspect_ratio_idc == 255 {
            // EXTENDED_SAR: explicit numerator / denominator. H.274 §7.3: a
            // zero sar_width / sar_height means "unspecified" — never
            // surface `Rational { den: 0 }` (mirrors h264/decode.rs).
            let w = br.read_u(16)?; // u(16) sar_width
            let h = br.read_u(16)?; // u(16) sar_height
            sample_aspect_ratio = if w != 0 && h != 0 {
                Some(Rational { num: w, den: h })
            } else {
                None
            };
        }
    }

    // §7.3.2.5 — overscan info.
    let vui_overscan_info_present_flag = br.read_bool()?; // u(1)
    if vui_overscan_info_present_flag {
        let _vui_overscan_appropriate_flag = br.read_bool()?; // u(1)
    }

    // §7.3.2.5 — colour description per H.273.
    let vui_colour_description_present_flag = br.read_bool()?; // u(1)
    let mut full_range = false;
    let mut primaries = ColourPrimaries::Unspecified;
    let mut transfer = TransferCharacteristics::Unspecified;
    let mut matrix = MatrixCoefficients::Unspecified;
    if vui_colour_description_present_flag {
        (primaries, transfer, matrix) = read_h273_colour(br)?;
        full_range = br.read_bool()?; // u(1) vui_full_range_flag
    }

    // §H.274 7.3 (p. 20) — chroma sample location.
    let vui_chroma_loc_info_present_flag = br.read_bool()?; // u(1)
    let mut chroma_loc = None;
    if vui_chroma_loc_info_present_flag {
        if vui_progressive_source_flag && !vui_interlaced_source_flag {
            // Progressive: single chroma_sample_loc_type_frame ue(v).
            // H.274 §7.3 (p. 20): shall be in range 0..=6 inclusive.
            let v = br.read_ue_max("vui_chroma_sample_loc_type_frame", 6)?;
            chroma_loc = Some(v as u8);
        } else {
            // Interlaced (or unspecified): top_field + bottom_field.
            // H.274 §7.3 (p. 20): both shall be in range 0..=6 inclusive.
            let top = br.read_ue_max("vui_chroma_sample_loc_type_top_field", 6)?;
            // ColorInfo::chroma_loc surfaces the top-field value (per the
            // type's single-byte design); bottom_field is consumed for
            // bit-stream correctness but currently not surfaced. See
            // H264-04 / H274-04 for the data-model gap.
            let _bottom = br.read_ue_max("vui_chroma_sample_loc_type_bottom_field", 6)?;
            chroma_loc = Some(top as u8);
        }
    }

    // Build ColorInfo when any color-related field was present.
    let color = if vui_colour_description_present_flag
        || vui_chroma_loc_info_present_flag
        || vui_aspect_ratio_info_present_flag
    {
        Some(ColorInfo {
            primaries,
            transfer,
            matrix,
            full_range,
            chroma_loc,
            sample_aspect_ratio,
        })
    } else {
        None
    };

    Ok(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::bitreader::BitReader;
    use crate::codec::test_util::BitWriter;

    /// Builds the minimum VUI bit stream that exercises the chroma_loc
    /// branch. Sequence per H.274 §7.2:
    ///   - 4 source flags (progressive=1, interlaced=0, non_packed=0, non_projected=0)
    ///   - aspect_ratio_info_present = 0
    ///   - overscan_info_present = 0
    ///   - colour_description_present = 0
    ///   - chroma_loc_info_present = 1
    ///   - chroma_sample_loc_type_frame ue(v) = `frame_loc`
    ///
    /// With progressive=1 && interlaced=0 the parser reads the single
    /// `chroma_sample_loc_type_frame` ue(v) (not the top+bottom pair).
    fn craft_vui_with_chroma_frame_loc(frame_loc_ue: u32) -> Vec<u8> {
        struct Bw {
            bytes: Vec<u8>,
            pos: u32,
        }
        impl Bw {
            fn new() -> Self {
                Self {
                    bytes: vec![],
                    pos: 0,
                }
            }
            fn u(&mut self, value: u32, n: u32) {
                for i in (0..n).rev() {
                    let bit = ((value >> i) & 1) as u8;
                    let byte_idx = (self.pos / 8) as usize;
                    let bit_in_byte = 7 - (self.pos % 8);
                    if byte_idx == self.bytes.len() {
                        self.bytes.push(0);
                    }
                    self.bytes[byte_idx] |= bit << bit_in_byte;
                    self.pos += 1;
                }
            }
            fn ue(&mut self, value: u32) {
                let v = value + 1;
                let leading_zeros = 31 - v.leading_zeros();
                for _ in 0..leading_zeros {
                    self.u(0, 1);
                }
                self.u(v, leading_zeros + 1);
            }
        }
        let mut bw = Bw::new();
        bw.u(1, 1); // vui_progressive_source_flag
        bw.u(0, 1); // vui_interlaced_source_flag
        bw.u(0, 1); // vui_non_packed_constraint_flag
        bw.u(0, 1); // vui_non_projected_constraint_flag
        bw.u(0, 1); // vui_aspect_ratio_info_present_flag
        bw.u(0, 1); // vui_overscan_info_present_flag
        bw.u(0, 1); // vui_colour_description_present_flag
        bw.u(1, 1); // vui_chroma_loc_info_present_flag
        bw.ue(frame_loc_ue);
        bw.bytes
    }

    /// H.274 §7.3 (p. 20): `vui_chroma_sample_loc_type_frame` shall be in
    /// 0..=6. Value 7 must be rejected.
    #[test]
    fn chroma_sample_loc_type_7_rejected() {
        let bytes = craft_vui_with_chroma_frame_loc(7);
        let mut br = BitReader::new(&bytes);
        match parse_h266_vui(&mut br) {
            Err(CodecParseError::ReservedValue { field, value }) => {
                assert!(
                    field.starts_with("vui_chroma_sample_loc_type"),
                    "field = {field:?}"
                );
                assert_eq!(value, 7);
            }
            other => panic!("expected ReservedValue, got {other:?}"),
        }
    }

    /// Adversarial: ue(v) = 256 currently silent-truncates to 0 via `as u8`
    /// (a valid value!), masking a malformed bitstream. Post-fix, the range
    /// check fires BEFORE the u8 cast, so 256 is rejected.
    #[test]
    fn chroma_sample_loc_type_256_rejected_via_ue_truncate_guard() {
        let bytes = craft_vui_with_chroma_frame_loc(256);
        let mut br = BitReader::new(&bytes);
        match parse_h266_vui(&mut br) {
            Err(CodecParseError::ReservedValue { field, value }) => {
                assert!(
                    field.starts_with("vui_chroma_sample_loc_type"),
                    "field = {field:?}"
                );
                assert_eq!(value, 256);
            }
            other => panic!(
                "expected ReservedValue (256 must NOT silent-truncate to valid 0), got {other:?}"
            ),
        }
    }

    /// H.274 §7.2 VUI carrying only an Extended_SAR aspect ratio.
    fn vui_with_extended_sar(w: u32, h: u32) -> Vec<u8> {
        let mut bw = BitWriter::new();
        bw.write(1, 1); // vui_progressive_source_flag
        bw.write(0, 1); // vui_interlaced_source_flag
        bw.write(0, 1); // vui_non_packed_constraint_flag
        bw.write(0, 1); // vui_non_projected_constraint_flag
        bw.write(1, 1); // vui_aspect_ratio_info_present_flag
        bw.write(0, 1); // vui_aspect_ratio_constant_flag
        bw.write(255, 8); // aspect_ratio_idc = EXTENDED_SAR
        bw.write(u64::from(w), 16); // sar_width
        bw.write(u64::from(h), 16); // sar_height
        bw.write(0, 1); // vui_overscan_info_present_flag
        bw.write(0, 1); // vui_colour_description_present_flag
        bw.write(0, 1); // vui_chroma_loc_info_present_flag
        bw.end_rbsp();
        bw.bytes
    }

    /// CORR-16: zero `sar_height` must read as "unspecified", never
    /// `Rational { den: 0 }`.
    #[test]
    fn extended_sar_with_zero_height_is_unspecified_not_div_by_zero() {
        let bytes = vui_with_extended_sar(16, 0);
        let mut br = BitReader::new(&bytes);
        let color = parse_h266_vui(&mut br).unwrap().expect("ColorInfo");
        assert_eq!(color.sample_aspect_ratio, None);
    }

    #[test]
    fn extended_sar_with_nonzero_dims_round_trips() {
        let bytes = vui_with_extended_sar(16, 11);
        let mut br = BitReader::new(&bytes);
        let color = parse_h266_vui(&mut br).unwrap().expect("ColorInfo");
        assert_eq!(
            color.sample_aspect_ratio,
            Some(Rational { num: 16, den: 11 })
        );
    }
}
