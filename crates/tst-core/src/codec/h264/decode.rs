//! H.264 / AVC parameter-set decoders (private; re-exported through [`super`]).

use super::model::{EntropyCodingMode, H264ParameterSets, H264Pps, H264Sps};
use crate::codec::bitreader::BitReader;
use crate::codec::{
    ChromaFormat, CodecParseError, ColorInfo, ColourPrimaries, MatrixCoefficients, Rational,
    TransferCharacteristics, aspect_ratio_idc_to_sar, read_h273_colour, validate_bit_depth_minus8,
};
use crate::mpegts::demux::event::NalUnit;

/// Parse all SPS/PPS NAL units from a slice of [`NalUnit`]s.
///
/// Behavior is partial-success-tolerant: bad parameter set NALs emit a
/// `tracing::warn!` and are skipped. If every parameter set NAL in the
/// input failed, the function returns `Err`. Inputs that contain no
/// parameter set NALs (e.g., non-IDR access units, H.265-only slices)
/// return `Ok(H264ParameterSets::default())`.
///
/// After NAL collection, each PPS is cross-validated against the parsed
/// SPS map: per H.264 V15 §7.4.2.2, a PPS's `seq_parameter_set_id` must
/// refer to an SPS active in the stream. PPSes whose referenced SPS is
/// absent from the input emit a `tracing::warn!` and are dropped from
/// the output. (Strict cross-validation enforcement is not currently
/// surfaced as a typed error — see [`CodecParseError::DanglingSpsReference`]
/// for the variant reserved for future strict-mode use.)
pub fn parse_parameter_sets(nals: &[NalUnit]) -> Result<H264ParameterSets, CodecParseError> {
    let mut out = H264ParameterSets::default();
    let mut had_param_set = false;
    let mut all_failed = true;

    for nal in nals {
        let NalUnit::H264 {
            nal_type, payload, ..
        } = nal
        else {
            continue;
        };
        match *nal_type {
            7 => {
                had_param_set = true;
                match parse_sps(payload) {
                    Ok(sps) => {
                        out.sps_by_id.insert(sps.seq_parameter_set_id, sps);
                        all_failed = false;
                    }
                    Err(e) => {
                        tracing::warn!(target: "tst_core::codec::h264",
                            error = ?e, "skipping malformed SPS");
                    }
                }
            }
            8 => {
                had_param_set = true;
                match parse_pps(payload) {
                    Ok(pps) => {
                        out.pps_by_id.insert(pps.pic_parameter_set_id, pps);
                        all_failed = false;
                    }
                    Err(e) => {
                        tracing::warn!(target: "tst_core::codec::h264",
                            error = ?e, "skipping malformed PPS");
                    }
                }
            }
            _ => {}
        }
    }

    if had_param_set && all_failed {
        return Err(CodecParseError::EngineError(
            "every parameter set NAL in the input failed to parse".into(),
        ));
    }

    // Cross-validate PPS→SPS references per H.264 V15 §7.4.2.2: each
    // PPS's `seq_parameter_set_id` must refer to an SPS present in the
    // stream. Drop dangling PPSes (matches the partial-success policy
    // used above for malformed SPS/PPS NALs).
    let sps_ids = &out.sps_by_id;
    out.pps_by_id.retain(|pps_id, pps| {
        if sps_ids.contains_key(&pps.seq_parameter_set_id) {
            true
        } else {
            tracing::warn!(
                target: "tst_core::codec::h264",
                pps_id = *pps_id,
                sps_id = pps.seq_parameter_set_id,
                "dropping PPS that references SPS id not in input"
            );
            false
        }
    });

    Ok(out)
}

