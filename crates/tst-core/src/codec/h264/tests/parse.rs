//! H.264 parameter-set parser tests.

use crate::codec::NalUnit;
use crate::codec::h264::{parse_parameter_sets, parse_pps, parse_slice_header_light, parse_sps};
use crate::codec::test_util::{BitWriter, escape_emulation};
use crate::codec::{
    ChromaFormat, CodecParseError, ColourPrimaries, MatrixCoefficients, Rational,
    TransferCharacteristics,
};
use crate::test_prelude::*;

fn nal_h264(nal_type: u8, payload: Vec<u8>) -> NalUnit {
    NalUnit::H264 {
        nal_type,
        ref_idc: 3,
        payload: payload.into(),
    }
}

const SPS_1080P_HIGH40: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h264/h264_1080p_high40_bt709_sps.bin");
const SPS_720P_MAIN31: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h264/h264_720p_main31_sps.bin");

#[test]
fn parse_sps_1080p_high_dimensions() {
    let sps = parse_sps(SPS_1080P_HIGH40).expect("parse 1080p SPS");
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.profile_idc, 100);
    assert_eq!(sps.level_idc, 40);
    assert_eq!(sps.bit_depth_luma, 8);
    assert_eq!(sps.bit_depth_chroma, 8);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
    assert!(sps.frame_mbs_only);
    assert_eq!(sps.seq_parameter_set_id, 0);
}

#[test]
fn parse_sps_720p_main_dimensions() {
    let sps = parse_sps(SPS_720P_MAIN31).expect("parse 720p SPS");
    assert_eq!(sps.width, 1280);
    assert_eq!(sps.height, 720);
    assert_eq!(sps.profile_idc, 77);
    assert_eq!(sps.level_idc, 31);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
}

#[test]
fn parse_sps_1080p_high_color_bt709() {
    let sps = parse_sps(SPS_1080P_HIGH40).expect("parse 1080p SPS");
    let color = sps.color.expect("VUI present in 1080p fixture");
    assert_eq!(color.primaries, ColourPrimaries::Bt709);
    assert_eq!(color.transfer, TransferCharacteristics::Bt709);
    assert_eq!(color.matrix, MatrixCoefficients::Bt709);
}

#[test]
fn parse_sps_preserves_raw_rbsp() {
    let sps = parse_sps(SPS_1080P_HIGH40).expect("parse");
    assert_eq!(sps.raw_rbsp, SPS_1080P_HIGH40);
}

#[test]
fn parse_sps_surfaces_frame_crop_offsets_invariant() {
    // Invariant: post-crop dims + crop offsets reconstruct the coded
    // dimensions exactly. Holds whether or not the fixture has
    // frame_cropping_flag set (uncropped → all four offsets are zero).
    for bytes in [SPS_1080P_HIGH40, SPS_720P_MAIN31] {
        let sps = parse_sps(bytes).expect("parse");
        assert_eq!(
            sps.coded_width(),
            sps.width + sps.crop_left + sps.crop_right,
            "coded_width helper must agree with field arithmetic"
        );
        assert_eq!(
            sps.coded_height(),
            sps.height + sps.crop_top + sps.crop_bottom,
            "coded_height helper must agree with field arithmetic"
        );
        // Coded dims are always macroblock-aligned (16x16) — H.264
        // pic_width_in_mbs × pic_height_in_map_units.
        assert_eq!(sps.coded_width() % 16, 0, "coded_width must be MB-aligned");
        assert_eq!(
            sps.coded_height() % 16,
            0,
            "coded_height must be MB-aligned"
        );
    }
}

#[test]
fn parse_sps_1080p_has_bottom_crop() {
    // The 1080p x264 fixture is coded as 1920×1088 (68 MB rows) and
    // signals frame_cropping_flag with bottom_offset that lops off the
    // 8 extra luma samples. crop_bottom is in chroma units; 4:2:0
    // SubHeightC=2 with frame_mbs_only=1 gives step_y=2 → 4×2=8 luma.
    let sps = parse_sps(SPS_1080P_HIGH40).expect("parse");
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.coded_width(), 1920);
    assert_eq!(sps.coded_height(), 1088);
    assert_eq!(sps.crop_left, 0);
    assert_eq!(sps.crop_right, 0);
    assert_eq!(sps.crop_top, 0);
    assert_eq!(sps.crop_bottom, 8);
}

#[test]
fn parse_sps_720p_has_no_crop() {
    // 1280×720 = 80×45 macroblocks — no crop needed; SPS likely omits
    // frame_cropping_flag entirely. Confirm all four offsets are zero
    // and coded == post-crop.
    let sps = parse_sps(SPS_720P_MAIN31).expect("parse");
    assert_eq!(sps.crop_left, 0);
    assert_eq!(sps.crop_right, 0);
    assert_eq!(sps.crop_top, 0);
    assert_eq!(sps.crop_bottom, 0);
    assert_eq!(sps.coded_width(), sps.width);
    assert_eq!(sps.coded_height(), sps.height);
}

#[test]
fn parse_sps_returns_err_on_garbage() {
    let bytes = [0xff_u8; 8];
    assert!(parse_sps(&bytes).is_err());
}

#[test]
fn parse_sps_returns_err_on_empty() {
    assert!(parse_sps(&[]).is_err());
}

const PPS_1080P_HIGH40: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h264/h264_1080p_high40_bt709_pps.bin");

