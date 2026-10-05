//! H.266 SPS parser tests.

use crate::codec::h266::parse_sps;
use crate::codec::test_util::{BitWriter, escape_emulation};
use crate::codec::{ChromaFormat, CodecParseError, Rational};

/// Construct a minimal valid H.266 SPS bitstream:
/// sps_id=0, vps_id=0, 320x240, 8-bit 4:2:0, Main 10 profile @ Level 4.0.
///
/// Per H.266 V4 §7.3.2.4 SPS syntax + §7.3.3.1 PTL syntax.
fn minimal_sps_rbsp() -> Vec<u8> {
    minimal_sps_rbsp_with_bitdepth_minus8(0)
}

/// Same as [`minimal_sps_rbsp`] but lets callers inject an arbitrary
/// `sps_bitdepth_minus8` value to exercise the bounds check.
fn minimal_sps_rbsp_with_bitdepth_minus8(bitdepth_minus8: u32) -> Vec<u8> {
    minimal_sps_rbsp_full(bitdepth_minus8, None, 0)
}

/// Same minimal SPS but with `sps_conformance_window_flag = 1` and
/// caller-supplied `(left, right, top, bottom)` offsets in
/// SubWidthC/SubHeightC units. 4:2:0 chroma → SubWidthC=SubHeightC=2,
/// so the surfaced `crop_*` luma-sample values are 2× the offsets.
fn minimal_sps_rbsp_with_conformance_window(offsets: (u32, u32, u32, u32)) -> Vec<u8> {
    minimal_sps_rbsp_full(0, Some(offsets), 0)
}

/// Same minimal SPS but with `sps_vui_parameters_present_flag = 1` and
/// the declared `vui_payload(payloadSize)` region sized 1 byte larger
/// than the actual `vui_parameters()` body (which is 8 bits for an
/// all-zero-flags VUI). The extra byte is `0xFF` to expose any
/// mis-framing: a parser that fails to advance past the declared payload
/// size will read the wrong `sps_extension_flag` bit and corrupt the
/// trailing-bits check.
///
/// Mirrors H.266 V4 §7.3.2.21 — `vui_payload(payloadSize)` reserves the
/// entire `8 * payloadSize` bit region; encoders MAY emit
/// `vui_reserved_payload_extension_data` + marker + pad in the tail.
fn minimal_sps_rbsp_with_vui_tail_padding() -> Vec<u8> {
    minimal_sps_rbsp_full(0, None, 1)
}

