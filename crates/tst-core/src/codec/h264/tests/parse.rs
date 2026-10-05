//! H.264 parameter-set parser tests.

use crate::codec::h264::{parse_parameter_sets, parse_pps, parse_sps};
use crate::codec::test_util::{BitWriter, escape_emulation};
use crate::codec::{
    ChromaFormat, CodecParseError, ColourPrimaries, MatrixCoefficients, TransferCharacteristics,
};
use crate::mpegts::demux::event::NalUnit;

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

// --- H264-01 regression test (chroma_format_idc Invalid arm) ---

/// Hand-crafted minimal High-profile SPS for exercising the
/// `chroma_format_idc` validation path. Returns the raw RBSP body (no
/// NAL header byte — `parse_sps` expects the caller to strip it).
///
/// Per H.264 V15 §7.3.2.1.1:
/// - profile_idc = 100 (High) → chroma_format_idc is signaled.
fn craft_high_sps_with_chroma_format_idc(idc: u32) -> Vec<u8> {
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
        fn trailing(&mut self) {
            self.u(1, 1);
            while self.pos % 8 != 0 {
                self.u(0, 1);
            }
        }
    }
    let mut bw = Bw::new();
    // §7.3.2.1.1 SPS body (NAL header already stripped by caller).
    bw.u(100, 8); // profile_idc = 100 (High)
    bw.u(0, 8); // constraint_set_flags + reserved_zero_2bits = 0
    bw.u(40, 8); // level_idc = 40 (Level 4.0)
    bw.ue(0); // seq_parameter_set_id = 0
    // profile_idc == 100 → chroma_format_idc is present.
    bw.ue(idc); // chroma_format_idc (CALLER-SUPPLIED — possibly out-of-spec)
    if idc == 3 {
        bw.u(0, 1); // separate_colour_plane_flag (only when idc=3)
    }
    bw.ue(0); // bit_depth_luma_minus8 = 0
    bw.ue(0); // bit_depth_chroma_minus8 = 0
    bw.u(0, 1); // qpprime_y_zero_transform_bypass_flag = 0
    bw.u(0, 1); // seq_scaling_matrix_present_flag = 0
    bw.ue(0); // log2_max_frame_num_minus4 = 0
    bw.ue(0); // pic_order_cnt_type = 0
    bw.ue(0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    bw.ue(1); // num_ref_frames = 1
    bw.u(0, 1); // gaps_in_frame_num_value_allowed_flag = 0
    bw.ue(19); // pic_width_in_mbs_minus1 = 19 (320px / 16)
    bw.ue(14); // pic_height_in_map_units_minus1 = 14 (240px / 16)
    bw.u(1, 1); // frame_mbs_only_flag = 1
    bw.u(0, 1); // direct_8x8_inference_flag = 0
    bw.u(0, 1); // frame_cropping_flag = 0
    bw.u(0, 1); // vui_parameters_present_flag = 0
    bw.trailing();
    bw.bytes
}

/// Per H.264 V15 §7.4.2.1.1, `chroma_format_idc` shall be in 0..=3.
/// `h264-reader 0.8` surfaces out-of-range values as
/// `ChromaFormat::Invalid(u32)`. The current `convert_sps` arm silently
/// coerces to `Yuv420`, producing a `H264Sps` with cropping math that
/// disagrees with the (also wrong) chroma_format. This test pins the
/// spec-correct behavior: reject with `CodecParseError::ReservedValue`.
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
/// `parse_pps` decoder reads only those first three values, so we can stop
/// after the entropy_coding_mode_flag (no rbsp_trailing_bits needed; the
/// parser tolerates a payload that ends mid-byte).
fn craft_pps(pic_parameter_set_id: u32, seq_parameter_set_id: u32) -> Vec<u8> {
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
        fn trailing(&mut self) {
            self.u(1, 1);
            while self.pos % 8 != 0 {
                self.u(0, 1);
            }
        }
    }
    let mut bw = Bw::new();
    bw.ue(pic_parameter_set_id);
    bw.ue(seq_parameter_set_id);
    bw.u(0, 1); // entropy_coding_mode_flag = 0 (CAVLC)
    bw.trailing();
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