#[test]
fn parse_pps_1080p_high_basics() {
    let pps = parse_pps(PPS_1080P_HIGH40).expect("parse PPS");
    assert_eq!(pps.pic_parameter_set_id, 0);
    assert_eq!(pps.seq_parameter_set_id, 0);
}

#[test]
fn parse_pps_preserves_raw_rbsp() {
    let pps = parse_pps(PPS_1080P_HIGH40).expect("parse");
    assert_eq!(pps.raw_rbsp, PPS_1080P_HIGH40);
}

#[test]
fn parse_pps_returns_err_on_empty() {
    assert!(parse_pps(&[]).is_err());
}

#[test]
fn parse_parameter_sets_sps_plus_pps() {
    let nals = vec![
        nal_h264(7, SPS_1080P_HIGH40.to_vec()),
        nal_h264(8, PPS_1080P_HIGH40.to_vec()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
    assert_eq!(ps.pps_by_id.len(), 1);
    assert_eq!(ps.sps_by_id[&0].width, 1920);
    assert_eq!(ps.pps_by_id[&0].seq_parameter_set_id, 0);
}

#[test]
fn parse_parameter_sets_skips_slice_nals_silently() {
    let nals = vec![
        nal_h264(5, vec![0xff; 32]),
        nal_h264(7, SPS_1080P_HIGH40.to_vec()),
        nal_h264(8, PPS_1080P_HIGH40.to_vec()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
}

#[test]
fn parse_parameter_sets_skips_h265_nals_silently() {
    let nals = vec![
        NalUnit::H265 {
            nal_type: 32,
            layer_id: 0,
            temporal_id_plus1: 1,
            payload: vec![0; 8].into(),
        },
        nal_h264(7, SPS_1080P_HIGH40.to_vec()),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert_eq!(ps.sps_by_id.len(), 1);
}

#[test]
fn parse_parameter_sets_empty_input_returns_ok_empty() {
    let ps = parse_parameter_sets(&[]).expect("parse");
    assert!(ps.sps_by_id.is_empty());
    assert!(ps.pps_by_id.is_empty());
}

#[test]
fn parse_parameter_sets_only_slice_nals_returns_ok_empty() {
    let nals = vec![nal_h264(1, vec![0; 16])];
    let ps = parse_parameter_sets(&nals).expect("parse");
    assert!(ps.sps_by_id.is_empty());
    assert!(ps.pps_by_id.is_empty());
}

#[test]
fn parse_parameter_sets_partial_success_one_bad_sps() {
    let nals = vec![
        nal_h264(7, SPS_1080P_HIGH40.to_vec()),
        nal_h264(7, vec![0xff; 8]),
    ];
    let ps = parse_parameter_sets(&nals).expect("parse — good SPS keeps it Ok");
    assert_eq!(ps.sps_by_id.len(), 1);
}

#[test]
fn parse_parameter_sets_all_param_sets_fail_returns_err() {
    // 0xff bytes: SPS fails (SeqParameterSet::from_bits rejects garbage profile/level).
    // 0x00 bytes: PPS fails (all-zero Golomb prefix exhausts the bit buffer mid-read).
    let nals = vec![nal_h264(7, vec![0xff; 8]), nal_h264(8, vec![0x00; 8])];
    assert!(parse_parameter_sets(&nals).is_err());
}

// --- Regression test: invalid chroma_format_idc is rejected ---

/// Hand-crafted minimal High-profile SPS for exercising the
/// `chroma_format_idc` validation path. Returns the raw RBSP body (no
/// NAL header byte — `parse_sps` expects the caller to strip it).
///
/// Per H.264 V15 §7.3.2.1.1:
/// - profile_idc = 100 (High) → chroma_format_idc is signaled.
fn craft_high_sps_with_chroma_format_idc(idc: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    // §7.3.2.1.1 SPS body (NAL header already stripped by caller).
    bw.write(100, 8); // profile_idc = 100 (High)
    bw.write(0, 8); // constraint_set_flags + reserved_zero_2bits = 0
    bw.write(40, 8); // level_idc = 40 (Level 4.0)
    bw.write_ue(0); // seq_parameter_set_id = 0
    // profile_idc == 100 → chroma_format_idc is present.
    bw.write_ue(idc); // chroma_format_idc (CALLER-SUPPLIED — possibly out-of-spec)
    if idc == 3 {
        bw.write(0, 1); // separate_colour_plane_flag (only when idc=3)
    }
    bw.write_ue(0); // bit_depth_luma_minus8 = 0
    bw.write_ue(0); // bit_depth_chroma_minus8 = 0
    bw.write(0, 1); // qpprime_y_zero_transform_bypass_flag = 0
    bw.write(0, 1); // seq_scaling_matrix_present_flag = 0
    bw.write_ue(0); // log2_max_frame_num_minus4 = 0
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    bw.write_ue(1); // num_ref_frames = 1
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag = 0
    bw.write_ue(19); // pic_width_in_mbs_minus1 = 19 (320px / 16)
    bw.write_ue(14); // pic_height_in_map_units_minus1 = 14 (240px / 16)
    bw.write(1, 1); // frame_mbs_only_flag = 1
    bw.write(0, 1); // direct_8x8_inference_flag = 0
    bw.write(0, 1); // frame_cropping_flag = 0
    bw.write(0, 1); // vui_parameters_present_flag = 0
    bw.end_rbsp();
    bw.bytes
}

/// Per H.264 V15 §7.4.2.1.1, `chroma_format_idc` shall be in 0..=3. An
/// out-of-range value must not be coerced to `Yuv420` (the cropping math
/// would then disagree with the stream): it is refused with
/// `CodecParseError::ReservedValue`.
#[test]
fn chroma_format_idc_5_rejected_with_reserved_value() {
    let rbsp = craft_high_sps_with_chroma_format_idc(5);
    match parse_sps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "chroma_format_idc");
            assert_eq!(value, 5);
        }
        other => {
            panic!("expected ReservedValue {{ field: chroma_format_idc, value: 5 }}, got {other:?}")
        }
    }
}

// --- seq_parameter_set_id range + cross-validation regression tests ---

/// Hand-craft a PPS RBSP with the given `pic_parameter_set_id` and
/// `seq_parameter_set_id`, no NAL header. Per H.264 V15 §7.3.2.2 the PPS
/// body starts with two ue(v) fields and an entropy_coding_mode_flag bit,
/// then more fields we don't need to terminate before — the standalone
/// `parse_pps` decoder reads only those first three values, so we stop
/// after the entropy_coding_mode_flag and close with rbsp_trailing_bits
/// (a stop bit and zero padding to a byte boundary).
fn craft_pps(pic_parameter_set_id: u32, seq_parameter_set_id: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write_ue(pic_parameter_set_id);
    bw.write_ue(seq_parameter_set_id);
    bw.write(0, 1); // entropy_coding_mode_flag = 0 (CAVLC)
    bw.end_rbsp();
    bw.bytes
}

/// Per H.264 V15 §7.4.2.2 (PDF p. 109): `seq_parameter_set_id` in a PPS
/// "shall be in the range of 0 to 31, inclusive". 31 is the boundary OK.
#[test]
fn parse_pps_with_sps_id_31_succeeds() {
    let rbsp = craft_pps(0, 31);
    let pps = parse_pps(&rbsp).expect("PPS with sps_id=31 must succeed");
    assert_eq!(pps.pic_parameter_set_id, 0);
    assert_eq!(pps.seq_parameter_set_id, 31);
}

/// Per H.264 V15 §7.4.2.2, 32 is just past the spec bound and must be
/// rejected with a typed error, not silently accepted.
#[test]
fn parse_pps_with_sps_id_32_returns_typed_range_error() {
    let rbsp = craft_pps(0, 32);
    match parse_pps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "seq_parameter_set_id");
            assert_eq!(value, 32);
        }
        other => panic!(
            "expected ReservedValue {{ field: seq_parameter_set_id, value: 32 }}, got {other:?}"
        ),
    }
}