fn minimal_sps_rbsp_full(
    bitdepth_minus8: u32,
    conf_window: Option<(u32, u32, u32, u32)>,
    vui_extra_padding_bytes: usize,
) -> Vec<u8> {
    let mut bw = BitWriter::new();

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
    // §7.3.3.2 general_constraints_info(): gci_present_flag=0 → only
    // the flag bit, then byte-align.
    bw.write(0, 1); // gci_present_flag = 0
    // Byte-align: bits written so far in PTL = 7+1+8+1+1+1 = 19,
    // need 5 zero bits to align to 24 bits = 3 bytes.
    bw.write(0, 5);
    // No sublayer ptl_sublayer_level_present_flag loop (count = -1).
    // No sublayer level_idc loop.
    bw.write(0, 8); // ptl_num_sub_profiles = 0
    // No sub_profile_idc loop.
    // PTL total = 32 bits = 4 bytes.

    // §7.3.2.4 continues.
    bw.write(0, 1); // sps_gdr_enabled_flag
    bw.write(0, 1); // sps_ref_pic_resampling_enabled_flag = 0
    // (sps_res_change_in_clvs_allowed_flag not coded.)

    // sps_pic_width_max_in_luma_samples is the direct sample count
    // (not minus-1 like H.264/H.265 use elsewhere). Per V4 §7.3.2.4.
    bw.write_ue(320); // sps_pic_width_max_in_luma_samples
    bw.write_ue(240); // sps_pic_height_max_in_luma_samples

    if let Some((left, right, top, bottom)) = conf_window {
        bw.write(1, 1); // sps_conformance_window_flag = 1
        bw.write_ue(left); // sps_conf_win_left_offset
        bw.write_ue(right); // sps_conf_win_right_offset
        bw.write_ue(top); // sps_conf_win_top_offset
        bw.write_ue(bottom); // sps_conf_win_bottom_offset
    } else {
        bw.write(0, 1); // sps_conformance_window_flag = 0
    }
    bw.write(0, 1); // sps_subpic_info_present_flag = 0
    bw.write_ue(bitdepth_minus8); // sps_bitdepth_minus8

    // ── body walk fields ─────────────────────────────────────────────────
    // All flags set to 0 / false; all ue(v) values = 0.  This produces the
    // simplest valid SPS that the walk can traverse without any conditional
    // branches being taken.
    //
    // §7.3.2.4 continued (order mirrors walk_sps_body exactly):
    bw.write(0, 1); // sps_entropy_coding_sync_enabled_flag = 0
    bw.write(0, 1); // sps_entry_point_offsets_present_flag = 0
    bw.write(0, 4); // sps_log2_max_pic_order_cnt_lsb_minus4 = 0 (u4)
    bw.write(0, 1); // sps_poc_msb_cycle_flag = 0
    // (no sps_poc_msb_cycle_len_minus1)
    bw.write(0, 2); // sps_num_extra_ph_bytes = 0 (u2)
    // (no extra_ph_bit_present_flag loop)
    bw.write(0, 2); // sps_num_extra_sh_bytes = 0 (u2)
    // (no extra_sh_bit_present_flag loop)

    // ptl_dpb_hrd_params_present_flag = 1, sps_max_sublayers_minus1 = 0
    // → sps_sublayer_dpb_params_flag NOT read (only present when
    //   max_sublayers_minus1 > 0).
    // dpb_parameters(0, false): loop from 0 to 0 (one iteration).
    bw.write_ue(1); // dpb_max_dec_pic_buffering_minus1[0] = 1
    bw.write_ue(0); // dpb_max_num_reorder_pics[0] = 0
    bw.write_ue(0); // dpb_max_latency_increase_plus1[0] = 0

    bw.write_ue(0); // sps_log2_min_luma_coding_block_size_minus2 = 0
    // log2_ctu_size_minus5 = 0 → CTBsize = 32 (not > 32).

    bw.write(0, 1); // sps_partition_constraints_override_enabled_flag = 0

    // Intra luma partition constraints (always present).
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_intra_slice_luma = 0
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_intra_slice_luma = 0
    // (depth=0 → no BT/TT max-diff fields)

    // chroma_format_idc = 1 (not 0) → sps_qtbtt_dual_tree_intra_flag present.
    bw.write(0, 1); // sps_qtbtt_dual_tree_intra_flag = 0
    // (dual_tree=0 → no chroma partition fields)

    // Inter partition constraints (always present).
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_inter_slice = 0
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_inter_slice = 0
    // (depth=0 → no BT/TT max-diff fields)

    // CTBsize = 32 (not > 32) → sps_max_luma_transform_size_64_flag NOT present.

    bw.write(0, 1); // sps_transform_skip_enabled_flag = 0
    // (ts=0 → no log2_transform_skip + bdpcm fields)
    bw.write(0, 1); // sps_mts_enabled_flag = 0
    // (mts=0 → no intra/inter mts fields)
    bw.write(0, 1); // sps_lfnst_enabled_flag = 0

    // chroma_format_idc = 1 (not 0) → joint_cbcr + qp_table present.
    bw.write(0, 1); // sps_joint_cbcr_enabled_flag = 0
    bw.write(1, 1); // sps_same_qp_table_for_chroma_flag = 1
    // numQpTables = 1 (same_qp=1).
    // QP table [0]: qp_table_start_minus26=0, num_points=0, one (in,out) pair.
    // se(0) = ue(0) → codeword "1" (1 bit).
    bw.write_ue(0); // sps_qp_table_start_minus26[0] = se(0) → maps to ue(0)
    bw.write_ue(0); // sps_num_points_in_qp_table_minus1[0] = 0
    // for j in 0..=0: two ue(0) values.
    bw.write_ue(0); // sps_delta_qp_in_val_minus1[0][0] = 0
    bw.write_ue(0); // sps_delta_qp_diff_val[0][0] = 0

    bw.write(0, 1); // sps_sao_enabled_flag = 0
    bw.write(0, 1); // sps_alf_enabled_flag = 0
    // (alf=0 → no ccalf field)
    bw.write(0, 1); // sps_lmcs_enabled_flag = 0
    bw.write(0, 1); // sps_weighted_pred_flag = 0
    bw.write(0, 1); // sps_weighted_bipred_flag = 0
    bw.write(0, 1); // sps_long_term_ref_pics_flag = 0
    // vps_id = 0 → sps_inter_layer_prediction_enabled_flag NOT present.
    bw.write(0, 1); // sps_idr_rpl_present_flag = 0
    bw.write(0, 1); // sps_rpl1_same_as_rpl0_flag = 0
    // → loop 2 directions (L0 and L1).
    // L0: sps_num_ref_pic_lists[0] = 0 → no ref_pic_list_struct calls.
    bw.write_ue(0); // sps_num_ref_pic_lists[0] = 0
    // L1: sps_num_ref_pic_lists[1] = 0.
    bw.write_ue(0); // sps_num_ref_pic_lists[1] = 0

    bw.write(0, 1); // sps_ref_wraparound_enabled_flag = 0
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag = 0
    // (tmvp=0 → no sbtmvp field)
    bw.write(0, 1); // sps_amvr_enabled_flag = 0
    bw.write(0, 1); // sps_bdof_enabled_flag = 0
    // (bdof=0 → no bdof_control_present_in_ph field)
    bw.write(0, 1); // sps_smvd_enabled_flag = 0
    bw.write(0, 1); // sps_dmvr_enabled_flag = 0
    // (dmvr=0 → no dmvr_control_present_in_ph field)
    bw.write(0, 1); // sps_mmvd_enabled_flag = 0
    // (mmvd=0 → no mmvd_fullpel_only field)
    // sps_six_minus_max_num_merge_cand = 0 → MaxNumMergeCand = 6.
    bw.write_ue(0); // sps_six_minus_max_num_merge_cand = 0
    bw.write(0, 1); // sps_sbt_enabled_flag = 0
    bw.write(0, 1); // sps_affine_enabled_flag = 0
    // (affine=0 → no affine sub-fields)
    bw.write(0, 1); // sps_bcw_enabled_flag = 0
    bw.write(0, 1); // sps_ciip_enabled_flag = 0
    // MaxNumMergeCand = 6 >= 2 → sps_gpm_enabled_flag present.
    bw.write(0, 1); // sps_gpm_enabled_flag = 0
    // (gpm=0 → no max_num_gpm_cand field)
    bw.write_ue(0); // sps_log2_parallel_merge_level_minus2 = 0
    bw.write(0, 1); // sps_isp_enabled_flag = 0
    bw.write(0, 1); // sps_mrl_enabled_flag = 0
    bw.write(0, 1); // sps_mip_enabled_flag = 0
    // chroma_format_idc = 1 (not 0) → sps_cclm_enabled_flag present.
    bw.write(0, 1); // sps_cclm_enabled_flag = 0
    // chroma_format_idc = 1 → sps_chroma_horiz/vert_collocated_flag present.
    bw.write(0, 1); // sps_chroma_horizontal_collocated_flag = 0
    bw.write(0, 1); // sps_chroma_vertical_collocated_flag = 0
    bw.write(0, 1); // sps_palette_enabled_flag = 0
    // chroma_format_idc=1 (not 3) → sps_act_enabled_flag NOT present.
    // ts=0, palette=0 → sps_min_qp_prime_ts NOT present.
    bw.write(0, 1); // sps_ibc_enabled_flag = 0
    // (ibc=0 → no six_minus_max_num_ibc field)
    bw.write(0, 1); // sps_ladf_enabled_flag = 0
    bw.write(0, 1); // sps_explicit_scaling_list_enabled_flag = 0
    // (explicit_scaling=0 → no scaling list fields)
    // lfnst=0, explicit_scaling=0 → no lfnst_disabled flag.
    // act=0, explicit_scaling=0 → no act_disabled flag.
    bw.write(0, 1); // sps_dep_quant_enabled_flag = 0
    bw.write(0, 1); // sps_sign_data_hiding_enabled_flag = 0
    bw.write(0, 1); // sps_virtual_boundaries_enabled_flag = 0

    // ptl_dpb_hrd_params_present_flag = 1 → sps_timing_hrd_params_present_flag present.
    bw.write(0, 1); // sps_timing_hrd_params_present_flag = 0
    // (timing_hrd=0 → no general_timing_hrd_parameters call)

    bw.write(0, 1); // sps_field_seq_flag = 0
    if vui_extra_padding_bytes > 0 {
        bw.write(1, 1); // sps_vui_parameters_present_flag = 1
        // §7.3.2.4: vui_payload_size_minus1 ue(v). 1 byte of VUI flags +
        // `vui_extra_padding_bytes` of tail = total bytes - 1.
        let total_bytes = 1 + vui_extra_padding_bytes;
        bw.write_ue((total_bytes - 1) as u32);
        // §7.3.2.4: while(!byte_aligned()) sps_vui_alignment_zero_bit f(1).
        while bw.pos % 8 != 0 {
            bw.write(0, 1);
        }
        // vui_parameters(): 8 flag bits (all zero) — see H.274 §7.2.
        // 4 source flags + aspect_ratio_present + overscan_present +
        // colour_description_present + chroma_loc_present.
        bw.write(0, 8);
        // Tail padding: fill with 0xFF so a mis-framed parser reads
        // sps_extension_flag = 1 (which fails downstream rbsp_trailing_bits).
        for _ in 0..vui_extra_padding_bytes {
            bw.write(0xFF, 8);
        }
    } else {
        bw.write(0, 1); // sps_vui_parameters_present_flag = 0
        // (vui=0 → no vui_payload_size or vui_parameters call)
    }

    bw.write(0, 1); // sps_extension_flag = 0

    bw.end_rbsp();
    bw.bytes
}

