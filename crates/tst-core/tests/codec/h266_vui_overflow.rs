//! Regression test: H.266 SPS VUI payload size arithmetic must not overflow
//! u32 on crafted input. Reachable from `parse_sps` on network-fed NALs.
//!
//! The bug: `vui_end_bits = vui_start_bits + (8 * payload_size_bytes as u32)`
//! where `payload_size_bytes = vui_payload_size_minus1 + 1`. A crafted
//! `vui_payload_size_minus1 = 0x1FFF_FFFF` makes `payload_size_bytes =
//! 0x2000_0000` and `8 * 0x2000_0000 = 2^32` — a u32 overflow that panics
//! in debug builds. The fix must return `Err` without panicking.

use crate::common::bitwriter::BitWriter;
use tst_core::codec::CodecParseError;
use tst_core::codec::h266::parse_sps;

/// Write the fixed SPS preamble (header through timing/field flags) into `bw`.
/// Identical to `minimal_sps_rbsp` in `h266_codec_integration.rs` up to and
/// including `sps_timing_hrd_params_present_flag` and `sps_field_seq_flag`.
fn write_sps_preamble(bw: &mut BitWriter) {
    // §7.3.2.4 SPS header.
    bw.write(0, 4); // sps_seq_parameter_set_id
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sublayers_minus1
    bw.write(1, 2); // sps_chroma_format_idc = 1 (4:2:0)
    bw.write(0, 2); // sps_log2_ctu_size_minus5
    bw.write(1, 1); // sps_ptl_dpb_hrd_params_present_flag = 1

    // §7.3.3.1 profile_tier_level(profileTierPresentFlag=1, MaxNumSubLayersMinus1=0).
    bw.write(1, 7); // general_profile_idc = 1 (Main 10)
    bw.write(0, 1); // general_tier_flag = 0 (Main tier)
    bw.write(63, 8); // general_level_idc = 63 (Level 4.0)
    bw.write(0, 1); // ptl_frame_only_constraint_flag
    bw.write(0, 1); // ptl_multilayer_enabled_flag
    bw.write(0, 1); // gci_present_flag = 0
    bw.write(0, 5); // byte-align PTL to 24 bits
    bw.write(0, 8); // ptl_num_sub_profiles = 0

    bw.write(0, 1); // sps_gdr_enabled_flag
    bw.write(0, 1); // sps_ref_pic_resampling_enabled_flag = 0

    bw.write_ue(320); // sps_pic_width_max_in_luma_samples
    bw.write_ue(240); // sps_pic_height_max_in_luma_samples

    bw.write(0, 1); // sps_conformance_window_flag = 0
    bw.write(0, 1); // sps_subpic_info_present_flag = 0
    bw.write_ue(0); // sps_bitdepth_minus8 = 0 → bit_depth = 8

    // §7.3.2.4 body walk fields (all flags off, ue values = 0).
    bw.write(0, 1); // sps_entropy_coding_sync_enabled_flag
    bw.write(0, 1); // sps_entry_point_offsets_present_flag
    bw.write(0, 4); // sps_log2_max_pic_order_cnt_lsb_minus4 u(4)
    bw.write(0, 1); // sps_poc_msb_cycle_flag
    bw.write(0, 2); // sps_num_extra_ph_bytes u(2)
    bw.write(0, 2); // sps_num_extra_sh_bytes u(2)
    // ptl_dpb_hrd_present=1, max_sublayers=0 → no sublayer_dpb_params_flag
    bw.write_ue(1); // dpb_max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // dpb_max_num_reorder_pics[0]
    bw.write_ue(0); // dpb_max_latency_increase_plus1[0]
    bw.write_ue(0); // sps_log2_min_luma_coding_block_size_minus2
    bw.write(0, 1); // sps_partition_constraints_override_enabled_flag
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_intra_slice_luma
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_intra_slice_luma
    bw.write(0, 1); // sps_qtbtt_dual_tree_intra_flag (chroma_format!=0)
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_inter_slice
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_inter_slice
    // CTBsize=32 → no max_luma_transform_size_64_flag
    bw.write(0, 1); // sps_transform_skip_enabled_flag
    bw.write(0, 1); // sps_mts_enabled_flag
    bw.write(0, 1); // sps_lfnst_enabled_flag
    bw.write(0, 1); // sps_joint_cbcr_enabled_flag
    bw.write(1, 1); // sps_same_qp_table_for_chroma_flag = 1 → 1 QP table
    bw.write_ue(0); // qp_table_start_minus26[0] = se(0) → ue(0)
    bw.write_ue(0); // num_points_in_qp_table_minus1[0]
    bw.write_ue(0); // delta_qp_in_val_minus1[0][0]
    bw.write_ue(0); // delta_qp_diff_val[0][0]
    bw.write(0, 1); // sps_sao_enabled_flag
    bw.write(0, 1); // sps_alf_enabled_flag
    bw.write(0, 1); // sps_lmcs_enabled_flag
    bw.write(0, 1); // sps_weighted_pred_flag
    bw.write(0, 1); // sps_weighted_bipred_flag
    bw.write(0, 1); // sps_long_term_ref_pics_flag
    // vps_id=0 → no inter_layer_pred_enabled_flag
    bw.write(0, 1); // sps_idr_rpl_present_flag
    bw.write(0, 1); // sps_rpl1_same_as_rpl0_flag
    bw.write_ue(0); // sps_num_ref_pic_lists[0]
    bw.write_ue(0); // sps_num_ref_pic_lists[1]
    bw.write(0, 1); // sps_ref_wraparound_enabled_flag
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag
    bw.write(0, 1); // sps_amvr_enabled_flag
    bw.write(0, 1); // sps_bdof_enabled_flag
    bw.write(0, 1); // sps_smvd_enabled_flag
    bw.write(0, 1); // sps_dmvr_enabled_flag
    bw.write(0, 1); // sps_mmvd_enabled_flag
    bw.write_ue(0); // sps_six_minus_max_num_merge_cand
    bw.write(0, 1); // sps_sbt_enabled_flag
    bw.write(0, 1); // sps_affine_enabled_flag
    bw.write(0, 1); // sps_bcw_enabled_flag
    bw.write(0, 1); // sps_ciip_enabled_flag
    bw.write(0, 1); // sps_gpm_enabled_flag (MaxNumMergeCand=6 >= 2)
    bw.write_ue(0); // sps_log2_parallel_merge_level_minus2
    bw.write(0, 1); // sps_isp_enabled_flag
    bw.write(0, 1); // sps_mrl_enabled_flag
    bw.write(0, 1); // sps_mip_enabled_flag
    bw.write(0, 1); // sps_cclm_enabled_flag (chroma_format!=0)
    bw.write(0, 1); // sps_chroma_horizontal_collocated_flag (chroma==1)
    bw.write(0, 1); // sps_chroma_vertical_collocated_flag
    bw.write(0, 1); // sps_palette_enabled_flag
    bw.write(0, 1); // sps_ibc_enabled_flag
    bw.write(0, 1); // sps_ladf_enabled_flag
    bw.write(0, 1); // sps_explicit_scaling_list_enabled_flag
    bw.write(0, 1); // sps_dep_quant_enabled_flag
    bw.write(0, 1); // sps_sign_data_hiding_enabled_flag
    bw.write(0, 1); // sps_virtual_boundaries_enabled_flag
    bw.write(0, 1); // sps_timing_hrd_params_present_flag (ptl_dpb_hrd=1)
    bw.write(0, 1); // sps_field_seq_flag
}