/// 255 was the upper bound of the old buggy [0, 255] range — must now be
/// rejected. Per H.264 V15 §7.4.2.2, `seq_parameter_set_id` ∈ [0, 31].
#[test]
fn parse_pps_with_sps_id_255_returns_typed_range_error() {
    let rbsp = craft_pps(0, 255);
    match parse_pps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "seq_parameter_set_id");
            assert_eq!(value, 255);
        }
        other => panic!(
            "expected ReservedValue {{ field: seq_parameter_set_id, value: 255 }}, got {other:?}"
        ),
    }
}

/// `parse_parameter_sets` cross-validates that each parsed PPS references
/// an SPS that is present in the input. On miss, it emits a warning
/// (matching the partial-success policy at lines 41-43 / 53-55 in
/// decode.rs) and drops the dangling PPS from the output. This test
/// builds a fixture with an SPS at id=0 and a PPS that points at id=7 —
/// the SPS is retained, the PPS is dropped.
#[test]
fn parse_parameter_sets_with_pps_referencing_missing_sps_drops_pps() {
    // sps_id=7 is in spec range (≤ 31) but no SPS with id=7 is in the input.
    let bad_pps = craft_pps(0, 7);
    let nals = vec![nal_h264(7, SPS_1080P_HIGH40.to_vec()), nal_h264(8, bad_pps)];
    let ps = parse_parameter_sets(&nals).expect("parse — SPS keeps it Ok");
    assert_eq!(ps.sps_by_id.len(), 1);
    // Dangling PPS dropped.
    assert!(
        ps.pps_by_id.is_empty(),
        "PPS referencing missing SPS must be dropped"
    );
}

// --- B-frame detection from the constraint flags (profile_idc=100), and
// the frame rate when 2 * num_units_in_tick overflows u32 ---