/// Parse a single PPS RBSP. Same input contract as `parse_sps`. Strict
/// (returns Err on first failure). Range-checks both IDs per H.264 V15
/// §7.4.2.2: `pic_parameter_set_id` ∈ [0, 255], `seq_parameter_set_id`
/// ∈ [0, 31].
///
/// This standalone variant does NOT cross-validate that
/// `seq_parameter_set_id` references an SPS actually present elsewhere
/// in the stream — that cross-check is performed by
/// [`parse_parameter_sets`], which has visibility into both maps.
pub fn parse_pps(rbsp: &[u8]) -> Result<H264Pps, CodecParseError> {
    if rbsp.is_empty() {
        return Err(CodecParseError::TruncatedRbsp {
            offset_bits: 0,
            needed_bits: 8,
        });
    }
    // `BitReader` transparently strips emulation-prevention-three bytes
    // while reading. We only need the first three fields
    // (pic_parameter_set_id, seq_parameter_set_id,
    // entropy_coding_mode_flag) — all three are at the very start of the
    // PPS RBSP, so we never need SPS context to read them.
    let mut br = BitReader::new(rbsp);
    let pps_id = br.read_ue()?;
    let sps_id = br.read_ue()?;
    let cabac = br.read_bool()?;
    // H.264 V15 §7.4.2.2 (PDF p. 109):
    //   pic_parameter_set_id ∈ [0, 255]
    //   seq_parameter_set_id ∈ [0, 31] (NOT the same range as pic id)
    // The PPS ID storage type (u8) enforces the first bound implicitly via
    // u8::try_from; the SPS ID bound is tighter than u8 and must be
    // checked explicitly. Accepting sps_id ∈ [32, 255] would silently
    // create a PPS that can never match any spec-conformant SPS map.
    // NOTE: deliberately NOT `read_ue_max` — the range checks here run
    // after all three header reads (parse errors first, then pic-id range,
    // then sps-id range); fusing the sps check into its read would reorder
    // error precedence on doubly-invalid inputs.
    let pic_parameter_set_id =
        u8::try_from(pps_id).map_err(|_| CodecParseError::ReservedValue {
            field: "pic_parameter_set_id",
            value: pps_id,
        })?;
    if sps_id > 31 {
        return Err(CodecParseError::ReservedValue {
            field: "seq_parameter_set_id",
            value: sps_id,
        });
    }
    let seq_parameter_set_id = sps_id as u8;
    Ok(H264Pps {
        pic_parameter_set_id,
        seq_parameter_set_id,
        entropy_coding_mode: if cabac {
            EntropyCodingMode::Cabac
        } else {
            EntropyCodingMode::Cavlc
        },
        raw_rbsp: rbsp.to_vec(),
    })
}