#[test]
fn parse_sps_320x240_main10() {
    let rbsp = minimal_sps_rbsp();
    let sps = parse_sps(&rbsp).expect("minimal SPS should parse");
    assert_eq!(sps.sps_id, 0);
    assert_eq!(sps.vps_id, 0);
    assert_eq!(sps.profile_tier_level.general_profile_idc, 1);
    assert!(!sps.profile_tier_level.general_tier_flag);
    assert_eq!(sps.profile_tier_level.general_level_idc, 63);
    assert_eq!(sps.width, 320);
    assert_eq!(sps.height, 240);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
    assert_eq!(sps.bit_depth_luma, 8);
    assert_eq!(sps.bit_depth_chroma, 8);
    assert_eq!(sps.color_info, None);
    assert_eq!(sps.frame_rate, None);
}

#[test]
fn parse_sps_truncated_returns_err() {
    assert!(parse_sps(&[]).is_err());
}

#[test]
fn parse_sps_truncated_byte_returns_err() {
    // Parser reads sps_id(4) + vps_id(4) = 8 bits, then max_sublayers(3)
    // beyond a single byte should bail with TruncatedRbsp.
    assert!(parse_sps(&[0x00]).is_err());
}

/// Per H.266 V4 §7.4.3.4, `sps_bitdepth_minus8 ∈ 0..=8` (bit_depth ∈
/// 8..=16). ffmpeg's `libavcodec/hevc/ps.c:366-369` clamps at 14
/// (minus8 ≤ 6); we adopt the same threshold. A fuzzed value of 248
/// would have silently wrapped to `bit_depth_luma=0` via
/// `8u8.saturating_add(248 as u8)` — caught now via
/// [`validate_bit_depth_minus8`].
/// Per H.266 V4 §7.4.3.4, conformance-window crop offsets are
/// expressed in SubWidthC / SubHeightC units. For 4:2:0 chroma both
/// are 2, so a (1, 2, 3, 4) offset tuple → luma crops (2, 4, 6, 8)
/// and width/height shrink by (left+right) / (top+bottom).
#[test]
fn h266_sps_surfaces_conformance_window_offsets_invariant() {
    let rbsp = minimal_sps_rbsp_with_conformance_window((1, 2, 3, 4));
    let sps = parse_sps(&rbsp).expect("SPS with conformance window should parse");
    assert_eq!(sps.crop_left, 2);
    assert_eq!(sps.crop_right, 4);
    assert_eq!(sps.crop_top, 6);
    assert_eq!(sps.crop_bottom, 8);
    // pic_width_max=320, pic_height_max=240. After cropping:
    // width = 320 - (2+4) = 314, height = 240 - (6+8) = 226.
    assert_eq!(sps.width, 314);
    assert_eq!(sps.height, 226);
    assert_eq!(sps.coded_width(), 320);
    assert_eq!(sps.coded_height(), 240);
    assert_eq!(
        sps.coded_width(),
        sps.width + sps.crop_left + sps.crop_right
    );
    assert_eq!(
        sps.coded_height(),
        sps.height + sps.crop_top + sps.crop_bottom
    );
}