/// Hand-craft a High-profile (profile_idc=100) SPS RBSP with caller-supplied
/// `constraint_set_flags` byte (MSB-first per H.264 §7.3.2.1.1: bit 7 =
/// constraint_set0_flag, bit 6 = constraint_set1_flag, ..., bit 2 =
/// constraint_set5_flag, bits 1-0 = reserved_zero_2bits = 0). Optionally
/// embeds a VUI with timing_info using the supplied `num_units_in_tick`
/// (time_scale is fixed at 30, fixed_frame_rate_flag = true). The VUI
/// emits NO bitstream_restrictions, so `extract_has_b_frames` falls back
/// to the profile/constraint-flag logic rather than the
/// `max_num_reorder_frames` short-circuit. No NAL header byte.
fn craft_high_sps_with_constraints_and_timing(
    constraint_flags: u8,
    num_units_in_tick: Option<u32>,
) -> Vec<u8> {
    let mut bw = BitWriter::new();
    // §7.3.2.1.1 SPS body (NAL header already stripped by caller).
    bw.write(100, 8); // profile_idc = 100 (High)
    bw.write(constraint_flags.into(), 8); // constraint_set_flags + reserved_zero_2bits (CALLER-SUPPLIED)
    bw.write(40, 8); // level_idc = 40 (Level 4.0)
    bw.write_ue(0); // seq_parameter_set_id = 0
    // profile_idc == 100 → chroma_format_idc is present.
    bw.write_ue(1); // chroma_format_idc = 1 (YUV 4:2:0)
    bw.write_ue(0); // bit_depth_luma_minus8 = 0
    bw.write_ue(0); // bit_depth_chroma_minus8 = 0
    bw.write(0, 1); // qpprime_y_zero_transform_bypass_flag = 0
    bw.write(0, 1); // seq_scaling_matrix_present_flag = 0
    bw.write_ue(0); // log2_max_frame_num_minus4 = 0
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    bw.write_ue(1); // num_ref_frames = 1
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag = 0
    bw.write_ue(19); // pic_width_in_mbs_minus1 = 19 (320px / 16)
    bw.write_ue(14); // pic_height_in_map_units_minus1 = 14 (240px / 16)
    bw.write(1, 1); // frame_mbs_only_flag = 1
    bw.write(0, 1); // direct_8x8_inference_flag = 0
    bw.write(0, 1); // frame_cropping_flag = 0
    // vui_parameters_present_flag — 1 iff caller asked for timing_info.
    if let Some(units) = num_units_in_tick {
        bw.write(1, 1); // vui_parameters_present_flag = 1
        bw.write(0, 1); // aspect_ratio_info_present_flag = 0
        bw.write(0, 1); // overscan_info_present_flag = 0
        bw.write(0, 1); // video_signal_type_present_flag = 0
        bw.write(0, 1); // chroma_loc_info_present_flag = 0
        bw.write(1, 1); // timing_info_present_flag = 1
        bw.write(units.into(), 32); // num_units_in_tick (CALLER-SUPPLIED)
        // time_scale: any non-zero value; the tests that use this builder
        // assert on the units' edge cases only.
        bw.write(0x4000_001E, 32); // time_scale
        bw.write(1, 1); // fixed_frame_rate_flag = 1
        bw.write(0, 1); // nal_hrd_parameters_present_flag = 0
        bw.write(0, 1); // vcl_hrd_parameters_present_flag = 0
        bw.write(0, 1); // pic_struct_present_flag = 0
        bw.write(0, 1); // bitstream_restriction_flag = 0 (no max_num_reorder_frames)
    } else {
        bw.write(0, 1); // vui_parameters_present_flag = 0
    }
    bw.end_rbsp();
    bw.bytes
}

/// High (profile_idc=100) with constraint_set1_flag=1 (bit 6, mask
/// 0b0100_0000) is read as B-frameless.
#[test]
fn extract_has_b_frames_false_for_constrained_high() {
    let rbsp = craft_high_sps_with_constraints_and_timing(0b0100_0000, None);
    let sps = parse_sps(&rbsp).expect("parse constrained-high SPS");
    assert_eq!(sps.profile_idc, 100);
    assert!(
        !sps.has_b_frames,
        "Constrained High (constraint_set1_flag=1) excludes B-frames per H.264 §A.2"
    );
}

/// Constrained High (profile_idc=100 + constraint_set4_flag=1 +
/// constraint_set5_flag=1, H.264 §A.2.4.2) excludes B-frames.
/// constraint_set4/5_flag are bits 3/2 (masks 0b0000_1000 / 0b0000_0100).
#[test]
fn extract_has_b_frames_false_for_constrained_baseline_lifted_high() {
    // constraint_set4_flag=1 | constraint_set5_flag=1 = 0b0000_1100
    let rbsp = craft_high_sps_with_constraints_and_timing(0b0000_1100, None);
    let sps = parse_sps(&rbsp).expect("parse constrained-baseline-lifted-high SPS");
    assert_eq!(sps.profile_idc, 100);
    assert!(
        !sps.has_b_frames,
        "constraint_set4+5_flags excludes B-frames per H.264 §A.2"
    );
}

/// Regression guard — unconstrained High (profile_idc=100 with
/// all constraint flags clear) MUST still report `has_b_frames=true`.
/// Catches over-broad narrowing of the constraint-flag check.
#[test]
fn extract_has_b_frames_true_for_unconstrained_high() {
    let rbsp = craft_high_sps_with_constraints_and_timing(0, None);
    let sps = parse_sps(&rbsp).expect("parse unconstrained-high SPS");
    assert_eq!(sps.profile_idc, 100);
    assert_eq!(sps.constraint_set_flags, 0);
    assert!(
        sps.has_b_frames,
        "unconstrained High must report has_b_frames=true"
    );
}

/// `CodecParseError` rustdoc promises non-panicking parse. A
/// stream with a num_units_in_tick value such that `2 * num_units_in_tick`
/// overflows u32 previously panicked in debug builds. The
/// `saturating_mul(2)` fix should treat the result as unknowable and
/// surface `frame_rate: None` rather than emit a nonsense ratio.
///
/// Test value note: `0xFFFF_FFFE` (u32::MAX - 1) and u32::MAX saturate
/// `* 2` identically, so either exercises the None-on-saturation behavior.
#[test]
fn num_units_in_tick_overflow_no_panic() {
    let rbsp = craft_high_sps_with_constraints_and_timing(0, Some(0xFFFF_FFFE));
    let sps = parse_sps(&rbsp).expect("parse SPS with num_units_in_tick = u32::MAX - 1");
    assert!(
        sps.frame_rate.is_none(),
        "frame_rate must be None when 2 * num_units_in_tick saturates u32"
    );
}