/// Per H.264 §7.3.2.1.1, `chroma_format_idc` and bit-depth fields are
/// only present for the High-profile family. The set below is the
/// spec's `profile_idc` list at §7.3.2.1.1.
fn profile_has_chroma_info(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// Map `chroma_format_idc` (+ `separate_colour_plane_flag` for 4:4:4) to
/// the typed [`ChromaFormat`]. Per H.264 §7.4.2.1.1 valid values are
/// 0..=3; any other value is reserved.
fn chroma_format_from(chroma_format_idc: u32) -> Result<ChromaFormat, CodecParseError> {
    match chroma_format_idc {
        0 => Ok(ChromaFormat::Monochrome),
        1 => Ok(ChromaFormat::Yuv420),
        2 => Ok(ChromaFormat::Yuv422),
        3 => Ok(ChromaFormat::Yuv444),
        other => Err(CodecParseError::ReservedValue {
            field: "chroma_format_idc",
            value: other,
        }),
    }
}

/// Outputs from the VUI walk that feed [`H264Sps`].
struct VuiOut {
    frame_rate: Option<Rational>,
    fixed_frame_rate: bool,
    color: Option<ColorInfo>,
    /// `max_num_reorder_frames` from the optional `bitstream_restriction()`
    /// block — `Some` only when `bitstream_restriction_flag` is set.
    max_num_reorder_frames: Option<u32>,
}

/// Parse a single SPS RBSP per H.264 §7.3.2.1.1. Input contract: RBSP
/// body only — Annex B start code stripped, NAL header byte stripped,
/// emulation prevention bytes preserved (matches
/// `NalUnit::H264 { payload }`).
pub fn parse_sps(rbsp: &[u8]) -> Result<H264Sps, CodecParseError> {
    if rbsp.is_empty() {
        return Err(CodecParseError::TruncatedRbsp {
            offset_bits: 0,
            needed_bits: 8,
        });
    }
    // `BitReader` transparently strips emulation-prevention-three bytes
    // while reading; callers strip the 1-byte NAL header before passing
    // the payload here.
    let mut br = BitReader::new(rbsp);

    let profile_idc = br.read_u(8)? as u8;
    // constraint_set0..5_flag (6 bits) + reserved_zero_2bits (2 bits),
    // surfaced as the full byte per H.264 §7.3.2.1.1.
    let constraint_set_flags = br.read_u(8)? as u8;
    let level_idc = br.read_u(8)? as u8;
    let seq_parameter_set_id = br.read_ue_max("seq_parameter_set_id", 31)? as u8;

    // High-profile family: chroma_format_idc + bit depths. Otherwise the
    // spec defaults apply (chroma 4:2:0, 8-bit luma/chroma).
    let mut chroma_format_idc: u32 = 1;
    let mut bit_depth_luma: u8 = 8;
    let mut bit_depth_chroma: u8 = 8;
    if profile_has_chroma_info(profile_idc) {
        chroma_format_idc = br.read_ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane_flag = br.read_bool()?;
        }
        let bit_depth_luma_minus8 = br.read_ue()?;
        bit_depth_luma = validate_bit_depth_minus8("bit_depth_luma_minus8", bit_depth_luma_minus8)?;
        let bit_depth_chroma_minus8 = br.read_ue()?;
        bit_depth_chroma =
            validate_bit_depth_minus8("bit_depth_chroma_minus8", bit_depth_chroma_minus8)?;
        let _qpprime_y_zero_transform_bypass_flag = br.read_bool()?;
        let seq_scaling_matrix_present_flag = br.read_bool()?;
        if seq_scaling_matrix_present_flag {
            skip_scaling_matrix(&mut br, chroma_format_idc)?;
        }
    }
    // Validate after reading so a reserved value still consumes the same
    // bits as a valid one would (keeps the error site spec-accurate).
    let chroma_format = chroma_format_from(chroma_format_idc)?;

    // H.264 §7.4.2.1.1: log2_max_frame_num_minus4 ∈ [0, 12].
    // The old u8::try_from only caught values > 255; values 13–255 were
    // accepted as in-spec.  Use read_ue_max to enforce the tighter bound.
    let log2_max_frame_num_minus4 = br.read_ue_max("log2_max_frame_num_minus4", 12)? as u8;

    let pic_order_cnt_type = br.read_ue()?;
    match pic_order_cnt_type {
        0 => {
            // H.264 §7.4.2.1.1: log2_max_pic_order_cnt_lsb_minus4 ∈ [0, 12].
            let _log2_max_pic_order_cnt_lsb_minus4 =
                br.read_ue_max("log2_max_pic_order_cnt_lsb_minus4", 12)?;
        }
        1 => {
            let _delta_pic_order_always_zero_flag = br.read_bool()?;
            let _offset_for_non_ref_pic = br.read_se()?;
            let _offset_for_top_to_bottom_field = br.read_se()?;
            let num_ref_frames_in_pic_order_cnt_cycle = br.read_ue()?;
            for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
                let _offset_for_ref_frame = br.read_se()?;
            }
        }
        2 => {}
        other => {
            return Err(CodecParseError::ReservedValue {
                field: "pic_order_cnt_type",
                value: other,
            });
        }
    }

    let _max_num_ref_frames = br.read_ue()?;
    let _gaps_in_frame_num_value_allowed_flag = br.read_bool()?;
    let pic_width_in_mbs_minus1 = br.read_ue()?;
    let pic_height_in_map_units_minus1 = br.read_ue()?;
    let frame_mbs_only_flag = br.read_bool()?;
    if !frame_mbs_only_flag {
        let _mb_adaptive_frame_field_flag = br.read_bool()?;
    }
    let _direct_8x8_inference_flag = br.read_bool()?;

    let frame_cropping_flag = br.read_bool()?;
    let (crop_l, crop_r, crop_t, crop_b) = if frame_cropping_flag {
        let l = br.read_ue()?;
        let r = br.read_ue()?;
        let t = br.read_ue()?;
        let b = br.read_ue()?;
        (l, r, t, b)
    } else {
        (0, 0, 0, 0)
    };

    let vui_parameters_present_flag = br.read_bool()?;
    let vui = if vui_parameters_present_flag {
        parse_vui(&mut br)?
    } else {
        VuiOut {
            frame_rate: None,
            fixed_frame_rate: false,
            color: None,
            max_num_reorder_frames: None,
        }
    };

    // Dimensions + frame_crop per H.264 §6.4 / §7.4.2.1.1. The coded
    // dimensions are macroblock-aligned; crop offsets are stored in
    // chroma-array units and converted to luma samples via
    // step_x = SubWidthC, step_y = SubHeightC * (2 - frame_mbs_only_flag).
    let mul: u32 = if frame_mbs_only_flag { 1 } else { 2 };
    let coded_width = pic_width_in_mbs_minus1.saturating_add(1).saturating_mul(16);
    let coded_height = pic_height_in_map_units_minus1
        .saturating_add(1)
        .saturating_mul(mul.saturating_mul(16));

    let vsub: u32 = u32::from(chroma_format_idc == 1);
    let hsub: u32 = u32::from(chroma_format_idc == 1 || chroma_format_idc == 2);
    let step_x: u32 = 1 << hsub;
    let step_y: u32 = mul << vsub;

    let crop_left = crop_l.saturating_mul(step_x);
    let crop_right = crop_r.saturating_mul(step_x);
    let crop_top = crop_t.saturating_mul(step_y);
    let crop_bottom = crop_b.saturating_mul(step_y);

    let width = crate::codec::apply_crop(
        "frame_crop_left_offset + frame_crop_right_offset",
        coded_width,
        crop_left,
        crop_right,
    )?;
    let height = crate::codec::apply_crop(
        "frame_crop_top_offset + frame_crop_bottom_offset",
        coded_height,
        crop_top,
        crop_bottom,
    )?;

    let has_b_frames = compute_has_b_frames(profile_idc, constraint_set_flags, &vui);

    Ok(H264Sps {
        seq_parameter_set_id,
        width,
        height,
        profile_idc,
        level_idc,
        constraint_set_flags,
        bit_depth_luma,
        bit_depth_chroma,
        chroma_format,
        frame_mbs_only: frame_mbs_only_flag,
        frame_rate: vui.frame_rate,
        fixed_frame_rate: vui.fixed_frame_rate,
        has_b_frames,
        color: vui.color,
        crop_left,
        crop_right,
        crop_top,
        crop_bottom,
        log2_max_frame_num_minus4,
        raw_rbsp: rbsp.to_vec(),
    })
}

