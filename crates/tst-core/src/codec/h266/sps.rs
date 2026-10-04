//! H.266 SPS parser. Per H.266 V4 §7.3.2.4.

use crate::codec::bitreader::BitReader;
use crate::codec::h266::profile_tier_level::{H266ProfileTierLevel, parse_into};
use crate::codec::h266::vui::parse_h266_vui;
use crate::codec::{ChromaFormat, CodecParseError, ColorInfo, Rational, validate_bit_depth_minus8};
use alloc::vec::Vec;

#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H266Sps {
    pub sps_id: u8,
    pub vps_id: u8,
    pub profile_tier_level: H266ProfileTierLevel,
    pub width: u32,
    pub height: u32,
    pub chroma_format: ChromaFormat,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub color_info: Option<ColorInfo>,
    pub frame_rate: Option<Rational>,
    /// Luma-sample conformance-window crop offsets per H.266 §7.4.3.4.
    /// `coded_width = width + crop_left + crop_right` (and similarly for
    /// height). Useful for sizing GPU buffers.
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    pub raw_rbsp: Vec<u8>,
}

impl H266Sps {
    /// Pre-crop luma width — the value of `pic_width_max_in_luma_samples`
    /// before conformance-window cropping was applied.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_width(&self) -> u32 {
        self.width + self.crop_left + self.crop_right
    }
    /// Pre-crop luma height — the value of `pic_height_max_in_luma_samples`
    /// before conformance-window cropping was applied.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_height(&self) -> u32 {
        self.height + self.crop_top + self.crop_bottom
    }
}