/// Craft a Baseline (profile_idc=66) SPS with VUI chroma_loc_info set to
/// `top_loc` for exercising the chroma_sample_loc_type_* range check.
fn craft_sps_with_chroma_loc(top_loc_ue: u32) -> Vec<u8> {
    use crate::codec::test_util::BitWriter;
    let mut bw = BitWriter::new();
    bw.write(66, 8); // profile_idc = 66 (Baseline — no chroma/depth block)
    bw.write(0, 8); // constraint_set_flags
    bw.write(30, 8); // level_idc
    bw.write_ue(0); // seq_parameter_set_id = 0
    bw.write_ue(0); // log2_max_frame_num_minus4 = 0
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    bw.write_ue(1); // max_num_ref_frames = 1
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag = 0
    bw.write_ue(9); // pic_width_in_mbs_minus1 = 9 (160px / 16)
    bw.write_ue(7); // pic_height_in_map_units_minus1 = 7 (128px / 16)
    bw.write(1, 1); // frame_mbs_only_flag = 1
    bw.write(0, 1); // direct_8x8_inference_flag = 0
    bw.write(0, 1); // frame_cropping_flag = 0
    // VUI: vui_parameters_present_flag = 1
    bw.write(1, 1);
    bw.write(0, 1); // aspect_ratio_info_present_flag = 0
    bw.write(0, 1); // overscan_info_present_flag = 0
    bw.write(0, 1); // video_signal_type_present_flag = 0
    // chroma_loc_info_present_flag = 1
    bw.write(1, 1);
    bw.write_ue(top_loc_ue); // chroma_sample_loc_type_top_field (CALLER-SUPPLIED)
    bw.write_ue(0); // chroma_sample_loc_type_bottom_field = 0
    bw.write(0, 1); // timing_info_present_flag = 0
    bw.write(0, 1); // nal_hrd_parameters_present_flag = 0
    bw.write(0, 1); // vcl_hrd_parameters_present_flag = 0
    bw.write(0, 1); // pic_struct_present_flag = 0
    bw.write(0, 1); // bitstream_restriction_flag = 0
    bw.end_rbsp();
    bw.bytes
}

/// `num_units_in_tick == 0` must not produce a zero denominator
/// in the frame_rate ratio. H.265 VUI already guards this (`if num_units_in_tick > 0`);
/// H.264 VUI had a gap where `0 * 2 = 0 != u32::MAX` flowed to
/// `Some(Rational { den: 0 })`, a ÷0 hazard for consumers. Post-fix, the
/// result must be `None`.
#[test]
fn num_units_in_tick_zero_yields_none_not_den0() {
    let rbsp = craft_high_sps_with_constraints_and_timing(0, Some(0));
    let sps = parse_sps(&rbsp).expect("parse SPS with num_units_in_tick = 0");
    assert!(
        sps.frame_rate.is_none(),
        "frame_rate must be None when num_units_in_tick == 0 (den=0 is a ÷0 hazard)"
    );
}

/// chroma_sample_loc_type_top_field ∈ [0,5] per H.264 §E.2.1.
/// Value 5 (at the boundary) must be accepted.
#[test]
fn chroma_sample_loc_type_5_accepted() {
    let rbsp = craft_sps_with_chroma_loc(5);
    let sps = parse_sps(&rbsp).expect("chroma_loc=5 is in-spec and must parse");
    let color = sps.color.expect("VUI color present");
    assert_eq!(color.chroma_loc, Some(5));
}

/// Value 6 is out of the H.264 §E.2.1 [0,5] range and must
/// be rejected as ReservedValue rather than silently passing.
#[test]
fn chroma_sample_loc_type_6_rejected() {
    let rbsp = craft_sps_with_chroma_loc(6);
    match parse_sps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert!(
                field.contains("chroma_sample_loc_type"),
                "unexpected field: {field}"
            );
            assert_eq!(value, 6);
        }
        other => panic!("expected ReservedValue, got {other:?}"),
    }
}

/// Adversarial value 256 truncates to 0 via `as u8` without the guard —
/// a valid value that hides the malformed input. It must be rejected
/// before any narrowing cast.
#[test]
fn chroma_sample_loc_type_256_rejected_not_silently_truncated() {
    let rbsp = craft_sps_with_chroma_loc(256);
    match parse_sps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert!(
                field.contains("chroma_sample_loc_type"),
                "unexpected field: {field}"
            );
            assert_eq!(value, 256);
        }
        other => panic!("expected ReservedValue(256), got {other:?}"),
    }
}

/// Craft a Baseline SPS with configurable `log2_max_frame_num_minus4` and
/// `log2_max_pic_order_cnt_lsb_minus4` (pic_order_cnt_type = 0), no VUI.
fn craft_sps_with_log2_fields(frame_num_minus4: u32, poc_lsb_minus4: u32) -> Vec<u8> {
    use crate::codec::test_util::BitWriter;
    let mut bw = BitWriter::new();
    bw.write(66, 8); // profile_idc = 66 (Baseline)
    bw.write(0, 8); // constraint_set_flags
    bw.write(30, 8); // level_idc
    bw.write_ue(0); // seq_parameter_set_id = 0
    bw.write_ue(frame_num_minus4); // log2_max_frame_num_minus4
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(poc_lsb_minus4); // log2_max_pic_order_cnt_lsb_minus4
    bw.write_ue(1); // max_num_ref_frames = 1
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(9); // pic_width_in_mbs_minus1
    bw.write_ue(7); // pic_height_in_map_units_minus1
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(0, 1); // direct_8x8_inference_flag
    bw.write(0, 1); // frame_cropping_flag
    bw.write(0, 1); // vui_parameters_present_flag
    bw.end_rbsp();
    bw.bytes
}