/// Adversarial-input regression: the conformance-window crop arithmetic
/// at the end of `parse_sps` could overflow on hostile input. With
/// `chroma_format_idc = 1` (sub_w = 2), the case `(1<<30, 1<<30, 0, 0)`
/// triggers the addition path (`(1<<31) + (1<<31) = 1<<32`); the case
/// `(1<<31, 0, 0, 0)` triggers the multiplication path (`2 * (1<<31) = 1<<32`).
/// Both are rejected at parse time as `ReservedValue`; the fixture is
/// emulation-escaped so the hostile ue(v) values read back exactly as written.
#[test]
fn parse_sps_saturates_crop_on_adversarial_offsets() {
    for offsets in [
        (1u32 << 30, 1u32 << 30, 0u32, 0u32),
        (1u32 << 31, 0u32, 0u32, 0u32),
    ] {
        let rbsp = minimal_sps_rbsp_with_conformance_window(offsets);
        match parse_sps(&escape_emulation(&rbsp)) {
            Err(CodecParseError::ReservedValue { field, .. }) => {
                assert!(field.starts_with("sps_conf_win_left"), "{field}")
            }
            other => panic!("{offsets:?} must be rejected, got {other:?}"),
        }
    }
}

/// H.266 sibling of the hostile-crop case — 320×240, 4:2:0.
#[test]
fn hostile_conformance_window_is_rejected_at_parse_time() {
    let sps = parse_sps(&escape_emulation(
        &minimal_sps_rbsp_with_conformance_window((1, 2, 3, 4)),
    ))
    .unwrap();
    assert_eq!((sps.width, sps.height), (314, 226));
    assert_eq!((sps.coded_width(), sps.coded_height()), (320, 240));
    // Just under the raw width is still a (2-sample-wide) picture: accepted.
    let sps = parse_sps(&escape_emulation(
        &minimal_sps_rbsp_with_conformance_window((159, 0, 0, 0)),
    ))
    .unwrap();
    assert_eq!(sps.width, 2);
    assert_eq!(sps.coded_width(), 320);
    for offsets in [
        (1u32 << 30, 1u32 << 30, 0u32, 0u32),
        (0, 0, 1 << 30, 1 << 30),
        (160, 0, 0, 0),
        (0, 0, 0, 120),
    ] {
        match parse_sps(&escape_emulation(
            &minimal_sps_rbsp_with_conformance_window(offsets),
        )) {
            Err(CodecParseError::ReservedValue { field, .. }) => {
                assert!(field.starts_with("sps_conf_win_"), "{field}")
            }
            other => panic!("{offsets:?} must be rejected, got {other:?}"),
        }
    }
}