/// Parse an H.266 SPS RBSP (Annex-B start codes already stripped,
/// emulation-prevention bytes preserved). Per H.266 V4 §7.3.2.4.
///
/// Current scope surfaces: `sps_id`, `vps_id`, headline `profile_tier_level`
/// fields, dimensions, chroma format, bit depth, and (optionally)
/// `color_info` + `frame_rate` from VUI. Conformance-window cropping is
/// applied to width/height before they are returned.
///
/// Bails `CodecParseError::UnsupportedProfile` on `sps_subpic_info_present_flag`
/// and `sps_scaling_list_data_present_flag` paths — these are rare in
/// reference-encoder defaults and would require walking large per-tile
/// or per-coefficient blocks not modeled here. Same conservative stance
/// as `codec::h265::parse_sps`.
pub fn parse_sps(rbsp: &[u8]) -> Result<H266Sps, CodecParseError> {
    if rbsp.is_empty() {
        return Err(CodecParseError::TruncatedRbsp {
            offset_bits: 0,
            needed_bits: 8,
        });
    }
    let mut br = BitReader::new(rbsp);

    // §7.3.2.4 SPS header.
    let sps_id = br.read_u(4)? as u8;
    let vps_id = br.read_u(4)? as u8;
    let max_sublayers_minus1 = br.read_u(3)? as u8;
    let chroma_format_idc = br.read_u(2)?;
    let log2_ctu_size_minus5 = br.read_u(2)?;
    let ptl_dpb_hrd_present = br.read_bool()?;

    // profile_tier_level(1, sps_max_sublayers_minus1) when flag is set.
    // The PTL walks past its headline fields — alignment matters for
    // every subsequent ue(v) read, so the full PTL syntax must be
    // consumed (not just the 16-bit headline that the standalone
    // `parse_profile_tier_level` exposes).
    let mut profile_tier_level = H266ProfileTierLevel::default();
    if ptl_dpb_hrd_present {
        parse_into(&mut br, true, max_sublayers_minus1, &mut profile_tier_level)?;
    }

    let _gdr_enabled_flag = br.read_bool()?;
    let ref_pic_resampling_enabled_flag = br.read_bool()?;
    if ref_pic_resampling_enabled_flag {
        let _res_change_in_clvs_allowed_flag = br.read_bool()?;
    }

    let pic_width_max_in_luma_samples = br.read_ue()?;
    let pic_height_max_in_luma_samples = br.read_ue()?;

    // Conformance-window cropping (semantics match H.265 §7.4.3.2.1).
    let conformance_window_flag = br.read_bool()?;
    let (mut crop_x_left, mut crop_x_right, mut crop_y_top, mut crop_y_bottom) = (0u32, 0, 0, 0);
    if conformance_window_flag {
        let conf_win_left_offset = br.read_ue()?;
        let conf_win_right_offset = br.read_ue()?;
        let conf_win_top_offset = br.read_ue()?;
        let conf_win_bottom_offset = br.read_ue()?;
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2u32, 2u32),
            2 => (2, 1),
            3 => (1, 1),
            _ => (1, 1),
        };
        crop_x_left = sub_w.saturating_mul(conf_win_left_offset);
        crop_x_right = sub_w.saturating_mul(conf_win_right_offset);
        crop_y_top = sub_h.saturating_mul(conf_win_top_offset);
        crop_y_bottom = sub_h.saturating_mul(conf_win_bottom_offset);
    }

    let subpic_info_present_flag = br.read_bool()?;
    if subpic_info_present_flag {
        // The subpic block reads variable-width per-subpic fields whose
        // bit-widths depend on CTU size and picture dimensions.
        // Modeling that path adds significant complexity for streams
        // that almost never occur in non-multi-picture encoders.
        return Err(CodecParseError::UnsupportedProfile {
            profile_idc: profile_tier_level.general_profile_idc,
        });
    }

    let bit_depth_minus8 = br.read_ue()?;
    let bit_depth_luma = validate_bit_depth_minus8("sps_bitdepth_minus8", bit_depth_minus8)?;
    // H.266 §7.4.3.4 has a single `sps_bitdepth_minus8` covering both
    // luma and chroma — spec invariant, not a parser simplification.
    let bit_depth_chroma = bit_depth_luma;

    // The chroma format derives directly from sps_chroma_format_idc.
    let chroma_format = match chroma_format_idc {
        0 => ChromaFormat::Monochrome,
        1 => ChromaFormat::Yuv420,
        2 => ChromaFormat::Yuv422,
        3 => ChromaFormat::Yuv444,
        other => {
            return Err(CodecParseError::ReservedValue {
                field: "sps_chroma_format_idc",
                value: other,
            });
        }
    };

    // Width/height after conformance-window cropping.
    let width = crate::codec::apply_crop(
        "sps_conf_win_left_offset + sps_conf_win_right_offset",
        pic_width_max_in_luma_samples,
        crop_x_left,
        crop_x_right,
    )?;
    let height = crate::codec::apply_crop(
        "sps_conf_win_top_offset + sps_conf_win_bottom_offset",
        pic_height_max_in_luma_samples,
        crop_y_top,
        crop_y_bottom,
    )?;

    // SPS body walk per H.266 V4 §7.3.2.4 — entropy_coding_sync,
    // log2_max_pic_order_cnt, partition constraints, ... up to the VUI
    // flag. Bails UnsupportedProfile on subpic / scaling list paths
    // (rare in default-encoder output).
    let (color_info, frame_rate) = walk_sps_body(
        &mut br,
        ptl_dpb_hrd_present,
        max_sublayers_minus1,
        vps_id,
        chroma_format_idc,
        log2_ctu_size_minus5,
    )?;

    Ok(H266Sps {
        sps_id,
        vps_id,
        profile_tier_level,
        width,
        height,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
        color_info,
        frame_rate,
        crop_left: crop_x_left,
        crop_right: crop_x_right,
        crop_top: crop_y_top,
        crop_bottom: crop_y_bottom,
        raw_rbsp: rbsp.to_vec(),
    })
}

// ─── SPS body walker ──────────────────────────────────────────────────────────