/// `seq_scaling_matrix()` per H.264 §7.3.2.1.1.1: a sequence of
/// `seq_scaling_list_present_flag` bits, each followed by a scaling list
/// when set. We only need to advance the bit position correctly, so the
/// delta_scale values are read and discarded.
fn skip_scaling_matrix(
    br: &mut BitReader<'_>,
    chroma_format_idc: u32,
) -> Result<(), CodecParseError> {
    let count = if chroma_format_idc == 3 { 12 } else { 8 };
    for i in 0..count {
        let present = br.read_bool()?;
        if present {
            let size = if i < 6 { 16 } else { 64 };
            skip_scaling_list(br, size)?;
        }
    }
    Ok(())
}

/// `scaling_list()` per H.264 §7.3.2.1.1.1 — walk `size` deltas, stopping
/// early once `next_scale` reaches 0 (matching the spec's loop guard).
fn skip_scaling_list(br: &mut BitReader<'_>, size: u32) -> Result<(), CodecParseError> {
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta_scale = br.read_se()?;
            // delta_scale is se(v) — attacker-controlled (up to ~±2^31). H.264
            // §7.4.2.1.1.1 bounds it to [-128,127] (where this is exact); the
            // i64 intermediate keeps a non-conformant SPS from overflowing the
            // i32 add. The skipped scaling-list result only drives loop
            // continuation, so a non-conformant value is tolerated, not fatal.
            next_scale = (((last_scale as i64) + (delta_scale as i64) + 256) % 256) as i32;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Ok(())
}