// --- H264-RV4 + H264-RV7 regression tests ---
// RV4: constraint flags consulted in B-frame detection (profile_idc=100).
// RV7: frame_rate overflow when 2 * num_units_in_tick overflows u32.

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
        fn trailing(&mut self) {
            self.u(1, 1);
            while self.pos % 8 != 0 {
                self.u(0, 1);
            }
        }
    }
    let mut bw = Bw::new();
    // §7.3.2.1.1 SPS body (NAL header already stripped by caller).
    bw.u(100, 8); // profile_idc = 100 (High)
    bw.u(constraint_flags as u32, 8); // constraint_set_flags + reserved_zero_2bits (CALLER-SUPPLIED)
    bw.u(40, 8); // level_idc = 40 (Level 4.0)
    bw.ue(0); // seq_parameter_set_id = 0
    // profile_idc == 100 → chroma_format_idc is present.
    bw.ue(1); // chroma_format_idc = 1 (YUV 4:2:0)
    bw.ue(0); // bit_depth_luma_minus8 = 0
    bw.ue(0); // bit_depth_chroma_minus8 = 0
    bw.u(0, 1); // qpprime_y_zero_transform_bypass_flag = 0
    bw.u(0, 1); // seq_scaling_matrix_present_flag = 0
    bw.ue(0); // log2_max_frame_num_minus4 = 0
    bw.ue(0); // pic_order_cnt_type = 0
    bw.ue(0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    bw.ue(1); // num_ref_frames = 1
    bw.u(0, 1); // gaps_in_frame_num_value_allowed_flag = 0
    bw.ue(19); // pic_width_in_mbs_minus1 = 19 (320px / 16)
    bw.ue(14); // pic_height_in_map_units_minus1 = 14 (240px / 16)
    bw.u(1, 1); // frame_mbs_only_flag = 1
    bw.u(0, 1); // direct_8x8_inference_flag = 0
    bw.u(0, 1); // frame_cropping_flag = 0
    // vui_parameters_present_flag — 1 iff caller asked for timing_info.
    if let Some(units) = num_units_in_tick {
        bw.u(1, 1); // vui_parameters_present_flag = 1
        bw.u(0, 1); // aspect_ratio_info_present_flag = 0
        bw.u(0, 1); // overscan_info_present_flag = 0
        bw.u(0, 1); // video_signal_type_present_flag = 0
        bw.u(0, 1); // chroma_loc_info_present_flag = 0
        bw.u(1, 1); // timing_info_present_flag = 1
        bw.u(units, 32); // num_units_in_tick (CALLER-SUPPLIED)
        // time_scale chosen to avoid 24 leading-zero bits (which can
        // produce 0x00 0x00 0x00 in the byte-stream that
        // ByteReader::without_skip rejects per RBSP emulation-prevention
        // semantics). 0x4000_001E retains frame-rate semantics for any
        // test that asserts on the value; for overflow tests the value
        // is irrelevant.
        bw.u(0x4000_001E, 32); // time_scale
        bw.u(1, 1); // fixed_frame_rate_flag = 1
        bw.u(0, 1); // nal_hrd_parameters_present_flag = 0
        bw.u(0, 1); // vcl_hrd_parameters_present_flag = 0
        bw.u(0, 1); // pic_struct_present_flag = 0
        bw.u(0, 1); // bitstream_restriction_flag = 0 (no max_num_reorder_frames)
    } else {
        bw.u(0, 1); // vui_parameters_present_flag = 0
    }
    bw.trailing();
    bw.bytes
}

/// H264-RV4: Constrained High (profile_idc=100 + constraint_set1_flag=1)
/// per H.264 §A.2 excludes B-frames. h264-reader's `ConstraintFlags::flag1()`
/// corresponds to constraint_set1_flag (bit 6, mask 0b0100_0000).
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

/// H264-RV4: Constrained-Baseline-lifted-to-High (profile_idc=100 +
/// constraint_set4_flag=1 + constraint_set5_flag=1) per H.264 §A.2
/// excludes B-frames. h264-reader's `flag4()`/`flag5()` correspond to
/// constraint_set4/5_flag (bits 3/2, masks 0b0000_1000 / 0b0000_0100).
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

/// H264-RV4: Regression guard — unconstrained High (profile_idc=100 with
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

/// H264-RV7: `CodecParseError` rustdoc promises non-panicking parse. A
/// stream with a num_units_in_tick value such that `2 * num_units_in_tick`
/// overflows u32 previously panicked in debug builds. The
/// `saturating_mul(2)` fix should treat the result as unknowable and
/// surface `frame_rate: None` rather than emit a nonsense ratio.
///
/// Test value note: We use `0xFFFF_FFFE` (u32::MAX - 1) instead of u32::MAX
/// directly so the encoded 32-bit `num_units_in_tick` field carries no
/// three-consecutive-zero-byte sequence — `ByteReader::without_skip` still
/// validates RBSP emulation-prevention semantics, and a hand-crafted
/// fixture with raw 0x00 0x00 0x00 anywhere in the payload would fail to
/// parse for unrelated reasons. Both values saturate `* 2` identically, so
/// the fix's None-on-saturation behavior is exercised either way.
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