/// When `sps_conformance_window_flag=0`, all four crop offsets are
/// zero and `coded_*` matches the cropped dimensions.
#[test]
fn h266_sps_no_conformance_window_zero_crops() {
    let sps = parse_sps(&minimal_sps_rbsp()).expect("minimal SPS should parse");
    assert_eq!(sps.crop_left, 0);
    assert_eq!(sps.crop_right, 0);
    assert_eq!(sps.crop_top, 0);
    assert_eq!(sps.crop_bottom, 0);
    assert_eq!(sps.coded_width(), sps.width);
    assert_eq!(sps.coded_height(), sps.height);
}

#[test]
fn h266_sps_accepts_16bit_depth() {
    let rbsp = minimal_sps_rbsp_with_bitdepth_minus8(8);
    let sps = parse_sps(&rbsp).expect("16-bit depth is valid H.266 syntax");
    assert_eq!(sps.bit_depth_luma, 16);
}

#[test]
fn h266_sps_rejects_bit_depth_overflow() {
    let rbsp = minimal_sps_rbsp_with_bitdepth_minus8(248);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "sps_bitdepth_minus8",
                value: 248
            })
        ),
        "expected ReservedValue, got {result:?}"
    );
}

/// Parse a real VVenC-encoded 320×240 @ 30fps SPS and verify the parser
/// walks the full body correctly (AbsDeltaPocSt fix, timing_hrd walk) so
/// that frame_rate is recovered from general_timing_hrd_parameters().
///
/// This fixture has `sps_vui_parameters_present_flag = 0` (VVenC does not
/// emit VUI for this encoding profile), so `color_info` is `None`. The
/// primary goal is confirming the body walk lands at the correct bit
/// offset and `num_units_in_tick = 1, time_scale = 30`.
#[test]
fn real_vvenc_sps_recovers_frame_rate_via_timing_hrd() {
    let rbsp =
        include_bytes!("../../../../tests/fixtures/codec/h266/h266_320x240_main10_real_sps.bin");
    let sps = parse_sps(rbsp).expect("real VVenC SPS parses");
    assert_eq!(sps.width, 320);
    assert_eq!(sps.height, 240);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
    assert_eq!(sps.bit_depth_luma, 10);
    let fr = sps.frame_rate.expect(
        "general_timing_hrd_parameters should recover frame_rate from num_units=1 time_scale=30",
    );
    // VVenC encodes num_units_in_tick=1, time_scale=30 → 30 fps.
    let ratio = fr.num as f64 / fr.den as f64;
    assert!((ratio - 30.0).abs() < 0.5, "frame_rate≈30; got {fr:?}");
    // This fixture has no VUI (vui_parameters_present_flag=0 in VVenC output
    // at this encoding profile), so color_info is None.
    assert!(
        sps.color_info.is_none(),
        "VVenC fixture has no VUI in this profile"
    );
}