/// `vui_parameters()` per H.264 §E.1.1. Reads the full structure (so the
/// bit position stays aligned through `bitstream_restriction()`), but only
/// surfaces the fields [`H264Sps`] exposes: color signalling, frame rate,
/// fixed-frame-rate, and `max_num_reorder_frames`.
fn parse_vui(br: &mut BitReader<'_>) -> Result<VuiOut, CodecParseError> {
    let aspect_ratio_info_present_flag = br.read_bool()?;
    let mut sample_aspect_ratio = None;
    if aspect_ratio_info_present_flag {
        let aspect_ratio_idc = br.read_u(8)? as u8;
        if aspect_ratio_idc == 255 {
            // Extended_SAR: sar_width / sar_height. Per H.264 §E.2.1, a
            // zero in either field means "unspecified".
            let w = br.read_u(16)?;
            let h = br.read_u(16)?;
            if w != 0 && h != 0 {
                sample_aspect_ratio = Some(Rational { num: w, den: h });
            }
        } else {
            sample_aspect_ratio = aspect_ratio_idc_to_sar(aspect_ratio_idc);
        }
    }

    let overscan_info_present_flag = br.read_bool()?;
    if overscan_info_present_flag {
        let _overscan_appropriate_flag = br.read_bool()?;
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
        // H.264 §E.2.1 Table E-1: chroma_sample_loc_type_* ∈ [0, 5].
        // Without the bound check, a crafted ue(v)=256 silently truncates
        // to 0 via `as u8` — a valid value that hides the malformed input.
        let top = br.read_ue_max("chroma_sample_loc_type_top_field", 5)? as u8;
        let _bottom = br.read_ue_max("chroma_sample_loc_type_bottom_field", 5)?;
        chroma_loc = Some(top);
    }

    let timing_info_present_flag = br.read_bool()?;
    let mut frame_rate = None;
    let mut fixed_frame_rate = false;
    if timing_info_present_flag {
        let num_units_in_tick = br.read_u(32)?;
        let time_scale = br.read_u(32)?;
        fixed_frame_rate = br.read_bool()?;
        // H.264 §E.2.1: frame_rate = time_scale / (2 * num_units_in_tick).
        // `2 * num_units_in_tick` can overflow u32; saturate and treat
        // saturation to u32::MAX as "unknowable" (None) rather than emit a
        // nonsense ratio. Also guard num_units_in_tick == 0: that would
        // produce den == 0, a ÷0 hazard for consumers. Mirrors H.265 §E.2.1
        // (h265/vui.rs: `if num_units_in_tick > 0`).
        let den = num_units_in_tick.saturating_mul(2);
        if num_units_in_tick > 0 && den != u32::MAX {
            frame_rate = Some(Rational {
                num: time_scale,
                den,
            });
        }
    }

    let mut hrd_parameters_present = false;
    let nal_hrd_parameters_present_flag = br.read_bool()?;
    if nal_hrd_parameters_present_flag {
        skip_hrd_parameters(br)?;
        hrd_parameters_present = true;
    }
    let vcl_hrd_parameters_present_flag = br.read_bool()?;
    if vcl_hrd_parameters_present_flag {
        skip_hrd_parameters(br)?;
        hrd_parameters_present = true;
    }
    if hrd_parameters_present {
        let _low_delay_hrd_flag = br.read_bool()?;
    }
    let _pic_struct_present_flag = br.read_bool()?;

    let bitstream_restriction_flag = br.read_bool()?;
    let mut max_num_reorder_frames = None;
    if bitstream_restriction_flag {
        let _motion_vectors_over_pic_boundaries_flag = br.read_bool()?;
        let _max_bytes_per_pic_denom = br.read_ue()?;
        let _max_bits_per_mb_denom = br.read_ue()?;
        let _log2_max_mv_length_horizontal = br.read_ue()?;
        let _log2_max_mv_length_vertical = br.read_ue()?;
        let reorder = br.read_ue()?;
        let _max_dec_frame_buffering = br.read_ue()?;
        max_num_reorder_frames = Some(reorder);
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

    Ok(VuiOut {
        frame_rate,
        fixed_frame_rate,
        color,
        max_num_reorder_frames,
    })
}

/// `hrd_parameters()` per H.264 §E.1.2 — read past to keep the VUI bit
/// position aligned; no surfaced fields.
fn skip_hrd_parameters(br: &mut BitReader<'_>) -> Result<(), CodecParseError> {
    let cpb_cnt_minus1 = br.read_ue()?;
    br.skip(4)?; // bit_rate_scale
    br.skip(4)?; // cpb_size_scale
    for _ in 0..cpb_cnt_minus1.saturating_add(1) {
        let _bit_rate_value_minus1 = br.read_ue()?;
        let _cpb_size_value_minus1 = br.read_ue()?;
        let _cbr_flag = br.read_bool()?;
    }
    br.skip(5)?; // initial_cpb_removal_delay_length_minus1
    br.skip(5)?; // cpb_removal_delay_length_minus1
    br.skip(5)?; // dpb_output_delay_length_minus1
    br.skip(5)?; // time_offset_length
    Ok(())
}

/// B-frame presence heuristic.
fn compute_has_b_frames(profile_idc: u8, constraint_set_flags: u8, vui: &VuiOut) -> bool {
    // Prefer the explicit VUI bitstream_restriction field when present.
    if let Some(reorder) = vui.max_num_reorder_frames {
        return reorder > 0;
    }
    // Fallback: Baseline (66) never uses B-frames by definition.
    if profile_idc == 66 {
        return false;
    }
    // H.264 §A.2: profile_idc=100 (High) admits two B-frameless subsets
    // distinguished by constraint flags:
    //   - Constrained High: constraint_set1_flag = 1
    //   - Constrained-Baseline-lifted-to-High:
    //       constraint_set4_flag = 1 AND constraint_set5_flag = 1
    // The constraint_set_flags byte is MSB-first per §7.3.2.1.1: bit 7 =
    // constraint_set0_flag, ..., bit 2 = constraint_set5_flag. Narrowed to
    // profile_idc == 100 because the same constraint bits carry different
    // semantics on other profile_idc values (e.g. constraint_set1_flag on
    // Baseline (66) signals Main compatibility, not "no B-frames").
    if profile_idc == 100 {
        let flag1 = constraint_set_flags & 0b0100_0000 != 0;
        let flag4 = constraint_set_flags & 0b0000_1000 != 0;
        let flag5 = constraint_set_flags & 0b0000_0100 != 0;
        if flag1 || (flag4 && flag5) {
            return false;
        }
    }
    true
}
