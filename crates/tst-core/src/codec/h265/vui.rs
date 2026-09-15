//! `vui_parameters()` parser per H.265 §E.2.1. Only the fields surfaced
//! on [`crate::codec::ColorInfo`] and frame_rate are decoded; the rest
//! are skipped.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use crate::codec::{
    ColorInfo, ColourPrimaries, MatrixCoefficients, Rational, TransferCharacteristics,
    aspect_ratio_idc_to_sar, read_h273_colour,
};

pub(crate) struct VuiOut {
    pub frame_rate: Option<Rational>,
    pub color: Option<ColorInfo>,
}

pub(crate) fn parse(
    br: &mut BitReader<'_>,
    _max_sub_layers_minus1: u8,
) -> Result<VuiOut, CodecParseError> {
    let aspect_ratio_info_present_flag = br.read_bool()?;
    let mut sample_aspect_ratio = None;
    if aspect_ratio_info_present_flag {
        let aspect_ratio_idc = br.read_u(8)? as u8;
        sample_aspect_ratio = aspect_ratio_idc_to_sar(aspect_ratio_idc);
        if aspect_ratio_idc == 255 {
            let w = br.read_u(16)?;
            let h = br.read_u(16)?;
            // H.265 §E.3.1: a zero sar_width / sar_height means the sample
            // aspect ratio is unspecified. Never surface `Rational { den: 0 }`
            // (÷0 hazard for consumers) — mirrors h264/decode.rs.
            sample_aspect_ratio = if w != 0 && h != 0 {
                Some(Rational { num: w, den: h })
            } else {
                None
            };
        }
    }

    let overscan_info_present_flag = br.read_bool()?;
    if overscan_info_present_flag {
        br.skip(1)?;
    }

    let video_signal_type_present_flag = br.read_bool()?;
    let mut full_range = false;
    let mut primaries = ColourPrimaries::Unspecified;
    let mut transfer = TransferCharacteristics::Unspecified;
    let mut matrix = MatrixCoefficients::Unspecified;
    if video_signal_type_present_flag {
        let _video_format = br.read_u(3)?;
        full_range = br.read_bool()?;
        let colour_description_present_flag = br.read_bool()?;
        if colour_description_present_flag {
            (primaries, transfer, matrix) = read_h273_colour(br)?;
        }
    }

    let chroma_loc_info_present_flag = br.read_bool()?;
    let mut chroma_loc = None;
    if chroma_loc_info_present_flag {
        let top = br.read_ue_max("chroma_sample_loc_type_top_field", 5)? as u8;
        // H.265 Table E.1: both fields are bounded 0..=5.
        let _bottom = br.read_ue_max("chroma_sample_loc_type_bottom_field", 5)?;
        chroma_loc = Some(top);
    }

    let _neutral_chroma_indication_flag = br.read_bool()?;
    let _field_seq_flag = br.read_bool()?;
    let _frame_field_info_present_flag = br.read_bool()?;

    let default_display_window_flag = br.read_bool()?;
    if default_display_window_flag {
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
    }

    let vui_timing_info_present_flag = br.read_bool()?;
    let mut frame_rate = None;
    if vui_timing_info_present_flag {
        let num_units_in_tick = br.read_u(32)?;
        let time_scale = br.read_u(32)?;
        if num_units_in_tick > 0 {
            frame_rate = Some(Rational {
                num: time_scale,
                den: num_units_in_tick,
            });
        }
        // PARTIAL-PARSE NOTE: this function does NOT read beyond the
        // vui_timing_info block (no vui_hrd_parameters, no bitstream_restriction).
        // That is intentional — only frame-rate and color metadata are surfaced.
        // This is safe because vui_parameters() is the last read in parse_sps
        // (h265/sps.rs); nothing follows it that depends on a correctly advanced
        // bit cursor. If a future change adds post-VUI reads to parse_sps, this
        // parser must be extended to consume the skipped fields first.
        //
        // Within the timing block, consume the two trailing fields
        // (vui_poc_proportional_to_timing_flag and the conditional
        // vui_num_ticks_poc_diff_one_minus1) for consistency with the
        // spec layout — omitting them would leave the cursor inside the
        // timing block, which is relevant if the partial-parse scope above
        // is ever widened.
        let poc_proportional = br.read_bool()?; // vui_poc_proportional_to_timing_flag
        if poc_proportional {
            let _ = br.read_ue()?; // vui_num_ticks_poc_diff_one_minus1
        }
    }

    let color = if video_signal_type_present_flag
        || chroma_loc_info_present_flag
        || aspect_ratio_info_present_flag
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

    Ok(VuiOut { frame_rate, color })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::test_util::BitWriter;

    /// VUI with only `aspect_ratio_info_present_flag = 1`,
    /// `aspect_ratio_idc = 255` (Extended_SAR) and the given
    /// `sar_width` / `sar_height`; every later presence flag is 0.
    fn vui_with_extended_sar(w: u32, h: u32) -> Vec<u8> {
        let mut bw = BitWriter::new();
        bw.write(1, 1); // aspect_ratio_info_present_flag
        bw.write(255, 8); // aspect_ratio_idc = Extended_SAR
        bw.write(u64::from(w), 16); // sar_width
        bw.write(u64::from(h), 16); // sar_height
        bw.write(0, 1); // overscan_info_present_flag
        bw.write(0, 1); // video_signal_type_present_flag
        bw.write(0, 1); // chroma_loc_info_present_flag
        bw.write(0, 1); // neutral_chroma_indication_flag
        bw.write(0, 1); // field_seq_flag
        bw.write(0, 1); // frame_field_info_present_flag
        bw.write(0, 1); // default_display_window_flag
        bw.write(0, 1); // vui_timing_info_present_flag
        bw.end_rbsp();
        bw.bytes
    }

    /// CORR-16: H.265 §E.3.1 — a zero `sar_width` / `sar_height` means
    /// "unspecified"; the parser used to surface `Rational { den: 0 }`,
    /// a ÷0 hazard for every consumer. Mirrors the H.264 guard.
    #[test]
    fn extended_sar_with_zero_height_is_unspecified_not_div_by_zero() {
        let bytes = vui_with_extended_sar(16, 0);
        let mut br = BitReader::new(&bytes);
        let out = parse(&mut br, 0).unwrap();
        let color = out
            .color
            .expect("aspect_ratio_info_present builds ColorInfo");
        assert_eq!(color.sample_aspect_ratio, None);
    }

    #[test]
    fn extended_sar_with_nonzero_dims_round_trips() {
        let bytes = vui_with_extended_sar(16, 11);
        let mut br = BitReader::new(&bytes);
        let out = parse(&mut br, 0).unwrap();
        assert_eq!(
            out.color.unwrap().sample_aspect_ratio,
            Some(Rational { num: 16, den: 11 })
        );
    }
}