/// Walk the SPS RBSP body from `sps_entropy_coding_sync_enabled_flag` through
/// `sps_vui_parameters_present_flag` (§7.3.2.4), then call the VUI parser if
/// the flag is set.
///
/// Returns `(color_info, frame_rate)`: `color_info` comes from the VUI walk
/// (`None` when `sps_vui_parameters_present_flag` is unset); `frame_rate` is
/// recovered from `general_timing_hrd_parameters()` (§7.3.5.1) — H.266 moved
/// timing out of VUI, unlike H.265 — and is `None` when timing/HRD params
/// aren't present.
fn walk_sps_body(
    br: &mut BitReader<'_>,
    ptl_dpb_hrd_present: bool,
    max_sublayers_minus1: u8,
    vps_id: u8,
    chroma_format_idc: u32,
    log2_ctu_size_minus5: u32,
) -> Result<(Option<ColorInfo>, Option<Rational>), CodecParseError> {
    // §7.3.2.4 — entropy coding / entry-point config.
    let _entropy_coding_sync_enabled_flag = br.read_bool()?;
    let _entry_point_offsets_present_flag = br.read_bool()?;

    // §7.3.2.4 — POC config.
    let _log2_max_pic_order_cnt_lsb_minus4 = br.read_u(4)?; // u(4)
    let poc_msb_cycle_flag = br.read_bool()?;
    if poc_msb_cycle_flag {
        let _poc_msb_cycle_len_minus1 = br.read_ue()?;
    }

    // §7.3.2.4 — extra PH/SH bytes: each byte contributes 8 flag bits.
    let sps_num_extra_ph_bytes = br.read_u(2)?; // u(2)
    for _ in 0..(sps_num_extra_ph_bytes * 8) {
        br.read_bool()?; // sps_extra_ph_bit_present_flag[i]
    }
    let sps_num_extra_sh_bytes = br.read_u(2)?; // u(2)
    for _ in 0..(sps_num_extra_sh_bytes * 8) {
        br.read_bool()?; // sps_extra_sh_bit_present_flag[i]
    }

    // §7.3.2.4 — DPB parameters (conditional on PTL/DPB/HRD present flag).
    // Per §7.3.4, dpb_parameters(MaxSubLayersMinus1, subLayerInfoFlag) loops
    // from (subLayerInfoFlag ? 0 : MaxSubLayersMinus1) to MaxSubLayersMinus1.
    if ptl_dpb_hrd_present {
        let sublayer_dpb_params_flag = if max_sublayers_minus1 > 0 {
            br.read_bool()? // sps_sublayer_dpb_params_flag
        } else {
            false
        };
        walk_dpb_parameters(br, max_sublayers_minus1, sublayer_dpb_params_flag)?;
    }

    // §7.3.2.4 — minimum luma coding-block size (ue(v)).
    let _log2_min_luma_coding_block_size_minus2 = br.read_ue()?;

    // CtbSizeY = 1 << (sps_log2_ctu_size_minus5 + 5).
    let ctu_size_y = 1u32 << (log2_ctu_size_minus5 + 5);

    // §7.3.2.4 — partition constraints block.
    let partition_constraints_override_enabled_flag = br.read_bool()?;
    let _ = partition_constraints_override_enabled_flag; // read for cursor only

    // Intra luma QT/MTT partition constraints (always present).
    let _log2_diff_min_qt_min_cb_intra_luma = br.read_ue()?;
    let max_mtt_depth_intra_luma = br.read_ue()?;
    if max_mtt_depth_intra_luma != 0 {
        let _log2_diff_max_bt_min_qt_intra_luma = br.read_ue()?;
        let _log2_diff_max_tt_min_qt_intra_luma = br.read_ue()?;
    }

    // Dual-tree intra chroma (only present when chroma_format_idc != 0).
    let qtbtt_dual_tree_intra_flag = if chroma_format_idc != 0 {
        br.read_bool()? // sps_qtbtt_dual_tree_intra_flag
    } else {
        false
    };
    if qtbtt_dual_tree_intra_flag {
        let _log2_diff_min_qt_min_cb_intra_chroma = br.read_ue()?;
        let max_mtt_depth_intra_chroma = br.read_ue()?;
        if max_mtt_depth_intra_chroma != 0 {
            let _log2_diff_max_bt_min_qt_intra_chroma = br.read_ue()?;
            let _log2_diff_max_tt_min_qt_intra_chroma = br.read_ue()?;
        }
    }

    // Inter partition constraints (always present).
    let _log2_diff_min_qt_min_cb_inter = br.read_ue()?;
    let max_mtt_depth_inter = br.read_ue()?;
    if max_mtt_depth_inter != 0 {
        let _log2_diff_max_bt_min_qt_inter = br.read_ue()?;
        let _log2_diff_max_tt_min_qt_inter = br.read_ue()?;
    }

    // §7.3.2.4 — max luma transform size flag (present only when CTBsize > 32).
    let max_luma_transform_size_64_flag = if ctu_size_y > 32 {
        br.read_bool()? // sps_max_luma_transform_size_64_flag
    } else {
        false
    };

    // §7.3.2.4 — transform / coding tool flags.
    let transform_skip_enabled_flag = br.read_bool()?;
    if transform_skip_enabled_flag {
        let _log2_transform_skip_max_size_minus2 = br.read_ue()?;
        let _bdpcm_enabled_flag = br.read_bool()?;
    }
    let mts_enabled_flag = br.read_bool()?;
    if mts_enabled_flag {
        let _explicit_mts_intra_enabled_flag = br.read_bool()?;
        let _explicit_mts_inter_enabled_flag = br.read_bool()?;
    }
    let lfnst_enabled_flag = br.read_bool()?;

    // §7.3.2.4 — chroma QP table (present when chroma_format_idc != 0).
    let _joint_cbcr_enabled_flag = if chroma_format_idc != 0 {
        let joint_cbcr = br.read_bool()?; // sps_joint_cbcr_enabled_flag
        let same_qp_table_for_chroma_flag = br.read_bool()?;
        // numQpTables depends on same_qp_table and joint_cbcr:
        //   same_qp_table → 1; else joint_cbcr → 3; else → 2.
        let num_qp_tables = if same_qp_table_for_chroma_flag {
            1
        } else if joint_cbcr {
            3
        } else {
            2
        };
        for _ in 0..num_qp_tables {
            let _qp_table_start_minus26 = br.read_se()?;
            let num_points = br.read_ue()?; // sps_num_points_in_qp_table_minus1
            for _ in 0..=num_points {
                let _delta_qp_in_val_minus1 = br.read_ue()?;
                let _delta_qp_diff_val = br.read_ue()?;
            }
        }
        joint_cbcr
    } else {
        false
    };

    // §7.3.2.4 — filter / tool presence flags.
    let _sao_enabled_flag = br.read_bool()?;
    let alf_enabled_flag = br.read_bool()?;
    if alf_enabled_flag && chroma_format_idc != 0 {
        let _ccalf_enabled_flag = br.read_bool()?;
    }
    let _lmcs_enabled_flag = br.read_bool()?;
    // Weighted-pred flags gate the `AbsDeltaPocSt` +1 fallback in
    // `ref_pic_list_struct` per H.266 V4 §7.4.9 equation (150) — threaded
    // into the walker below. Pre-fix this code stored them as `_weighted_*`
    // and discarded them, leaving the walker to use an
    // `inter_layer_ref_pic_flag`-shaped predicate that diverged from spec
    // at `i >= 1`.
    let weighted_pred_flag = br.read_bool()?;
    let weighted_bipred_flag = br.read_bool()?;
    let long_term_ref_pics_flag = br.read_bool()?;
    // sps_inter_layer_prediction_enabled_flag present only when vps_id > 0.
    let inter_layer_pred_enabled_flag = if vps_id > 0 { br.read_bool()? } else { false };

    // §7.3.2.4 — reference picture list config.
    let _idr_rpl_present_flag = br.read_bool()?;
    let rpl1_same_as_rpl0_flag = br.read_bool()?;
    // Loop over both RPL directions (or just L0 if rpl1_same_as_rpl0).
    let num_directions = if rpl1_same_as_rpl0_flag { 1u32 } else { 2u32 };
    for list_idx in 0..num_directions {
        let num_ref_pic_lists = br.read_ue()?; // sps_num_ref_pic_lists[i]
        for rpls_idx in 0..num_ref_pic_lists {
            walk_ref_pic_list_struct(
                br,
                list_idx,
                rpls_idx,
                num_ref_pic_lists,
                long_term_ref_pics_flag,
                inter_layer_pred_enabled_flag,
                weighted_pred_flag,
                weighted_bipred_flag,
            )?;
        }
    }

    // §7.3.2.4 — motion-prediction tool flags.
    let _ref_wraparound_enabled_flag = br.read_bool()?;
    let temporal_mvp_enabled_flag = br.read_bool()?;
    if temporal_mvp_enabled_flag {
        let _sbtmvp_enabled_flag = br.read_bool()?;
    }
    let amvr_enabled_flag = br.read_bool()?;
    let bdof_enabled_flag = br.read_bool()?;
    if bdof_enabled_flag {
        let _bdof_control_present_in_ph_flag = br.read_bool()?;
    }
    let _smvd_enabled_flag = br.read_bool()?;
    let dmvr_enabled_flag = br.read_bool()?;
    if dmvr_enabled_flag {
        let _dmvr_control_present_in_ph_flag = br.read_bool()?;
    }
    let mmvd_enabled_flag = br.read_bool()?;
    if mmvd_enabled_flag {
        let _mmvd_fullpel_only_enabled_flag = br.read_bool()?;
    }

    // sps_six_minus_max_num_merge_cand encodes MaxNumMergeCand = 6 - value.
    let six_minus_max_num_merge_cand = br.read_ue()?;
    let max_num_merge_cand = 6u32.saturating_sub(six_minus_max_num_merge_cand);

    let _sbt_enabled_flag = br.read_bool()?;
    let affine_enabled_flag = br.read_bool()?;
    if affine_enabled_flag {
        let _five_minus_max_num_subblock_merge_cand = br.read_ue()?;
        let _six_param_affine_enabled_flag = br.read_bool()?;
        if amvr_enabled_flag {
            let _affine_amvr_enabled_flag = br.read_bool()?;
        }
        let affine_prof_enabled_flag = br.read_bool()?;
        if affine_prof_enabled_flag {
            let _prof_control_present_in_ph_flag = br.read_bool()?;
        }
    }
    let _bcw_enabled_flag = br.read_bool()?;
    let _ciip_enabled_flag = br.read_bool()?;

    if max_num_merge_cand >= 2 {
        let gpm_enabled_flag = br.read_bool()?;
        if gpm_enabled_flag && max_num_merge_cand >= 3 {
            let _max_num_merge_cand_minus_max_num_gpm_cand = br.read_ue()?;
        }
    }

    let _log2_parallel_merge_level_minus2 = br.read_ue()?;
    let _isp_enabled_flag = br.read_bool()?;
    let _mrl_enabled_flag = br.read_bool()?;
    let _mip_enabled_flag = br.read_bool()?;
    if chroma_format_idc != 0 {
        let _cclm_enabled_flag = br.read_bool()?;
    }
    if chroma_format_idc == 1 {
        let _chroma_horizontal_collocated_flag = br.read_bool()?;
        let _chroma_vertical_collocated_flag = br.read_bool()?;
    }

    let palette_enabled_flag = br.read_bool()?;
    let act_enabled_flag = if chroma_format_idc == 3 && !max_luma_transform_size_64_flag {
        br.read_bool()? // sps_act_enabled_flag
    } else {
        false
    };
    if transform_skip_enabled_flag || palette_enabled_flag {
        let _sps_min_qp_prime_ts = br.read_ue()?;
    }

    let ibc_enabled_flag = br.read_bool()?;
    if ibc_enabled_flag {
        let _six_minus_max_num_ibc_merge_cand = br.read_ue()?;
    }

    let ladf_enabled_flag = br.read_bool()?;
    if ladf_enabled_flag {
        let num_ladf_intervals_minus2 = br.read_u(2)?; // u(2)
        let _ladf_lowest_interval_qp_offset = br.read_se()?;
        for _ in 0..(num_ladf_intervals_minus2 + 1) {
            let _ladf_qp_offset = br.read_se()?;
            let _ladf_delta_threshold_minus1 = br.read_ue()?;
        }
    }

    // §7.3.2.4 — scaling list. Bail on explicit scaling lists — walking
    // scaling_list_data() is complex and rarely enabled in reference encoders.
    let sps_explicit_scaling_list_enabled_flag = br.read_bool()?;
    if sps_explicit_scaling_list_enabled_flag {
        return Err(CodecParseError::UnsupportedProfile {
            profile_idc: 0, // profile_idc not relevant here; bail is structural
        });
    }
    // These two flags are conditional on lfnst/act enabled AND explicit
    // scaling list enabled — both false since we bailed above if the latter
    // was set; but read the guard conditions correctly for any future path.
    if lfnst_enabled_flag && sps_explicit_scaling_list_enabled_flag {
        let _scaling_matrix_for_lfnst_disabled_flag = br.read_bool()?;
    }
    if act_enabled_flag && sps_explicit_scaling_list_enabled_flag {
        let _scaling_matrix_for_alternative_colour_space_disabled_flag = br.read_bool()?;
        // sps_scaling_matrix_designated_colour_space_flag is gated on the
        // flag just read, which we can't know here (already bailed before
        // it becomes relevant in the flag-true path).
    }

    let _dep_quant_enabled_flag = br.read_bool()?;
    let _sign_data_hiding_enabled_flag = br.read_bool()?;

    // §7.3.2.4 — virtual boundaries (per-picture or per-SPS signalling).
    let virtual_boundaries_enabled_flag = br.read_bool()?;
    if virtual_boundaries_enabled_flag {
        let virtual_boundaries_present_in_sps_flag = br.read_bool()?;
        if virtual_boundaries_present_in_sps_flag {
            let num_ver_virtual_boundaries = br.read_ue()?;
            for _ in 0..num_ver_virtual_boundaries {
                let _sps_virtual_boundary_pos_x_minus1 = br.read_ue()?;
            }
            let num_hor_virtual_boundaries = br.read_ue()?;
            for _ in 0..num_hor_virtual_boundaries {
                let _sps_virtual_boundary_pos_y_minus1 = br.read_ue()?;
            }
        }
    }

    // §7.3.2.4 — timing/HRD parameters (conditional on PTL/DPB/HRD flag).
    // Per §7.3.5.1, general_timing_hrd_parameters() carries num_units_in_tick
    // + time_scale. H.266 moved timing out of VUI (contrast with H.265 §E.2.1),
    // so frame_rate is recovered here rather than from the VUI walk.
    let mut frame_rate: Option<Rational> = None;
    if ptl_dpb_hrd_present {
        let timing_hrd_params_present_flag = br.read_bool()?;
        if timing_hrd_params_present_flag {
            if let Some((num_units, time_scale)) =
                walk_timing_hrd_parameters(br, max_sublayers_minus1)?
            {
                frame_rate = Some(Rational {
                    num: time_scale,
                    den: num_units,
                });
            }
        }
    }

    let _sps_field_seq_flag = br.read_bool()?;
    let sps_vui_parameters_present_flag = br.read_bool()?;

    // §7.3.2.4 — VUI parameters. VUI carries color_info in H.266; timing
    // lives in general_timing_hrd_parameters() above (§7.3.5.1).
    let color_info = if sps_vui_parameters_present_flag {
        let vui_payload_size_minus1 = br.read_ue()? as usize;
        let payload_size_bytes = vui_payload_size_minus1 + 1;
        // §7.3.2.4: "while( !byte_aligned() ) sps_vui_alignment_zero_bit f(1)"
        // Consume zero-padding bits to reach the next byte boundary before VUI.
        while br.position() % 8 != 0 {
            br.read_u(1)?;
        }
        // §7.3.2.21 vui_payload(payloadSize) reserves exactly 8 * payloadSize
        // bits. `vui_parameters()` (H.274 §7.2) may not consume all of them —
        // the wrapper allows `vui_reserved_payload_extension_data` + marker +
        // zero-pad tail. Snapshot the start position so we can advance the
        // cursor to the declared payload end before reading sps_extension_flag.
        let vui_start_bits = br.position();
        let color = parse_h266_vui(br)?;
        // Compute the VUI end boundary in u64 to avoid u32 overflow on a
        // crafted `vui_payload_size_minus1` ≥ 0x1FFF_FFFF (DoS via debug
        // panic). `vui_payload_size_minus1` comes from `read_ue()` so it is at
        // most `u32::MAX`; `(payload_size_bytes * 8) + vui_start_bits` therefore
        // peaks near 0x8_FFFF_FFFF and can never overflow u64 — the u64 math
        // itself is panic-free without any checked-arithmetic guard.
        //
        // What *can* happen is a crafted payload whose declared end lands beyond
        // u32::MAX bits. The tail-skip loop below compares against `br.position()`
        // (a u32), so such an end is unreachable in a valid bitstream — reject it
        // with ReservedValue rather than spinning the loop to EOF.
        let vui_end_bits: u64 = (vui_start_bits as u64) + (payload_size_bytes as u64) * 8;
        if vui_end_bits > u32::MAX as u64 {
            return Err(CodecParseError::ReservedValue {
                field: "vui_payload_size_minus1",
                value: vui_payload_size_minus1 as u32,
            });
        }
        while (br.position() as u64) < vui_end_bits {
            // Consume the optional extension+marker+pad tail. We do not
            // interpret the bits — H.266 §7.3.2.21 reserves them for future
            // use and §7.4.2.21's `more_data_in_payload()` test allows
            // benign extension data here.
            br.read_u(1)?;
        }
        color
    } else {
        None
    };

    // §7.3.2.4 — extension flag (consume; don't model range extensions).
    let _sps_extension_flag = br.read_bool()?;

    Ok((color_info, frame_rate))
}