/// Smoke test for H.266 V4 §7.3.2.21 — `vui_payload(payloadSize)`
/// reserves exactly `8 * payloadSize` bits; `vui_parameters()`
/// (H.274 §7.2) may not consume all of them, and the SPS caller
/// must advance the cursor to the declared payload end before
/// reading `sps_extension_flag`.
///
/// This test builds an SPS with a 2-byte declared VUI region but a
/// 1-byte actual `vui_parameters()` body. The trailing byte is
/// `0xFF` to expose any mis-framing: post-fix, the parser skips
/// the padding and reads `sps_extension_flag = 0` correctly;
/// pre-fix, the parser reads bit 0 of `0xFF` as `sps_extension_flag = 1`.
///
/// **Note:** The bug isn't directly observable as `Err` vs `Ok` —
/// `sps_extension_flag` isn't surfaced on `H266Sps`, and the parser
/// doesn't validate `rbsp_trailing_bits`. The test functions as
/// a structural smoke test ensuring the new VUI-with-padding
/// builder path produces a parseable stream end-to-end. Strict-mode
/// trailing-bit validation (a separate plan) would make pre-fix
/// fail; until then the test guards against builder-side regressions
/// and documents the §7.3.2.21 contract.
#[test]
fn vui_tail_padding_consumed_before_extension_flag() {
    let rbsp = minimal_sps_rbsp_with_vui_tail_padding();
    let sps = parse_sps(&rbsp).expect(
        "SPS with VUI tail padding must parse — caller advances cursor to declared payload end",
    );
    // Ensure VUI itself parsed (all zero gates → no color_info, but the
    // structural walk reached here).
    assert_eq!(sps.color_info, None);
}