/// H.264 §7.4.2.1.1 specifies log2_max_frame_num_minus4 ∈ [0, 12].
/// Value 12 (the spec maximum) must be accepted.
#[test]
fn log2_max_frame_num_minus4_12_accepted() {
    let rbsp = craft_sps_with_log2_fields(12, 0);
    let _sps = parse_sps(&rbsp).expect("log2_max_frame_num_minus4=12 is in-spec");
}

/// Value 13 is beyond the [0,12] spec range and must be rejected; a plain
/// u8::try_from accepts 13 (it fits in u8), silently enabling malformed
/// streams.
#[test]
fn log2_max_frame_num_minus4_13_rejected() {
    let rbsp = craft_sps_with_log2_fields(13, 0);
    match parse_sps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "log2_max_frame_num_minus4");
            assert_eq!(value, 13);
        }
        other => panic!("expected ReservedValue(13), got {other:?}"),
    }
}

/// H.264 §7.4.2.1.1 specifies log2_max_pic_order_cnt_lsb_minus4 ∈ [0, 12].
/// Value 12 must be accepted (pic_order_cnt_type = 0 path).
#[test]
fn log2_max_pic_order_cnt_lsb_minus4_12_accepted() {
    let rbsp = craft_sps_with_log2_fields(0, 12);
    let _sps = parse_sps(&rbsp).expect("log2_max_pic_order_cnt_lsb_minus4=12 is in-spec");
}

/// Value 13 is beyond the [0,12] spec range and must be rejected, not
/// silently accepted.
#[test]
fn log2_max_pic_order_cnt_lsb_minus4_13_rejected() {
    let rbsp = craft_sps_with_log2_fields(0, 13);
    match parse_sps(&rbsp) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "log2_max_pic_order_cnt_lsb_minus4");
            assert_eq!(value, 13);
        }
        other => panic!("expected ReservedValue(13), got {other:?}"),
    }
}

/// A non-conformant scaling-list `delta_scale` (outside the
/// H.264 §7.4.2.1.1.1 [-128,127] range) must not panic. Before the fix, the
/// `last_scale + delta_scale + 256` i32 add in `skip_scaling_list` overflowed
/// on this crafted ~14-byte High-profile SPS (panic in overflow-checked builds,
/// silent wrap + cursor desync in release).
#[test]
fn parse_sps_scaling_list_large_delta_does_not_panic() {
    let rbsp = [
        0x64, 0x00, 0x28, 0xad, 0x80, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x00,
    ];
    // Must return a Result (Ok or Err) — never panic.
    let _ = parse_sps(&rbsp);
}

/// Hand-built Baseline SPS, 1920×1088, 4:2:0 (the Baseline profile has no
/// chroma_format_idc block), `frame_cropping_flag = 1` with caller-supplied
/// offsets in chroma units (×2 luma for 4:2:0 frame pictures). Mirrors the
/// H.265 / H.266 fixtures; emulation-escaped like a real NAL.
fn build_h264_sps_with_frame_crop(left: u32, right: u32, top: u32, bottom: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(66, 8); // profile_idc = Baseline
    bw.write(0, 8); // constraint_set flags + reserved_zero_2bits
    bw.write(30, 8); // level_idc 3.0
    bw.write_ue(0); // seq_parameter_set_id
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(2); // pic_order_cnt_type = 2 (no POC fields follow)
    bw.write_ue(1); // max_num_ref_frames
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(119); // pic_width_in_mbs_minus1 → 1920
    bw.write_ue(67); // pic_height_in_map_units_minus1 → 1088
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(1, 1); // direct_8x8_inference_flag
    bw.write(1, 1); // frame_cropping_flag
    bw.write_ue(left);
    bw.write_ue(right);
    bw.write_ue(top);
    bw.write_ue(bottom);
    bw.write(0, 1); // vui_parameters_present_flag
    bw.end_rbsp();
    escape_emulation(&bw.bytes)
}

/// H.264 sibling of the H.265 getter-overflow case.
#[test]
fn hostile_frame_crop_is_rejected_at_parse_time() {
    let sps = parse_sps(&build_h264_sps_with_frame_crop(0, 0, 0, 4)).unwrap();
    assert_eq!((sps.width, sps.height), (1920, 1080));
    assert_eq!((sps.coded_width(), sps.coded_height()), (1920, 1088));
    // Just under the raw width is still a (2-sample-wide) picture: accepted.
    let sps = parse_sps(&build_h264_sps_with_frame_crop(959, 0, 0, 0)).unwrap();
    assert_eq!(sps.width, 2);
    assert_eq!(sps.coded_width(), 1920);
    for (l, r, t, b) in [
        (1u32 << 30, 1u32 << 30, 0u32, 0u32),
        (1 << 31, 0, 0, 0), // multiply-saturation path: 2 * 2^31 -> u32::MAX
        (0, 0, 1 << 30, 1 << 30),
        (960, 0, 0, 0),
        (0, 0, 272, 272),
    ] {
        match parse_sps(&build_h264_sps_with_frame_crop(l, r, t, b)) {
            Err(CodecParseError::ReservedValue { field, .. }) => {
                assert!(field.starts_with("frame_crop_"), "{field}")
            }
            other => panic!("({l},{r},{t},{b}) must be rejected, got {other:?}"),
        }
    }
}