/// Walk `dpb_parameters(MaxSubLayersMinus1, subLayerInfoFlag)` per §7.3.4.
/// The loop runs from `(subLayerInfoFlag ? 0 : MaxSubLayersMinus1)`
/// to `MaxSubLayersMinus1`, reading 3 ue(v) fields per layer.
fn walk_dpb_parameters(
    br: &mut BitReader<'_>,
    max_sublayers_minus1: u8,
    sublayer_info_flag: bool,
) -> Result<(), CodecParseError> {
    let start = if sublayer_info_flag {
        0u8
    } else {
        max_sublayers_minus1
    };
    for _ in start..=max_sublayers_minus1 {
        let _dpb_max_dec_pic_buffering_minus1 = br.read_ue()?;
        let _dpb_max_num_reorder_pics = br.read_ue()?;
        let _dpb_max_latency_increase_plus1 = br.read_ue()?;
    }
    Ok(())
}

/// Walk `general_timing_hrd_parameters()` (§7.3.5.1) followed by
/// `ols_timing_hrd_parameters(firstSubLayer, sps_max_sublayers_minus1)`
/// (§7.3.5.2). Called only when `sps_timing_hrd_params_present_flag = 1`.
///
/// Returns `Some((num_units_in_tick, time_scale))` for frame_rate recovery
/// when the values are non-zero; `None` otherwise.
fn walk_timing_hrd_parameters(
    br: &mut BitReader<'_>,
    max_sublayers_minus1: u8,
) -> Result<Option<(u32, u32)>, CodecParseError> {
    // §7.3.5.1 general_timing_hrd_parameters().
    let num_units_in_tick = br.read_u(32)?;
    let time_scale = br.read_u(32)?;
    let general_nal_hrd_params_present_flag = br.read_bool()?;
    let general_vcl_hrd_params_present_flag = br.read_bool()?;
    let (general_du_hrd_params_present_flag, hrd_cpb_cnt_minus1) =
        if general_nal_hrd_params_present_flag || general_vcl_hrd_params_present_flag {
            let _general_same_pic_timing_in_all_ols_flag = br.read_bool()?;
            let du_hrd = br.read_bool()?; // general_du_hrd_params_present_flag
            if du_hrd {
                let _tick_divisor_minus2 = br.read_u(8)?;
            }
            let _bit_rate_scale = br.read_u(4)?;
            let _cpb_size_scale = br.read_u(4)?;
            if du_hrd {
                let _cpb_size_du_scale = br.read_u(4)?;
            }
            let cpb_cnt = br.read_ue()?; // hrd_cpb_cnt_minus1
            (du_hrd, cpb_cnt)
        } else {
            (false, 0)
        };

    // §7.3.5.2 ols_timing_hrd_parameters(firstSubLayer, MaxSubLayersVal).
    let sublayer_cpb_params_present_flag = if max_sublayers_minus1 > 0 {
        br.read_bool()?
    } else {
        false
    };
    let first_sub_layer = if sublayer_cpb_params_present_flag {
        0u8
    } else {
        max_sublayers_minus1
    };
    for _ in first_sub_layer..=max_sublayers_minus1 {
        let fixed_pic_rate_general_flag = br.read_bool()?;
        let fixed_pic_rate_within_cvs_flag = if !fixed_pic_rate_general_flag {
            br.read_bool()?
        } else {
            true
        };
        if fixed_pic_rate_within_cvs_flag {
            let _elemental_duration_in_tc_minus1 = br.read_ue()?;
        } else if (general_nal_hrd_params_present_flag || general_vcl_hrd_params_present_flag)
            && hrd_cpb_cnt_minus1 == 0
        {
            let _low_delay_hrd_flag = br.read_bool()?;
        }
        // §7.3.5.3 sublayer_hrd_parameters(subLayerId).
        // Called once per sublayer-HRD direction that has params present.
        if general_nal_hrd_params_present_flag {
            walk_sublayer_hrd_parameters(
                br,
                hrd_cpb_cnt_minus1,
                general_du_hrd_params_present_flag,
            )?;
        }
        if general_vcl_hrd_params_present_flag {
            walk_sublayer_hrd_parameters(
                br,
                hrd_cpb_cnt_minus1,
                general_du_hrd_params_present_flag,
            )?;
        }
    }
    // Both values must be non-zero for a valid frame_rate ratio.
    let timing = if num_units_in_tick > 0 && time_scale > 0 {
        Some((num_units_in_tick, time_scale))
    } else {
        None
    };
    Ok(timing)
}

