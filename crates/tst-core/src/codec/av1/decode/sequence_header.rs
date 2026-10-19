//! AV1 Sequence Header OBU parser. Per AV1 spec §5.5.1 / §6.4.1.
//!
//! Surfaces the fields a TS / file consumer typically needs:
//! profile, level/tier of operating point 0, max frame dimensions,
//! bit depth, chroma format, monochrome flag, still-picture flags,
//! optional color description (mapped via H.273), and optional
//! frame rate (when timing info is present with `equal_picture_interval`).
//!
//! All other fields are walked-and-discarded — the bitstream cursor
//! must advance through every conditional path correctly so that
//! downstream parsers (e.g. frame_header) can rely on the OBU body
//! shape being well-formed even though we don't surface the data.

use super::bitreader::Av1BitReader;
use crate::codec::av1::model::Av1SequenceHeader;
use crate::codec::{
    ChromaFormat, CodecParseError, ColorInfo, ColourPrimaries, MatrixCoefficients, Rational,
    TransferCharacteristics,
};

pub fn parse_sequence_header(payload: &[u8]) -> Result<Av1SequenceHeader, CodecParseError> {
    let mut br = Av1BitReader::new(payload);

    let profile = br.f(3)? as u8;
    // AV1 §6.4.1: seq_profile values 3..=7 are reserved. Reject immediately
    // rather than carry a misparsed profile through the later branches
    // (which treat any non-{0,1} value as profile 2).
    if profile > 2 {
        return Err(CodecParseError::ReservedValue {
            field: "seq_profile",
            value: u32::from(profile),
        });
    }
    let still_picture = br.f(1)? != 0;
    let reduced_still_picture_header = br.f(1)? != 0;

    let mut timing_info_present = false;
    let mut decoder_model_info_present = false;
    let mut initial_display_delay_present = false;
    let mut op_cnt_minus_1: usize = 0;
    let mut buffer_delay_length_minus_1: u8 = 0;
    let mut frame_rate: Option<Rational> = None;

    if reduced_still_picture_header {
        // Per spec: timing_info / decoder_model_info / initial_display_delay
        // all defaulted to 0, single op, seq_tier[0]=0. Only seq_level_idx[0]
        // is coded — read it here so the i==0 branch in the loop doesn't
        // misread.
    } else {
        timing_info_present = br.f(1)? != 0;
        if timing_info_present {
            // timing_info() — §5.5.3
            let num_units_in_display_tick = br.f(32)? as u32;
            let time_scale = br.f(32)? as u32;
            // AV1 §6.4.3: both fields are required to be greater than 0.
            if num_units_in_display_tick == 0 {
                return Err(CodecParseError::ReservedValue {
                    field: "num_units_in_display_tick",
                    value: 0,
                });
            }
            if time_scale == 0 {
                return Err(CodecParseError::ReservedValue {
                    field: "time_scale",
                    value: 0,
                });
            }
            let equal_picture_interval = br.f(1)? != 0;
            if equal_picture_interval {
                // §6.4.3: the display interval per picture is
                //   (num_ticks_per_picture_minus_1 + 1) * num_units_in_display_tick
                // display ticks, so the picture rate is
                //   time_scale / [num_units_in_display_tick * (ticks_minus_1 + 1)].
                // The previous code discarded num_ticks_per_picture_minus_1
                // and reported time_scale / num_units, overstating the rate
                // by (ticks_minus_1 + 1)x.
                let num_ticks_per_picture_minus_1 = br.uvlc()?; // u64
                let ticks = num_ticks_per_picture_minus_1.saturating_add(1);
                if let Some(den_u64) = u64::from(num_units_in_display_tick).checked_mul(ticks) {
                    let g = gcd(u64::from(time_scale), den_u64).max(1);
                    let num = u64::from(time_scale) / g;
                    let den = den_u64 / g;
                    // Surface only when the reduced rational fits u32 Rational;
                    // otherwise leave None rather than truncate silently.
                    if let (Ok(num), Ok(den)) = (u32::try_from(num), u32::try_from(den))
                        && den != 0
                    {
                        frame_rate = Some(Rational { num, den });
                    }
                }
            }
            // (When equal_picture_interval is false the stream asserts no
            // constant picture rate; frame_rate stays None.)
            decoder_model_info_present = br.f(1)? != 0;
            if decoder_model_info_present {
                // decoder_model_info() — §5.5.4
                buffer_delay_length_minus_1 = br.f(5)? as u8;
                let _num_units_in_decoding_tick = br.f(32)?;
                let _buffer_removal_time_length_minus_1 = br.f(5)?;
                let _frame_presentation_time_length_minus_1 = br.f(5)?;
            }
        }
        initial_display_delay_present = br.f(1)? != 0;
        op_cnt_minus_1 = br.f(5)? as usize;
    }

    let mut level = 0u8;
    let mut tier = 0u8;

    if reduced_still_picture_header {
        // Single op, seq_level_idx[0] f(5), no seq_tier, no decoder_model
        // / initial_display_delay walks.
        level = br.f(5)? as u8;
        tier = 0;
    } else {
        for i in 0..=op_cnt_minus_1 {
            let _operating_point_idc = br.f(12)?;
            let seq_level_idx = br.f(5)? as u8;
            let seq_tier = if seq_level_idx > 7 { br.f(1)? as u8 } else { 0 };
            if i == 0 {
                level = seq_level_idx;
                tier = seq_tier;
            }
            if decoder_model_info_present {
                let dmpfto = br.f(1)? != 0;
                if dmpfto {
                    // operating_parameters_info(i) — §5.5.5:
                    //   decoder_buffer_delay[op] f(n)
                    //   encoder_buffer_delay[op] f(n)
                    //   low_delay_mode_flag[op]   f(1)
                    // where n = buffer_delay_length_minus_1 + 1.
                    let n = (buffer_delay_length_minus_1 as usize) + 1;
                    let _ = br.f(n)?;
                    let _ = br.f(n)?;
                    let _ = br.f(1)?;
                }
            }
            if initial_display_delay_present {
                let idd_present = br.f(1)? != 0;
                if idd_present {
                    let _initial_display_delay_minus_1 = br.f(4)?;
                }
            }
        }
    }

    let frame_width_bits_minus_1 = br.f(4)? as usize;
    let frame_height_bits_minus_1 = br.f(4)? as usize;
    let max_frame_width = (br.f(frame_width_bits_minus_1 + 1)? + 1) as u32;
    let max_frame_height = (br.f(frame_height_bits_minus_1 + 1)? + 1) as u32;

    let frame_id_numbers_present = if reduced_still_picture_header {
        false
    } else {
        br.f(1)? != 0
    };
    if frame_id_numbers_present {
        let _delta_frame_id_length_minus_2 = br.f(4)?;
        let _additional_frame_id_length_minus_1 = br.f(3)?;
    }

    let _use_128x128_superblock = br.f(1)? != 0;
    let _enable_filter_intra = br.f(1)? != 0;
    let _enable_intra_edge_filter = br.f(1)? != 0;
    if !reduced_still_picture_header {
        let _enable_interintra_compound = br.f(1)?;
        let _enable_masked_compound = br.f(1)?;
        let _enable_warped_motion = br.f(1)?;
        let _enable_dual_filter = br.f(1)?;
        let enable_order_hint = br.f(1)? != 0;
        if enable_order_hint {
            let _enable_jnt_comp = br.f(1)?;
            let _enable_ref_frame_mvs = br.f(1)?;
        }
        let seq_choose_screen_content_tools = br.f(1)? != 0;
        let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            // SELECT_SCREEN_CONTENT_TOOLS = 2 — implicit, not coded.
            2u8
        } else {
            br.f(1)? as u8
        };
        if seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = br.f(1)? != 0;
            if !seq_choose_integer_mv {
                let _seq_force_integer_mv = br.f(1)?;
            }
            // else: SELECT_INTEGER_MV (implicit, not coded).
        }
        if enable_order_hint {
            let _order_hint_bits_minus_1 = br.f(3)?;
        }
    }

    let _enable_superres = br.f(1)? != 0;
    let _enable_cdef = br.f(1)? != 0;
    let _enable_restoration = br.f(1)? != 0;

    // color_config() — §5.5.2.
    let high_bitdepth = br.f(1)? != 0;
    let bit_depth = if profile == 2 && high_bitdepth {
        let twelve_bit = br.f(1)? != 0;
        if twelve_bit { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let monochrome = if profile == 1 { false } else { br.f(1)? != 0 };
    let color_description_present = br.f(1)? != 0;
    let (cp_byte, tc_byte, mc_byte) = if color_description_present {
        (br.f(8)? as u8, br.f(8)? as u8, br.f(8)? as u8)
    } else {
        // Defaults from §5.5.2: CP_UNSPECIFIED=2, TC_UNSPECIFIED=2, MC_UNSPECIFIED=2.
        (2u8, 2u8, 2u8)
    };

    // The wire-format color_range bit lives outside the
    // color_description_present_flag=1 block (see AV1 §6.4.2): for both
    // monochrome and non-monochrome non-special-case paths we read it
    // unconditionally. To preserve the dynamic-range signal even when no
    // explicit color description is signalled, we emit ColorInfo with
    // UNSPECIFIED primaries/transfer/matrix in that branch — mirroring the
    // H.264/H.265/H.266 parsers' treatment of the same shape.
    let (full_range, subsampling_x, subsampling_y);
    if monochrome {
        full_range = br.f(1)? != 0;
        subsampling_x = true;
        subsampling_y = true;
        // For monochrome streams, chroma_sample_position and
        // separate_uv_delta_q are not coded in the bitstream — they're
        // inferred (CSP_UNKNOWN and 0 respectively) per AV1 §5.5.2. The
        // `if !monochrome` guards below skip those reads on this path.
    } else if cp_byte == 1 /* CP_BT_709 */
        && tc_byte == 13 /* TC_SRGB */
        && mc_byte == 0
    /* MC_IDENTITY */
    {
        // Implicit: color_range=1, subsampling_x=0, subsampling_y=0.
        full_range = true;
        subsampling_x = false;
        subsampling_y = false;
    } else {
        full_range = br.f(1)? != 0;
        if profile == 0 {
            subsampling_x = true;
            subsampling_y = true;
        } else if profile == 1 {
            subsampling_x = false;
            subsampling_y = false;
        } else {
            // profile == 2
            if bit_depth == 12 {
                let sx = br.f(1)? != 0;
                let sy = if sx { br.f(1)? != 0 } else { false };
                subsampling_x = sx;
                subsampling_y = sy;
            } else {
                subsampling_x = true;
                subsampling_y = false;
            }
        }
    }

    if !monochrome && subsampling_x && subsampling_y {
        let _chroma_sample_position = br.f(2)?;
    }
    if !monochrome {
        let _separate_uv_delta_q = br.f(1)?;
    }

    let chroma_format = if monochrome {
        ChromaFormat::Monochrome
    } else {
        match (subsampling_x, subsampling_y) {
            (false, false) => ChromaFormat::Yuv444,
            (true, false) => ChromaFormat::Yuv422,
            (true, true) => ChromaFormat::Yuv420,
            (false, true) => {
                // AV1 forbids 4:4:0 — spec doesn't define this combination;
                // surface as ReservedValue rather than a misleading enum.
                return Err(CodecParseError::ReservedValue {
                    field: "av1_subsampling",
                    value: 0b01,
                });
            }
        }
    };

    // Always surface ColorInfo: even with color_description_present_flag=0,
    // the wire-format carries color_range, so the dynamic-range signal is
    // observable. When the explicit description is absent, fields default to
    // UNSPECIFIED (CP/TC/MC byte 2) per AV1 §5.5.2.
    let color_info = Some(ColorInfo {
        primaries: ColourPrimaries::from_h273(cp_byte),
        transfer: TransferCharacteristics::from_h273(tc_byte),
        matrix: MatrixCoefficients::from_h273(mc_byte),
        full_range,
        chroma_loc: None,
        sample_aspect_ratio: None,
    });

    if !reduced_still_picture_header {
        let _film_grain_params_present = br.f(1)?;
    }

    // Suppress unused-warning paths the compiler can't see through.
    let _ = (timing_info_present, initial_display_delay_present);

    Ok(Av1SequenceHeader {
        profile,
        level,
        tier,
        max_frame_width,
        max_frame_height,
        bit_depth,
        monochrome,
        chroma_format,
        still_picture,
        reduced_still_picture_header,
        color_info,
        frame_rate,
        raw: payload.to_vec(),
    })
}

/// Euclid GCD for reducing the frame-rate rational. Both AV1 timing
/// integers are 32-bit; the product is computed in u64.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}