/// Baseline SPS with `pic_order_cnt_type` 1 and `cycle` reference-frame offsets.
fn craft_baseline_sps_poc_type1(cycle: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(66, 8); // profile_idc = Baseline
    bw.write(0, 8); // constraint_set flags + reserved_zero_2bits
    bw.write(40, 8); // level_idc 4.0
    bw.write_ue(0); // seq_parameter_set_id
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(1); // pic_order_cnt_type = 1
    bw.write(0, 1); // delta_pic_order_always_zero_flag
    bw.write_ue(0); // offset_for_non_ref_pic = se(0)
    bw.write_ue(0); // offset_for_top_to_bottom_field = se(0)
    bw.write_ue(cycle); // num_ref_frames_in_pic_order_cnt_cycle
    for _ in 0..cycle {
        bw.write_ue(0); // offset_for_ref_frame = se(0)
    }
    bw.write_ue(1); // max_num_ref_frames
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(19); // pic_width_in_mbs_minus1 → 320
    bw.write_ue(14); // pic_height_in_map_units_minus1 → 240
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(0, 1); // direct_8x8_inference_flag
    bw.write(0, 1); // frame_cropping_flag
    bw.write(0, 1); // vui_parameters_present_flag
    bw.end_rbsp();
    escape_emulation(&bw.bytes)
}

/// H.264 §7.4.2.1.1: num_ref_frames_in_pic_order_cnt_cycle shall be in 0..=255.
#[test]
fn poc_cycle_count_above_255_is_rejected() {
    let sps = parse_sps(&craft_baseline_sps_poc_type1(255)).expect("255 is in range");
    assert_eq!((sps.width, sps.height), (320, 240));
    match parse_sps(&craft_baseline_sps_poc_type1(256)) {
        Err(CodecParseError::ReservedValue { field, value }) => {
            assert_eq!(field, "num_ref_frames_in_pic_order_cnt_cycle");
            assert_eq!(value, 256);
        }
        other => panic!("256 must be refused, got {other:?}"),
    }
}

/// High-profile SPS with a caller-chosen chroma format, separate-colour-plane
/// flag and bit depths; `log2_max_frame_num_minus4 = 0`, so `frame_num` is 4 bits.
fn craft_high_sps(
    chroma_format_idc: u32,
    separate_colour_plane: bool,
    luma_minus8: u32,
    chroma_minus8: u32,
) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(100, 8); // profile_idc = High
    bw.write(0, 8); // constraint_set flags + reserved_zero_2bits
    bw.write(40, 8); // level_idc 4.0
    bw.write_ue(0); // seq_parameter_set_id
    bw.write_ue(chroma_format_idc);
    if chroma_format_idc == 3 {
        bw.write(u64::from(separate_colour_plane), 1); // separate_colour_plane_flag
    }
    bw.write_ue(luma_minus8); // bit_depth_luma_minus8
    bw.write_ue(chroma_minus8); // bit_depth_chroma_minus8
    bw.write(0, 1); // qpprime_y_zero_transform_bypass_flag
    bw.write(0, 1); // seq_scaling_matrix_present_flag
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    bw.write_ue(1); // max_num_ref_frames
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(19); // pic_width_in_mbs_minus1 → 320
    bw.write_ue(14); // pic_height_in_map_units_minus1 → 240
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(0, 1); // direct_8x8_inference_flag
    bw.write(0, 1); // frame_cropping_flag
    bw.write(0, 1); // vui_parameters_present_flag
    bw.end_rbsp();
    escape_emulation(&bw.bytes)
}

/// An SPS of any profile with a caller-chosen constraint byte and no VUI, so
/// `has_b_frames` comes from the profile and the constraint flags alone.
fn craft_sps_with_constraints(profile_idc: u8, constraint_flags: u8) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(profile_idc.into(), 8);
    bw.write(constraint_flags.into(), 8);
    bw.write(40, 8); // level_idc 4.0
    bw.write_ue(0); // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        bw.write_ue(1); // chroma_format_idc = 4:2:0
        bw.write_ue(0); // bit_depth_luma_minus8
        bw.write_ue(0); // bit_depth_chroma_minus8
        bw.write(0, 1); // qpprime_y_zero_transform_bypass_flag
        bw.write(0, 1); // seq_scaling_matrix_present_flag
    }
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    bw.write_ue(1); // max_num_ref_frames
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(19); // pic_width_in_mbs_minus1
    bw.write_ue(14); // pic_height_in_map_units_minus1
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(0, 1); // direct_8x8_inference_flag
    bw.write(0, 1); // frame_cropping_flag
    bw.write(0, 1); // vui_parameters_present_flag
    bw.end_rbsp();
    escape_emulation(&bw.bytes)
}