/// Walk `sublayer_hrd_parameters(subLayerId)` per §7.3.5.3.
/// Reads bit_rate_value_minus1, cpb_size_value_minus1 (and optional DU
/// variants) + cbr_flag for each CPB in `0..=hrd_cpb_cnt_minus1`.
fn walk_sublayer_hrd_parameters(
    br: &mut BitReader<'_>,
    hrd_cpb_cnt_minus1: u32,
    du_hrd_params_present: bool,
) -> Result<(), CodecParseError> {
    for _ in 0..=hrd_cpb_cnt_minus1 {
        let _bit_rate_value_minus1 = br.read_ue()?;
        let _cpb_size_value_minus1 = br.read_ue()?;
        if du_hrd_params_present {
            let _cpb_size_du_value_minus1 = br.read_ue()?;
            let _bit_rate_du_value_minus1 = br.read_ue()?;
        }
        let _cbr_flag = br.read_bool()?;
    }
    Ok(())
}

/// Walk `ref_pic_list_struct(listIdx, rplsIdx)` per §7.3.10.
///
/// Advances the cursor past one reference picture list structure.
/// Long-term ref-pic entries that signal `rpls_poc_lsb_lt` require a variable
/// bit width (`u(v)` with width derived from `sps_log2_max_pic_order_cnt_lsb_minus4`);
/// since that width is not passed into this helper, the long-term non-in-header
/// path bails to `UnsupportedProfile`. In practice `sps_long_term_ref_pics_flag`
/// is `false` on VVenC default output so this path is never taken on real fixtures.
///
/// `weighted_pred_flag` and `weighted_bipred_flag` gate the `AbsDeltaPocSt`
/// +1 fallback per H.266 V4 §7.4.9 equation (150). Cross-checked against
/// ffmpeg `libavcodec/vvc/refs.c:522-526` and
/// `libavcodec/cbs_h266_syntax_template.c:464-471`.
//
// Each argument threads a distinct SPS field through the walker — grouping
// them into a context struct would obscure the spec's per-call shape.
#[allow(clippy::too_many_arguments)]
fn walk_ref_pic_list_struct(
    br: &mut BitReader<'_>,
    _list_idx: u32,
    rpls_idx: u32,
    num_ref_pic_lists: u32,
    long_term_ref_pics_flag: bool,
    inter_layer_pred_enabled_flag: bool,
    weighted_pred_flag: bool,
    weighted_bipred_flag: bool,
) -> Result<(), CodecParseError> {
    let num_ref_entries = br.read_ue()?;
    // ltrp_in_header_flag: present when long_term_ref_pics_flag is true,
    // rplsIdx < num_ref_pic_lists[listIdx], and num_ref_entries > 0.
    let ltrp_in_header_flag =
        if long_term_ref_pics_flag && rpls_idx < num_ref_pic_lists && num_ref_entries > 0 {
            br.read_bool()?
        } else {
            true // implied when not signalled
        };

    // AbsDeltaPocSt derivation per H.266 V4 §7.4.9 equation (150):
    //   if ((sps_weighted_pred_flag || sps_weighted_bipred_flag) && i != 0)
    //       AbsDeltaPocSt[ listIdx ][ rplsIdx ][ i ] = abs_delta_poc_st[ ... ][ i ]
    //   else
    //       AbsDeltaPocSt[ listIdx ][ rplsIdx ][ i ] = abs_delta_poc_st[ ... ][ i ] + 1
    // The +1 fallback ensures `strp_entry_sign_flag` is coded at `i == 0` even
    // when `abs_delta_poc_st == 0`, and continues to fire at `i >= 1` unless
    // weighted-pred signalling is enabled.
    for i in 0..num_ref_entries {
        let inter_layer_ref_pic_flag = if inter_layer_pred_enabled_flag {
            br.read_bool()?
        } else {
            false
        };
        if !inter_layer_ref_pic_flag {
            let st_ref_pic_flag = if long_term_ref_pics_flag {
                br.read_bool()? // st_ref_pic_flag[listIdx][rplsIdx][i]
            } else {
                true // short-term only when long_term_ref_pics=0
            };
            if st_ref_pic_flag {
                // Short-term entry: abs_delta_poc_st ue(v) + sign bit when
                // AbsDeltaPocSt > 0 per §7.4.9 eq.(150).
                let abs_delta_poc_st = br.read_ue()?;
                let weighted_signalling_present =
                    (weighted_pred_flag || weighted_bipred_flag) && i != 0;
                let abs_delta_poc_st_semantics = if weighted_signalling_present {
                    abs_delta_poc_st
                } else {
                    abs_delta_poc_st + 1
                };
                if abs_delta_poc_st_semantics > 0 {
                    let _strp_entry_sign_flag = br.read_bool()?;
                }
            } else if ltrp_in_header_flag {
                // Long-term entry signalled in slice header — no bits here.
            } else {
                // Long-term entry signalled in RPLS — requires poc_lsb_lt of
                // width `log2_max_pic_order_cnt_lsb_minus4 + 4` bits. We don't
                // thread that width through the call stack, so bail here.
                return Err(CodecParseError::UnsupportedProfile { profile_idc: 0 });
            }
        } else {
            // Inter-layer ref-pic: ilrp_idx ue(v).
            let _ilrp_idx = br.read_ue()?;
        }
    }
    Ok(())
}