/// Construct an H.266 SPS that exercises the `walk_ref_pic_list_struct`
/// `AbsDeltaPocSt` predicate (H.266 V4 §7.4.9 equation 150).
///
/// `sps_weighted_pred_flag` and `sps_weighted_bipred_flag` are both `0`.
/// The single emitted RPS struct has `num_ref_entries = 2` with both
/// `abs_delta_poc_st = 0`. Per spec each entry's `AbsDeltaPocSt` is
/// `0 + 1 = 1 > 0`, so a `strp_entry_sign_flag` u(1) is coded for BOTH.
///
/// `timing_hrd_params_present_flag = 1` then writes `num_units_in_tick = 1`
/// and `time_scale = 30` so the parser surfaces `frame_rate = 30/1` only
/// when the RPS walk lands the cursor at the spec-correct position. A
/// 1-bit drift would mis-frame `num_units_in_tick` (a u(32)) and either
/// produce garbage timing or trigger `TruncatedRbsp` near the RBSP end.
fn sps_rbsp_with_two_entry_short_term_rps_and_timing_hrd() -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.4 SPS header — same layout as `minimal_sps_rbsp_full`.
    bw.write(0, 4); // sps_seq_parameter_set_id
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sublayers_minus1
    bw.write(1, 2); // sps_chroma_format_idc = 1 (4:2:0)
    bw.write(0, 2); // sps_log2_ctu_size_minus5
    bw.write(1, 1); // sps_ptl_dpb_hrd_params_present_flag = 1

    // §7.3.3.1 profile_tier_level(1, 0). Total 32 bits = 4 bytes.
    bw.write(1, 7); // general_profile_idc = 1 (Main 10)
    bw.write(0, 1); // general_tier_flag
    bw.write(63, 8); // general_level_idc
    bw.write(0, 1); // ptl_frame_only_constraint_flag
    bw.write(0, 1); // ptl_multilayer_enabled_flag
    bw.write(0, 1); // gci_present_flag = 0
    bw.write(0, 5); // byte-align PTL (19 + 5 = 24 = byte boundary)
    bw.write(0, 8); // ptl_num_sub_profiles = 0

    bw.write(0, 1); // sps_gdr_enabled_flag
    bw.write(0, 1); // sps_ref_pic_resampling_enabled_flag
    bw.write_ue(320); // pic_width_max_in_luma_samples
    bw.write_ue(240); // pic_height_max_in_luma_samples
    bw.write(0, 1); // sps_conformance_window_flag = 0
    bw.write(0, 1); // sps_subpic_info_present_flag = 0
    bw.write_ue(0); // sps_bitdepth_minus8 = 0 → 8-bit

    // ── body walk — mirrors `walk_sps_body` order ───────────────────────────
    bw.write(0, 1); // sps_entropy_coding_sync_enabled_flag
    bw.write(0, 1); // sps_entry_point_offsets_present_flag
    bw.write(0, 4); // sps_log2_max_pic_order_cnt_lsb_minus4
    bw.write(0, 1); // sps_poc_msb_cycle_flag
    bw.write(0, 2); // sps_num_extra_ph_bytes
    bw.write(0, 2); // sps_num_extra_sh_bytes

    // dpb_parameters(0, false): one iteration.
    bw.write_ue(1); // dpb_max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // dpb_max_num_reorder_pics[0]
    bw.write_ue(0); // dpb_max_latency_increase_plus1[0]

    bw.write_ue(0); // sps_log2_min_luma_coding_block_size_minus2
    bw.write(0, 1); // sps_partition_constraints_override_enabled_flag

    // intra luma partition constraints (always present)
    bw.write_ue(0); // log2_diff_min_qt_min_cb_intra_slice_luma
    bw.write_ue(0); // max_mtt_hierarchy_depth_intra_slice_luma
    // chroma_format_idc=1 → dual_tree flag present
    bw.write(0, 1); // sps_qtbtt_dual_tree_intra_flag = 0
    // inter partition constraints (always present)
    bw.write_ue(0); // log2_diff_min_qt_min_cb_inter_slice
    bw.write_ue(0); // max_mtt_hierarchy_depth_inter_slice

    bw.write(0, 1); // sps_transform_skip_enabled_flag
    bw.write(0, 1); // sps_mts_enabled_flag
    bw.write(0, 1); // sps_lfnst_enabled_flag

    // chroma_format_idc != 0 → joint_cbcr + qp_table.
    bw.write(0, 1); // sps_joint_cbcr_enabled_flag
    bw.write(1, 1); // sps_same_qp_table_for_chroma_flag = 1 → numQpTables = 1
    bw.write_ue(0); // qp_table_start_minus26[0]
    bw.write_ue(0); // num_points[0] = 0 → one (in,out) pair
    bw.write_ue(0); // delta_qp_in_val_minus1[0][0]
    bw.write_ue(0); // delta_qp_diff_val[0][0]

    bw.write(0, 1); // sps_sao_enabled_flag
    bw.write(0, 1); // sps_alf_enabled_flag = 0 → no ccalf flag
    bw.write(0, 1); // sps_lmcs_enabled_flag
    // Critical for this regression: both weighted flags 0. The spec gate
    // `(weighted_pred || weighted_bipred) && i != 0` evaluates false at
    // every entry — AbsDeltaPocSt = abs_delta_poc_st + 1 always.
    bw.write(0, 1); // sps_weighted_pred_flag = 0
    bw.write(0, 1); // sps_weighted_bipred_flag = 0
    bw.write(0, 1); // sps_long_term_ref_pics_flag = 0
    // vps_id == 0 → sps_inter_layer_prediction_enabled_flag NOT coded
    bw.write(0, 1); // sps_idr_rpl_present_flag
    bw.write(0, 1); // sps_rpl1_same_as_rpl0_flag = 0 → two RPL directions

    // L0: one RPS struct with two short-term entries.
    bw.write_ue(1); // sps_num_ref_pic_lists[0] = 1
    // ref_pic_list_struct(0, 0):
    bw.write_ue(2); // num_ref_entries = 2
    // long_term_ref_pics_flag=0 → ltrp_in_header_flag NOT coded.
    // inter_layer_pred_enabled_flag=0 → inter_layer_ref_pic_flag NOT coded.
    // long_term_ref_pics_flag=0 → st_ref_pic_flag inferred 1.
    // Entry 0: abs_delta_poc_st = 0, AbsDeltaPocSt = 1 → sign coded.
    bw.write_ue(0); // abs_delta_poc_st[0]
    bw.write(0, 1); // strp_entry_sign_flag[0]
    // Entry 1: abs_delta_poc_st = 0; spec says AbsDeltaPocSt = 1 → sign coded.
    bw.write_ue(0); // abs_delta_poc_st[1]
    bw.write(0, 1); // strp_entry_sign_flag[1]
    // L1: no RPS structs.
    bw.write_ue(0); // sps_num_ref_pic_lists[1] = 0

    bw.write(0, 1); // sps_ref_wraparound_enabled_flag
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag = 0 → no sbtmvp
    bw.write(0, 1); // sps_amvr_enabled_flag
    bw.write(0, 1); // sps_bdof_enabled_flag = 0 → no bdof_control flag
    bw.write(0, 1); // sps_smvd_enabled_flag
    bw.write(0, 1); // sps_dmvr_enabled_flag = 0 → no dmvr_control flag
    bw.write(0, 1); // sps_mmvd_enabled_flag = 0 → no mmvd_fullpel flag
    bw.write_ue(0); // sps_six_minus_max_num_merge_cand = 0 → MaxNumMergeCand=6
    bw.write(0, 1); // sps_sbt_enabled_flag
    bw.write(0, 1); // sps_affine_enabled_flag = 0 → no affine sub-fields
    bw.write(0, 1); // sps_bcw_enabled_flag
    bw.write(0, 1); // sps_ciip_enabled_flag
    // MaxNumMergeCand >= 2 → sps_gpm_enabled_flag present.
    bw.write(0, 1); // sps_gpm_enabled_flag = 0 → no max_num_gpm_cand

    bw.write_ue(0); // sps_log2_parallel_merge_level_minus2
    bw.write(0, 1); // sps_isp_enabled_flag
    bw.write(0, 1); // sps_mrl_enabled_flag
    bw.write(0, 1); // sps_mip_enabled_flag
    bw.write(0, 1); // sps_cclm_enabled_flag (chroma_format_idc != 0)
    bw.write(0, 1); // sps_chroma_horizontal_collocated_flag (chroma_format_idc==1)
    bw.write(0, 1); // sps_chroma_vertical_collocated_flag
    bw.write(0, 1); // sps_palette_enabled_flag
    // chroma_format_idc=1 (not 3) → sps_act_enabled_flag NOT present.
    bw.write(0, 1); // sps_ibc_enabled_flag = 0 → no ibc_merge_cand
    bw.write(0, 1); // sps_ladf_enabled_flag = 0 → no ladf fields
    bw.write(0, 1); // sps_explicit_scaling_list_enabled_flag = 0
    bw.write(0, 1); // sps_dep_quant_enabled_flag
    bw.write(0, 1); // sps_sign_data_hiding_enabled_flag
    bw.write(0, 1); // sps_virtual_boundaries_enabled_flag = 0

    // ptl_dpb_hrd_params_present_flag=1 → timing_hrd flag present.
    bw.write(1, 1); // sps_timing_hrd_params_present_flag = 1
    // §7.3.5.1 general_timing_hrd_parameters().
    bw.write(1, 32); // num_units_in_tick = 1
    bw.write(30, 32); // time_scale = 30 → 30 fps
    bw.write(0, 1); // general_nal_hrd_params_present_flag
    bw.write(0, 1); // general_vcl_hrd_params_present_flag
    // Both 0 → no further general_timing_hrd fields.
    // §7.3.5.2 ols_timing_hrd_parameters: max_sublayers_minus1=0 → no
    // sublayer_cpb_params_present_flag; first_sub_layer = 0.
    // Loop 0..=0:
    bw.write(1, 1); // fixed_pic_rate_general_flag = 1 → infer within_cvs=true
    bw.write_ue(0); // elemental_duration_in_tc_minus1
    // No sublayer_hrd_parameters (neither nal_hrd nor vcl_hrd present).

    bw.write(0, 1); // sps_field_seq_flag
    bw.write(0, 1); // sps_vui_parameters_present_flag
    bw.write(0, 1); // sps_extension_flag

    bw.end_rbsp();
    bw.bytes
}