/// Build an H.266 SPS RBSP with `sps_vui_parameters_present_flag=1` and the
/// given `vui_payload_size_minus1`. The VUI body is a minimal 1-byte block
/// (all flags off). When `write_epilogue` is true, `sps_extension_flag=0` and
/// `rbsp_trailing_bits` are appended, producing a complete parseable RBSP.
/// When false, those are omitted — the parser will hit `TruncatedRbsp` before
/// the VUI tail-skip completes (acceptable for the overflow-panic test).
fn make_sps_rbsp_with_vui(vui_payload_size_minus1: u32, write_epilogue: bool) -> Vec<u8> {
    let mut bw = BitWriter::new();
    write_sps_preamble(&mut bw);

    // sps_vui_parameters_present_flag = 1
    bw.write(1, 1);
    // vui_payload_size_minus1
    bw.write_ue(vui_payload_size_minus1);
    // §7.3.2.4 alignment: zero-pad to byte boundary before vui_parameters().
    while !bw.pos.is_multiple_of(8) {
        bw.write(0, 1);
    }
    // Minimal vui_parameters() body — all flags off (8 bits / 1 byte):
    //   vui_progressive_source_flag       u(1) = 0
    //   vui_interlaced_source_flag        u(1) = 0
    //   vui_non_packed_constraint_flag    u(1) = 0
    //   vui_non_projected_constraint_flag u(1) = 0
    //   vui_aspect_ratio_info_present_flag u(1) = 0
    //   vui_overscan_info_present_flag    u(1) = 0
    //   vui_colour_description_present_flag u(1) = 0
    //   vui_chroma_loc_info_present_flag  u(1) = 0
    bw.write(0, 8);

    if write_epilogue {
        // sps_extension_flag = 0
        bw.write(0, 1);
        bw.end_rbsp();
    }
    bw.bytes
}

/// Regression: crafted `vui_payload_size_minus1 = 0x1FFF_FFFF` (536870911)
/// causes `8 * (vui_payload_size_minus1 + 1) as u32 = 8 * 2^29 = 2^32`,
/// overflowing u32. The parser must return Err without panicking.
///
/// The fix computes `vui_end_bits` in u64 (panic-free) and then explicitly
/// rejects an end past `u32::MAX` with `ReservedValue` — the loop below
/// compares against a u32 `position()`, so such an end is unreachable in a
/// valid bitstream. This asserts that *reachable* `ReservedValue` branch
/// fires (it was dead code when guarded only by `checked_mul`/`checked_add`,
/// neither of which can overflow u64 given the bounded operands).
#[test]
fn vui_giant_payload_size_does_not_panic() {
    // 0x1FFF_FFFF: payload_size_bytes = 0x2000_0000, 8 * that = 2^32 → vui_end_bits
    // exceeds u32::MAX, so the explicit bound check returns ReservedValue.
    let rbsp = make_sps_rbsp_with_vui(0x1FFF_FFFF, false);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "vui_payload_size_minus1",
                ..
            })
        ),
        "expected ReservedValue for giant VUI payload size, got: {result:?}"
    );
}

/// Sanity check: a normal small VUI payload (1 byte declared, 1 byte written)
/// still parses successfully after the fix. The tail-skip is zero bits so the
/// parser advances cleanly to `sps_extension_flag` and RBSP end.
#[test]
fn vui_normal_payload_size_parses_ok() {
    // payload_size_bytes = 1, vui_end_bits = vui_start_bits + 8 — no overflow.
    let rbsp = make_sps_rbsp_with_vui(0, true);
    let result = parse_sps(&rbsp);
    assert!(
        result.is_ok(),
        "expected Ok for 1-byte VUI payload, got: {result:?}"
    );
    let sps = result.unwrap();
    assert_eq!(sps.width, 320);
    assert_eq!(sps.height, 240);
}