/// Baseline SPS whose VUI carries only the timing fields.
fn craft_sps_with_timing(num_units_in_tick: u32, time_scale: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(66, 8); // profile_idc = Baseline
    bw.write(0, 8); // constraint_set flags + reserved_zero_2bits
    bw.write(40, 8); // level_idc 4.0
    bw.write_ue(0); // seq_parameter_set_id
    bw.write_ue(0); // log2_max_frame_num_minus4
    bw.write_ue(0); // pic_order_cnt_type = 0
    bw.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    bw.write_ue(1); // max_num_ref_frames
    bw.write(0, 1); // gaps_in_frame_num_value_allowed_flag
    bw.write_ue(19); // pic_width_in_mbs_minus1
    bw.write_ue(14); // pic_height_in_map_units_minus1
    bw.write(1, 1); // frame_mbs_only_flag
    bw.write(0, 1); // direct_8x8_inference_flag
    bw.write(0, 1); // frame_cropping_flag
    bw.write(1, 1); // vui_parameters_present_flag
    bw.write(0, 4); // aspect_ratio, overscan, video_signal_type, chroma_loc: absent
    bw.write(1, 1); // timing_info_present_flag
    bw.write(num_units_in_tick.into(), 32);
    bw.write(time_scale.into(), 32);
    bw.write(1, 1); // fixed_frame_rate_flag
    bw.write(0, 4); // nal_hrd, vcl_hrd, pic_struct, bitstream_restriction: absent
    bw.end_rbsp();
    escape_emulation(&bw.bytes)
}

/// H.264 §7.3.3: with `separate_colour_plane_flag` the slice header carries
/// `colour_plane_id u(2)` before `frame_num`. The slice bytes `EC C0` read
/// `first_mb_in_slice = 0`, `slice_type = 0`, `pic_parameter_set_id = 0`,
/// then `01` and `1001`: `frame_num` 9 behind a colour plane id, or 6 when
/// the next four bits are `frame_num` directly.
#[test]
fn light_slice_header_skips_colour_plane_id() {
    for (chroma_format_idc, what) in [(1, "4:2:0"), (3, "4:4:4 without separate planes")] {
        let sps = parse_sps(&craft_high_sps(chroma_format_idc, false, 0, 0)).unwrap();
        assert!(!sps.separate_colour_plane, "{what}");
        let sh = parse_slice_header_light(&[0xEC, 0xC0], Some(&sps), 1).unwrap();
        assert_eq!(sh.frame_num, Some(6), "{what}");
    }
    let sps = parse_sps(&craft_high_sps(3, true, 0, 0)).unwrap();
    assert!(sps.separate_colour_plane);
    let sh = parse_slice_header_light(&[0xEC, 0xC0], Some(&sps), 1).unwrap();
    assert_eq!(sh.frame_num, Some(9));
}

/// H.264 §7.4.2.1.1: `bit_depth_luma_minus8` and `bit_depth_chroma_minus8`
/// shall be in 0..=6.
#[test]
fn avc_bit_depth_minus8_bound_is_six() {
    let sps = parse_sps(&craft_high_sps(1, false, 6, 6)).unwrap();
    assert_eq!((sps.bit_depth_luma, sps.bit_depth_chroma), (14, 14));
    for (luma, chroma, field, value) in [
        (7, 0, "bit_depth_luma_minus8", 7),
        (8, 0, "bit_depth_luma_minus8", 8),
        (0, 7, "bit_depth_chroma_minus8", 7),
        (0, 8, "bit_depth_chroma_minus8", 8),
    ] {
        match parse_sps(&craft_high_sps(1, false, luma, chroma)) {
            Err(CodecParseError::ReservedValue { field: f, value: v }) => {
                assert_eq!((f, v), (field, value))
            }
            other => panic!("({luma}, {chroma}) must be refused, got {other:?}"),
        }
    }
}

/// H.264 §7.4.2.1.1: on profiles 77, 88 and 100, `constraint_set5_flag = 1`
/// says no B slices are present.
#[test]
fn extract_has_b_frames_false_for_set5_alone_on_77_88_100() {
    for profile in [77u8, 88, 100] {
        let sps = parse_sps(&craft_sps_with_constraints(profile, 0b0000_0100)).unwrap();
        assert!(!sps.has_b_frames, "profile {profile}");
    }
}

/// The flag has other meanings on other profiles, and without it these
/// profiles may carry B slices.
#[test]
fn extract_has_b_frames_true_without_set5_or_on_other_profiles() {
    for profile in [77u8, 88, 100] {
        let sps = parse_sps(&craft_sps_with_constraints(profile, 0)).unwrap();
        assert!(sps.has_b_frames, "profile {profile}");
    }
    let sps = parse_sps(&craft_sps_with_constraints(110, 0b0000_0100)).unwrap();
    assert!(sps.has_b_frames, "profile 110");
}

/// H.264 §E.2.1: `time_scale` shall be greater than 0; a zero clock gives no
/// rate.
#[test]
fn a_zero_time_scale_gives_no_frame_rate() {
    assert_eq!(
        parse_sps(&craft_sps_with_timing(1, 60)).unwrap().frame_rate,
        Some(Rational { num: 60, den: 2 })
    );
    assert_eq!(
        parse_sps(&craft_sps_with_timing(1, 0)).unwrap().frame_rate,
        None
    );
}

/// `nal_unit_type` is a 5-bit field: a value above 31 is refused before any
/// bit of the slice header is read.
#[test]
fn a_nal_unit_type_over_31_is_refused() {
    let rbsp = [0x88, 0x80]; // first_mb_in_slice 0, slice_type 7 (I), pps 0
    assert!(parse_slice_header_light(&rbsp, None, 31).is_ok());
    for bytes in [&rbsp[..], &[]] {
        match parse_slice_header_light(bytes, None, 32) {
            Err(CodecParseError::ReservedValue { field, value }) => {
                assert_eq!((field, value), ("nal_unit_type", 32))
            }
            other => panic!("32 must be refused, got {other:?}"),
        }
    }
}