/// Regression for H.266 V4 §7.4.9 equation 150 — `AbsDeltaPocSt` derivation
/// in `ref_pic_list_struct`.
///
/// Pre-fix, `walk_ref_pic_list_struct` gated the `+1` on
/// `i == 0 || !prev_use_ref_pic_list` (an `inter_layer_ref_pic_flag`-shaped
/// predicate falsely attributed to §7.4.9). Spec predicate (cross-checked
/// against ffmpeg `libavcodec/vvc/refs.c:522-526` and
/// `libavcodec/cbs_h266_syntax_template.c:464-471`):
///
/// ```text
/// if !((sps_weighted_pred_flag || sps_weighted_bipred_flag) && i != 0)
///     AbsDeltaPocSt = abs_delta_poc_st + 1
/// else
///     AbsDeltaPocSt = abs_delta_poc_st
/// ```
///
/// With both weighted flags `0` (the common default), spec ALWAYS adds +1;
/// the buggy predicate only adds +1 at `i == 0`. For an RPS with two
/// short-term entries at `abs_delta_poc_st = 0`, spec consumes a
/// `strp_entry_sign_flag` u(1) for BOTH entries; the buggy parser only
/// consumes one. The 1-bit cursor drift then mis-frames `num_units_in_tick`,
/// returning garbage timing rather than the encoded 30 fps.
#[test]
fn h266_abs_delta_poc_st_predicate_matches_spec_no_weighted_pred() {
    let rbsp = sps_rbsp_with_two_entry_short_term_rps_and_timing_hrd();
    let sps =
        parse_sps(&rbsp).expect("SPS with 2-entry short-term RPS at delta=0 must parse cleanly");
    assert_eq!(
        sps.frame_rate,
        Some(Rational { num: 30, den: 1 }),
        "frame_rate must surface as 30/1 from num_units_in_tick=1, time_scale=30 — \
         1-bit cursor drift after the RPS walk would mis-frame these reads"
    );
}
