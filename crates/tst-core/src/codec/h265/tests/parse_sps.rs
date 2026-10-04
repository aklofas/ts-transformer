//! SPS parser tests.

use crate::codec::h265::parse_sps;
use crate::codec::{
    ChromaFormat, CodecParseError, ColourPrimaries, MatrixCoefficients, TransferCharacteristics,
};

const SPS_1080P_MAIN40: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");
const SPS_1080P_MAIN10_50: &[u8] =
    include_bytes!("../../../../tests/fixtures/codec/h265/h265_1080p_main10_50_pq_sps.bin");

#[test]
fn parse_sps_1080p_main40_dimensions() {
    let sps = parse_sps(SPS_1080P_MAIN40).expect("parse SPS");
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.bit_depth_luma, 8);
    assert_eq!(sps.bit_depth_chroma, 8);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
    assert_eq!(sps.sps_seq_parameter_set_id, 0);
    assert_eq!(sps.sps_video_parameter_set_id, 0);
    assert_eq!(sps.general_level_idc, 120);
}

#[test]
fn parse_sps_1080p_main10_50_pq_color() {
    let sps = parse_sps(SPS_1080P_MAIN10_50).expect("parse SPS");
    assert_eq!(sps.bit_depth_luma, 10);
    assert_eq!(sps.bit_depth_chroma, 10);
    assert_eq!(sps.general_level_idc, 150);
    let color = sps.color.expect("VUI present");
    assert_eq!(color.primaries, ColourPrimaries::Bt2020);
    assert_eq!(color.transfer, TransferCharacteristics::SmpteSt2084);
    assert_eq!(color.matrix, MatrixCoefficients::Bt2020NonConstant);
}

#[test]
fn parse_sps_surfaces_profile_compatibility_flags() {
    // Real x265-emitted Main10 fixture: x265 sets
    // `general_profile_idc = 2` (Main10) AND `profile_compatibility_flags`
    // with bit 2 set so consumers can detect Main10 compatibility.
    // ffmpeg's `hevc/ps.c:267-270` keys off this bit pattern to
    // disambiguate Main vs Main10 vs Main10-Intra.
    //
    // Bit positions are MSB-first per H.265 §7.3.3: spec-bit `j` lives
    // at `flags & (1 << (31 - j))`. Bit j=2 (Main10) → `1 << 29` =
    // `0x20000000`. This is what x265 emits for `profile=main10`.
    // ffmpeg's `hevc/ps.c:267-270` reads the same flag word.
    let sps = parse_sps(SPS_1080P_MAIN10_50).expect("parse SPS");
    assert_eq!(sps.general_profile_idc, 2);
    assert_eq!(sps.general_profile_compatibility_flags, 0x2000_0000);
    assert_ne!(
        sps.general_profile_compatibility_flags & (1u32 << 29),
        0,
        "spec-bit 2 (Main10) must be set"
    );
    assert!(sps.general_progressive_source_flag);
    assert!(!sps.general_interlaced_source_flag);
    assert!(sps.general_frame_only_constraint_flag);
    assert!(!sps.general_non_packed_constraint_flag);
}

#[test]
fn parse_sps_preserves_raw_rbsp() {
    let sps = parse_sps(SPS_1080P_MAIN40).expect("parse");
    assert_eq!(sps.raw_rbsp, SPS_1080P_MAIN40);
}

#[test]
fn parse_sps_surfaces_conformance_window_offsets_invariant() {
    // Invariant: post-crop dims + crop offsets reconstruct the coded
    // dimensions exactly. Holds whether or not the fixture has
    // `conformance_window_flag` set (uncropped → all four offsets are
    // zero). Coded dims are also CTB-aligned (the encoder pads pic
    // width/height up to a multiple of MinCbSizeY = 8, so the crop
    // adjusts at most 7 luma samples in each direction — the 1080p
    // Main fixture is coded as 1920×1088 and crops 8 off the bottom).
    for bytes in [SPS_1080P_MAIN40, SPS_1080P_MAIN10_50] {
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
        // MinCbSizeY = 8 for Main / Main10 — coded dims are 8-aligned.
        assert_eq!(sps.coded_width() % 8, 0, "coded_width must be CB-aligned");
        assert_eq!(sps.coded_height() % 8, 0, "coded_height must be CB-aligned");
    }
}

#[test]
fn parse_sps_1080p_has_bottom_crop() {
    // The 1080p HEVC Main fixture is coded as 1920×1088 and signals
    // `conformance_window_flag` with `conf_win_bottom_offset = 2`
    // chroma units. 4:2:0 → SubHeightC = 2 → crop_bottom = 4 luma
    // samples (the parser computes `sub_h * conf_win_bottom_offset`).
    // After crop: 1080. Other three offsets are zero.
    let sps = parse_sps(SPS_1080P_MAIN40).expect("parse");
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
fn parse_sps_returns_err_on_garbage() {
    assert!(parse_sps(&[0xff; 16]).is_err());
}

/// Shared bit-builder. Mirrors the parser's expected reads exactly,
/// keeping the test bytes debuggable by reading the field-write
/// sequence top-to-bottom.
use crate::codec::test_util::BitWriter;
use crate::codec::test_util::escape_emulation;

/// Construct a minimal but complete H.265 SPS RBSP with caller-specified
/// bit_depth_luma_minus8 value. All other fields use safe defaults that allow
/// full parsing to complete. Terminates with rbsp_trailing_bits.
///
/// Per H.265 §7.3.2.2 SPS syntax + §7.3.3 PTL syntax.
fn h265_sps_with_bit_depth_luma_minus8(bit_depth_luma_minus8: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.2 SPS header.
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    // §7.3.3 profile_tier_level(max_sub_layers_minus1 = 0): 96 bits.
    bw.write(0, 2); // general_profile_space
    bw.write(0, 1); // general_tier_flag
    bw.write(1, 5); // general_profile_idc = 1 (Main)
    bw.write(0, 32); // general_profile_compatibility_flags
    bw.write(0, 32); // 32 of the 48 constraint/reserved bits
    bw.write(0, 16); // remaining 16 of the 48 constraint/reserved bits
    bw.write(120, 8); // general_level_idc = 120 (Level 4.0)

    // §7.3.2.2 continues.
    bw.write_ue(0); // sps_seq_parameter_set_id
    bw.write_ue(1); // chroma_format_idc = 1 (4:2:0)
    // separate_colour_plane_flag not coded (chroma_format_idc != 3).
    bw.write_ue(320); // pic_width_in_luma_samples
    bw.write_ue(240); // pic_height_in_luma_samples
    bw.write(0, 1); // conformance_window_flag = 0
    bw.write_ue(bit_depth_luma_minus8); // bit_depth_luma_minus8
    bw.write_ue(bit_depth_luma_minus8); // bit_depth_chroma_minus8 (use same value)
    bw.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4 = 4

    // sub_layer_ordering_info_present_flag = 0, so only one layer
    bw.write(0, 1); // sub_layer_ordering_info_present_flag = 0
    // sps_max_dec_pic_buffering_minus1, sps_max_num_reorder_pics, sps_max_latency_increase_plus1 (all ue)
    bw.write_ue(0);
    bw.write_ue(0);
    bw.write_ue(0);

    // Remaining ue values
    bw.write_ue(0); // log2_min_luma_coding_block_size_minus3
    bw.write_ue(0); // log2_diff_max_min_luma_coding_block_size
    bw.write_ue(0); // log2_min_transform_block_size_minus2
    bw.write_ue(0); // log2_diff_max_min_transform_block_size
    bw.write_ue(0); // max_transform_hierarchy_depth_inter
    bw.write_ue(0); // max_transform_hierarchy_depth_intra

    // Remaining flags
    bw.write(0, 1); // scaling_list_enabled_flag = 0
    bw.write(0, 1); // amp_enabled_flag = 0
    bw.write(0, 1); // sample_adaptive_offset_enabled_flag = 0
    bw.write(0, 1); // pcm_enabled_flag = 0
    bw.write_ue(0); // num_short_term_ref_pic_sets = 0
    bw.write(0, 1); // long_term_ref_pics_present_flag = 0
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag = 0
    bw.write(0, 1); // strong_intra_smoothing_enabled_flag = 0
    bw.write(0, 1); // vui_parameters_present_flag = 0

    // rbsp_trailing_bits: one '1' bit + zero padding to byte align
    bw.write(1, 1);
    while bw.pos % 8 != 0 {
        bw.write(0, 1);
    }

    bw.bytes
}

#[test]
fn h265_sps_accepts_16bit_luma_depth() {
    // H.265 §7.4.3.2.1: bit_depth_luma_minus8 ∈ 0..=8 (bit_depth 8..=16).
    // 15- and 16-bit are conformant syntax, not reserved.
    let rbsp = h265_sps_with_bit_depth_luma_minus8(8);
    let sps = parse_sps(&rbsp).expect("16-bit luma depth is valid H.265 syntax");
    assert_eq!(sps.bit_depth_luma, 16);
}

#[test]
fn h265_sps_rejects_out_of_spec_bit_depth() {
    // minus8 = 9 → bit_depth 17, outside the normative 0..=8 range.
    let rbsp = h265_sps_with_bit_depth_luma_minus8(9);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "bit_depth_luma_minus8",
                value: 9
            })
        ),
        "expected ReservedValue for minus8=9, got {result:?}"
    );
}

/// Per H.265 §7.4.3.2.1, the normative syntax range is `bit_depth_luma_minus8 ∈ 0..=8`
/// (bit_depth ∈ 8..=16). A fuzzed value of 248 would have silently wrapped to
/// `bit_depth_luma = 8` via `8 + (248 as u8)` — caught via `validate_bit_depth_minus8`.
#[test]
fn h265_sps_rejects_bit_depth_overflow() {
    let rbsp = h265_sps_with_bit_depth_luma_minus8(248);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "bit_depth_luma_minus8",
                value: 248
            })
        ),
        "expected ReservedValue, got {result:?}"
    );
}

/// Construct a synthetic H.265 SPS prefix that walks correctly up
/// through `sps_seq_parameter_set_id`, then writes the caller-supplied
/// value at that field. `parse_sps` validates `sps_seq_parameter_set_id`
/// eagerly right after the read, so the bytes after that field don't
/// need to be valid.
///
/// Per H.265 §7.3.2.2 SPS syntax + §7.3.3 PTL syntax with
/// `max_sub_layers_minus1 = 0` (no sublayer fields).
fn h265_sps_with_seq_parameter_set_id(sps_seq_parameter_set_id: u32) -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.2 SPS header.
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    // §7.3.3 profile_tier_level(max_sub_layers_minus1 = 0): 96 bits.
    bw.write(0, 2); // general_profile_space
    bw.write(0, 1); // general_tier_flag
    bw.write(1, 5); // general_profile_idc = 1 (Main)
    bw.write(0, 32); // general_profile_compatibility_flags
    bw.write(0, 32); // 32 of the 48 constraint/reserved bits
    bw.write(0, 16); // remaining 16 of the 48 constraint/reserved bits
    bw.write(120, 8); // general_level_idc = 120 (Level 4.0)

    // §7.3.2.2 continues.
    bw.write_ue(sps_seq_parameter_set_id); // sps_seq_parameter_set_id

    bw.bytes
}

/// Per H.265 §7.4.3.2.1, `sps_seq_parameter_set_id ∈ 0..=15`. The field
/// is the `sps_by_id` map key and is stored in a `u8`; an unchecked
/// `as u8` would silently wrap a fuzzed value of 16 to a valid-looking
/// id, aliasing a different parameter set. Caught now via the eager
/// range check (`read_ue_max`).
#[test]
fn h265_sps_rejects_seq_parameter_set_id_above_15() {
    let rbsp = h265_sps_with_seq_parameter_set_id(16);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "sps_seq_parameter_set_id",
                value: 16
            })
        ),
        "expected ReservedValue, got {result:?}"
    );
}

/// Construct a synthetic H.265 SPS prefix that walks correctly up
/// through `log2_max_pic_order_cnt_lsb_minus4`, then writes the
/// caller-supplied value at that field. `parse_sps` validates eagerly
/// right after the read, so the bytes after that field don't need to
/// be valid.
///
/// Per H.265 §7.3.2.2 SPS syntax + §7.3.3 PTL syntax with
/// `max_sub_layers_minus1 = 0` (no sublayer fields).
fn h265_sps_with_log2_max_pic_order_cnt_lsb_minus4(
    log2_max_pic_order_cnt_lsb_minus4: u32,
) -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.2 SPS header.
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    // §7.3.3 profile_tier_level(max_sub_layers_minus1 = 0): 96 bits.
    bw.write(0, 2); // general_profile_space
    bw.write(0, 1); // general_tier_flag
    bw.write(1, 5); // general_profile_idc = 1 (Main)
    bw.write(0, 32); // general_profile_compatibility_flags
    bw.write(0, 32); // 32 of the 48 constraint/reserved bits
    bw.write(0, 16); // remaining 16 of the 48 constraint/reserved bits
    bw.write(120, 8); // general_level_idc = 120 (Level 4.0)

    // §7.3.2.2 continues.
    bw.write_ue(0); // sps_seq_parameter_set_id
    bw.write_ue(1); // chroma_format_idc = 1 (4:2:0)
    // separate_colour_plane_flag not coded (chroma_format_idc != 3).
    bw.write_ue(320); // pic_width_in_luma_samples
    bw.write_ue(240); // pic_height_in_luma_samples
    bw.write(0, 1); // conformance_window_flag = 0
    bw.write_ue(0); // bit_depth_luma_minus8 = 0 (8-bit)
    bw.write_ue(0); // bit_depth_chroma_minus8 = 0 (8-bit)
    bw.write_ue(log2_max_pic_order_cnt_lsb_minus4); // log2_max_pic_order_cnt_lsb_minus4

    bw.bytes
}

/// Per H.265 §7.4.3.2.1, `log2_max_pic_order_cnt_lsb_minus4 ∈ 0..=12`
/// (valid bit widths 4..=16). The old `parse_sps` used the field as
/// a bit width via `read_u(log2_max + 4)` without bounds-checking — a
/// hostile value of 248 (or anywhere near `u32::MAX`) overflowed the
/// `+ 4` addition. Caught now via the eager range check right after
/// the `read_ue`.
#[test]
fn h265_sps_rejects_log2_max_pic_order_cnt_lsb_minus4_overflow() {
    let rbsp = h265_sps_with_log2_max_pic_order_cnt_lsb_minus4(248);
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "log2_max_pic_order_cnt_lsb_minus4",
                value: 248
            })
        ),
        "expected ReservedValue, got {result:?}"
    );
}

/// Construct a synthetic Main10 H.265 SPS prefix that walks correctly
/// up through `scaling_list_enabled_flag`, then sets both
/// `scaling_list_enabled_flag` and `sps_scaling_list_data_present_flag`
/// to 1 so the parser hits the `scaling_list_data()` gap (§7.3.4).
///
/// Per H.265 §7.3.2.2 SPS syntax + §7.3.3 PTL syntax with
/// `max_sub_layers_minus1 = 0` (no sublayer fields). Profile is Main10
/// (idc=2, compat-bit 2 set) so the test can confirm the error is
/// attributed to the parser gap and **not** to the profile.
fn h265_main10_sps_with_scaling_list_data_present() -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.2 SPS header.
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    // §7.3.3 profile_tier_level(max_sub_layers_minus1 = 0): 96 bits.
    bw.write(0, 2); // general_profile_space
    bw.write(0, 1); // general_tier_flag
    bw.write(2, 5); // general_profile_idc = 2 (Main10)
    bw.write(0x2000_0000, 32); // profile_compatibility_flags (Main10 bit)
    bw.write(0, 32); // 32 of the 48 constraint/reserved bits
    bw.write(0, 16); // remaining 16 constraint/reserved bits
    bw.write(150, 8); // general_level_idc = 150 (Level 5.0)

    // §7.3.2.2 continues.
    bw.write_ue(0); // sps_seq_parameter_set_id
    bw.write_ue(1); // chroma_format_idc = 1 (4:2:0)
    bw.write_ue(320); // pic_width_in_luma_samples
    bw.write_ue(240); // pic_height_in_luma_samples
    bw.write(0, 1); // conformance_window_flag = 0
    bw.write_ue(2); // bit_depth_luma_minus8 = 2 (10-bit)
    bw.write_ue(2); // bit_depth_chroma_minus8 = 2 (10-bit)
    bw.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
    bw.write(0, 1); // sps_sub_layer_ordering_info_present_flag = 0
    bw.write_ue(0); // max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // max_num_reorder_pics[0]
    bw.write_ue(0); // max_latency_increase_plus1[0]
    bw.write_ue(0); // log2_min_luma_coding_block_size_minus3
    bw.write_ue(0); // log2_diff_max_min_luma_coding_block_size
    bw.write_ue(0); // log2_min_luma_transform_block_size_minus2
    bw.write_ue(0); // log2_diff_max_min_luma_transform_block_size
    bw.write_ue(0); // max_transform_hierarchy_depth_inter
    bw.write_ue(0); // max_transform_hierarchy_depth_intra
    bw.write(1, 1); // scaling_list_enabled_flag = 1
    bw.write(1, 1); // sps_scaling_list_data_present_flag = 1
    // Parser exits here with an EngineError — bytes after this are
    // never consumed and need not be valid.

    bw.bytes
}

/// Conformant HDR Main10 streams routinely set
/// `scaling_list_data_present_flag=1`. Prior to validate-1 item H265-V1-M02
/// the parser surfaced this as `UnsupportedProfile { profile_idc: 2 }`,
/// misdirecting consumers debugging HDR streams. The cause is a parser
/// gap (`scaling_list_data()` at H.265 §7.3.4 is not implemented) — it is
/// surfaced via `EngineError` referencing `scaling_list`.
#[test]
fn h265_sps_scaling_list_data_returns_engine_error_not_unsupported_profile() {
    let rbsp = h265_main10_sps_with_scaling_list_data_present();
    let result = parse_sps(&rbsp);
    match &result {
        Err(CodecParseError::EngineError(msg)) => {
            assert!(
                msg.contains("scaling_list"),
                "EngineError should reference scaling_list; got {msg:?}"
            );
        }
        other => panic!("expected EngineError(scaling_list_data ...), got {other:?}"),
    }
    // And explicitly NOT UnsupportedProfile — the bug we're fixing.
    assert!(
        !matches!(result, Err(CodecParseError::UnsupportedProfile { .. })),
        "must not attribute to UnsupportedProfile"
    );
}

/// Build a complete synthetic H.265 SPS RBSP with a configurable RPS
/// section and an optional VUI body. The SPS header fields are fixed
/// (1920×1088, 10-bit, 4:2:0, Level 5.0 High). The caller supplies a
/// closure `write_rps` that writes the `num_short_term_ref_pic_sets` ue(v)
/// value plus the actual RPS body bits. After the RPS section, a VUI is
/// always emitted so tests can verify that the bit cursor lands correctly.
///
/// VUI emitted: `vui_parameters_present_flag=1`, `aspect_ratio_info_present_flag=0`,
/// `overscan_info_present_flag=0`, `video_signal_type_present_flag=1` (format=0,
/// `video_full_range_flag=0`, `colour_description_present_flag=1`, primaries=1
/// BT.709, transfer=1 BT.709, matrix=1 BT.709), `chroma_loc_info_present_flag=0`,
/// `neutral_chroma_indication_flag=0`, `field_seq_flag=0`,
/// `frame_field_info_present_flag=0`, `default_display_window_flag=0`,
/// `vui_timing_info_present_flag=1`, `num_units_in_tick=1001`,
/// `time_scale=30000` → frame_rate ≈ 29.97 fps.
fn build_synthetic_sps(write_rps: impl Fn(&mut BitWriter)) -> Vec<u8> {
    build_synthetic_sps_with_vui_opts(write_rps, None)
}

/// `chroma_loc = Some((top, bottom))` emits `chroma_loc_info_present_flag = 1`
/// with the two `ue(v)` fields; `None` keeps the flag at 0 (default fixture).
fn build_synthetic_sps_with_vui_opts(
    write_rps: impl Fn(&mut BitWriter),
    chroma_loc: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.2 SPS header.
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    // §7.3.3 profile_tier_level(max_sub_layers_minus1 = 0): 96 bits.
    bw.write(0, 2); // general_profile_space
    bw.write(1, 1); // general_tier_flag = 1 (High tier — unique signal for the test)
    bw.write(2, 5); // general_profile_idc = 2 (Main10)
    // profile_compatibility_flags: spec-bit 2 set (Main10 compatible) per §7.3.3
    // MSB-first encoding: spec-bit j lives at 1 << (31 - j), so bit 2 → 1<<29.
    bw.write(0x2000_0000, 32);
    // 48 constraint/reserved bits; set general_progressive_source_flag (the
    // first bit) so we can verify it round-trips.
    bw.write(1, 1); // general_progressive_source_flag = 1
    bw.write(0, 31); // remaining 31 bits of first u32
    bw.write(0, 16); // remaining 16 bits
    bw.write(150, 8); // general_level_idc = 150 (Level 5.0)

    // §7.3.2.2 continues.
    bw.write_ue(0); // sps_seq_parameter_set_id
    bw.write_ue(1); // chroma_format_idc = 1 (4:2:0)
    // separate_colour_plane_flag not coded (chroma_format_idc != 3).
    bw.write_ue(1920); // pic_width_in_luma_samples
    bw.write_ue(1088); // pic_height_in_luma_samples
    bw.write(1, 1); // conformance_window_flag = 1
    bw.write_ue(0); // conf_win_left_offset
    bw.write_ue(0); // conf_win_right_offset
    bw.write_ue(0); // conf_win_top_offset
    bw.write_ue(4); // conf_win_bottom_offset = 4 chroma units → 8 luma (1088→1080)
    bw.write_ue(2); // bit_depth_luma_minus8 = 2 (10-bit)
    bw.write_ue(2); // bit_depth_chroma_minus8 = 2 (10-bit)
    bw.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
    bw.write(0, 1); // sps_sub_layer_ordering_info_present_flag = 0
    // Single sub_layer_ordering_info loop (1 iteration when flag = 0).
    bw.write_ue(0); // max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // max_num_reorder_pics[0]
    bw.write_ue(0); // max_latency_increase_plus1[0]
    // Six ue(v) coding-tool parameters.
    bw.write_ue(0); // log2_min_luma_coding_block_size_minus3
    bw.write_ue(0); // log2_diff_max_min_luma_coding_block_size
    bw.write_ue(0); // log2_min_luma_transform_block_size_minus2
    bw.write_ue(0); // log2_diff_max_min_luma_transform_block_size
    bw.write_ue(0); // max_transform_hierarchy_depth_inter
    bw.write_ue(0); // max_transform_hierarchy_depth_intra
    bw.write(0, 1); // scaling_list_enabled_flag = 0
    bw.write(0, 1); // amp_enabled_flag = 0
    bw.write(0, 1); // sample_adaptive_offset_enabled_flag = 0
    bw.write(0, 1); // pcm_enabled_flag = 0

    // Caller writes num_short_term_ref_pic_sets (ue) + RPS body.
    write_rps(&mut bw);

    // Post-RPS SPS fields.
    bw.write(0, 1); // long_term_ref_pics_present_flag = 0
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag = 0
    bw.write(0, 1); // strong_intra_smoothing_enabled_flag = 0

    // VUI: vui_parameters_present_flag = 1.
    bw.write(1, 1);
    // §E.2.1 vui_parameters():
    bw.write(0, 1); // aspect_ratio_info_present_flag = 0
    bw.write(0, 1); // overscan_info_present_flag = 0
    // video_signal_type_present_flag = 1 so color info is emitted.
    bw.write(1, 1);
    bw.write(0, 3); // video_format = 0 (Component)
    bw.write(0, 1); // video_full_range_flag = 0
    bw.write(1, 1); // colour_description_present_flag = 1
    bw.write(1, 8); // colour_primaries = 1 (BT.709)
    bw.write(1, 8); // transfer_characteristics = 1 (BT.709)
    bw.write(1, 8); // matrix_coeffs = 1 (BT.709)
    match chroma_loc {
        Some((top, bottom)) => {
            bw.write(1, 1); // chroma_loc_info_present_flag = 1
            bw.write_ue(top); // chroma_sample_loc_type_top_field
            bw.write_ue(bottom); // chroma_sample_loc_type_bottom_field
        }
        None => bw.write(0, 1), // chroma_loc_info_present_flag = 0
    }
    bw.write(0, 1); // neutral_chroma_indication_flag = 0
    bw.write(0, 1); // field_seq_flag = 0
    bw.write(0, 1); // frame_field_info_present_flag = 0
    bw.write(0, 1); // default_display_window_flag = 0
    // vui_timing_info_present_flag = 1 so frame_rate is emitted.
    bw.write(1, 1);
    bw.write(1001, 32); // num_units_in_tick = 1001
    bw.write(30000, 32); // time_scale = 30000 → ~29.97 fps
    bw.write(0, 1); // vui_poc_proportional_to_timing_flag = 0

    bw.bytes
}

/// Synthetic SPS RBSP with `num_short_term_ref_pic_sets=0` and VUI.
/// Kept for the existing partial-parse structural-field assertions.
fn h265_sps_with_num_st_rps(num_short_term_ref_pic_sets: u32) -> Vec<u8> {
    // For the pre-existing structural-field test we only need the fields
    // up through num_short_term_ref_pic_sets; the walker calls back to the
    // caller to let it write the actual RPS body.
    build_synthetic_sps(|bw| {
        bw.write_ue(num_short_term_ref_pic_sets);
        // For num_short_term_ref_pic_sets=0 there is nothing more to write.
        // For num_short_term_ref_pic_sets=1, write one explicit RPS:
        //   num_negative_pics=1, num_positive_pics=0
        //   delta_poc_s0_minus1=ue(0)=0, used_by_curr_pic_s0_flag=1
        if num_short_term_ref_pic_sets == 1 {
            // rps_idx=0 → inter_ref_pic_set_prediction_flag not coded.
            bw.write_ue(1); // num_negative_pics = 1
            bw.write_ue(0); // num_positive_pics = 0
            bw.write_ue(0); // delta_poc_s0_minus1[0] = 0
            bw.write(1, 1); // used_by_curr_pic_s0_flag[0] = 1
        }
    })
}

/// Synthetic SPS RBSP with one explicit RPS (num_negative=2, num_positive=0)
/// and VUI. Used by `parse_sps_walks_past_short_term_rps_in_explicit_form`.
fn build_synthetic_sps_with_one_rps_and_vui() -> Vec<u8> {
    build_synthetic_sps(|bw| {
        bw.write_ue(1); // num_short_term_ref_pic_sets = 1
        // rps_idx=0 → inter_ref_pic_set_prediction_flag not coded (§7.3.7).
        bw.write_ue(2); // num_negative_pics = 2
        bw.write_ue(0); // num_positive_pics = 0
        // Two negative delta-POC entries:
        bw.write_ue(0); // delta_poc_s0_minus1[0] = 0
        bw.write(1, 1); // used_by_curr_pic_s0_flag[0] = 1
        bw.write_ue(0); // delta_poc_s0_minus1[1] = 0
        bw.write(1, 1); // used_by_curr_pic_s0_flag[1] = 1
    })
}

/// Synthetic SPS RBSP with two RPSes where the second uses
/// `inter_ref_pic_set_prediction_flag=1`.
///
/// rps_idx=0: explicit, num_negative=2, num_positive=0 → NumDeltaPocs[0]=2.
/// rps_idx=1: inter, delta_idx_minus1=0 (references rps 0), delta_rps_sign=0,
///   abs_delta_rps_minus1=ue(0)=0. Then 3 iterations (0..=NumDeltaPocs[0]=2):
///   - j=0: used=1 (copy), use_delta implicit true → count 1.
///   - j=1: used=1 → count 2.
///   - j=2: used=0, use_delta=1 → count 3.
///
/// NumDeltaPocs[1] = 3.
fn build_synthetic_sps_with_inter_predicted_rps() -> Vec<u8> {
    build_synthetic_sps(|bw| {
        bw.write_ue(2); // num_short_term_ref_pic_sets = 2

        // RPS 0: explicit, num_negative=2, num_positive=0.
        // inter_ref_pic_set_prediction_flag not coded for rps_idx=0.
        bw.write_ue(2); // num_negative_pics = 2
        bw.write_ue(0); // num_positive_pics = 0
        bw.write_ue(0); // delta_poc_s0_minus1[0]
        bw.write(1, 1); // used_by_curr_pic_s0_flag[0]
        bw.write_ue(0); // delta_poc_s0_minus1[1]
        bw.write(1, 1); // used_by_curr_pic_s0_flag[1]

        // RPS 1: inter_ref_pic_set_prediction_flag=1.
        // Per H.265 §7.3.7, delta_idx_minus1 is signaled ONLY when
        // stRpsIdx == num_short_term_ref_pic_sets (slice-header context).
        // In SPS context (this fixture) it is inferred to 0, so DO NOT
        // encode it here — encoding it would mis-align the cursor by
        // exactly the bits of an inferred ue(0)=1.
        bw.write(1, 1); // inter_ref_pic_set_prediction_flag = 1
        bw.write(0, 1); // delta_rps_sign = 0
        bw.write_ue(0); // abs_delta_rps_minus1 = 0
        // Iterate j in 0..=NumDeltaPocs[0]=2 (3 iterations):
        // j=0: used_by_curr_pic_flag=1 → use_delta implicit true.
        bw.write(1, 1); // used_by_curr_pic_flag[0] = 1
        // j=1: used_by_curr_pic_flag=1.
        bw.write(1, 1); // used_by_curr_pic_flag[1] = 1
        // j=2: used_by_curr_pic_flag=0, use_delta_flag=1.
        bw.write(0, 1); // used_by_curr_pic_flag[2] = 0
        bw.write(1, 1); // use_delta_flag[2] = 1
    })
}

/// Full RPS walk: after plan #29 Task 4.1, the parser walks past
/// `num_short_term_ref_pic_sets > 0` and populates VUI fields.
/// Structural fields verified alongside VUI fields to confirm the
/// bit cursor is correctly positioned end-to-end.
#[test]
fn parse_sps_walks_past_short_term_rps_in_explicit_form() {
    let rbsp = build_synthetic_sps_with_one_rps_and_vui();
    let sps = parse_sps(&rbsp).expect("parse SPS with explicit RPS");
    // Structural fields round-trip correctly.
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.bit_depth_luma, 10);
    assert_eq!(sps.general_profile_idc, 2);
    assert!(sps.general_tier_flag);
    assert_eq!(sps.general_level_idc, 150);
    assert!(sps.general_progressive_source_flag);
    // VUI fields: the walker must advance past the RPS region so the bit
    // cursor reaches the VUI. frame_rate and color must be populated.
    assert!(sps.frame_rate.is_some(), "VUI walked through after RPS");
    let fr = sps.frame_rate.unwrap();
    assert_eq!(fr.num, 30000);
    assert_eq!(fr.den, 1001);
    assert!(sps.color.is_some(), "VUI color walked through after RPS");
}

#[test]
fn parse_sps_walks_past_short_term_rps_with_inter_prediction() {
    let rbsp = build_synthetic_sps_with_inter_predicted_rps();
    let sps = parse_sps(&rbsp).expect("parse SPS with inter-predicted RPS");
    assert!(sps.frame_rate.is_some(), "VUI walked past two-RPS region");
    let fr = sps.frame_rate.unwrap();
    assert_eq!(fr.num, 30000);
    assert_eq!(fr.den, 1001);
}

/// Build an adversarial SPS whose conformance-window offsets sum past
/// `u32::MAX`. Mirrors `build_synthetic_sps` (1920×1088 / 10-bit / 4:2:0
/// / Level 5.0) but parameterizes the four ue(v) crop offsets and ends
/// with a minimal RPS (`num_short_term_ref_pic_sets = 0`) + VUI so the
/// parser walks the full SPS and reaches the crop arithmetic at the
/// end of `parse_sps`.
fn build_sps_with_conf_window_offsets(
    conf_left: u32,
    conf_right: u32,
    conf_top: u32,
    conf_bottom: u32,
) -> Vec<u8> {
    let mut bw = BitWriter::new();

    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sub_layers_minus1 = 0
    bw.write(0, 1); // sps_temporal_id_nesting_flag

    bw.write(0, 2); // general_profile_space
    bw.write(1, 1); // general_tier_flag
    bw.write(2, 5); // general_profile_idc = 2 (Main10)
    bw.write(0x2000_0000, 32); // profile_compatibility_flags (Main10 bit)
    bw.write(1, 1); // general_progressive_source_flag
    bw.write(0, 31);
    bw.write(0, 16);
    bw.write(150, 8); // general_level_idc

    bw.write_ue(0); // sps_seq_parameter_set_id
    bw.write_ue(1); // chroma_format_idc = 1 (4:2:0 → sub_w=sub_h=2)
    bw.write_ue(1920); // pic_width_in_luma_samples
    bw.write_ue(1088); // pic_height_in_luma_samples
    bw.write(1, 1); // conformance_window_flag = 1
    bw.write_ue(conf_left);
    bw.write_ue(conf_right);
    bw.write_ue(conf_top);
    bw.write_ue(conf_bottom);
    bw.write_ue(2); // bit_depth_luma_minus8 = 2 (10-bit)
    bw.write_ue(2); // bit_depth_chroma_minus8 = 2
    bw.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
    bw.write(0, 1); // sps_sub_layer_ordering_info_present_flag = 0
    bw.write_ue(0); // max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // max_num_reorder_pics[0]
    bw.write_ue(0); // max_latency_increase_plus1[0]
    bw.write_ue(0); // log2_min_luma_coding_block_size_minus3
    bw.write_ue(0); // log2_diff_max_min_luma_coding_block_size
    bw.write_ue(0); // log2_min_luma_transform_block_size_minus2
    bw.write_ue(0); // log2_diff_max_min_luma_transform_block_size
    bw.write_ue(0); // max_transform_hierarchy_depth_inter
    bw.write_ue(0); // max_transform_hierarchy_depth_intra
    bw.write(0, 1); // scaling_list_enabled_flag
    bw.write(0, 1); // amp_enabled_flag
    bw.write(0, 1); // sample_adaptive_offset_enabled_flag
    bw.write(0, 1); // pcm_enabled_flag
    bw.write_ue(0); // num_short_term_ref_pic_sets = 0
    bw.write(0, 1); // long_term_ref_pics_present_flag
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag
    bw.write(0, 1); // strong_intra_smoothing_enabled_flag
    bw.write(0, 1); // vui_parameters_present_flag = 0

    bw.bytes
}

/// Regression for unchecked u32 arithmetic in the conformance-window crop
/// at the end of `parse_sps`: both `sub_w * conf_win_*_offset` and
/// `crop_x_left + crop_x_right` could overflow on hostile input. With
/// `chroma_format_idc = 1` (sub_w = 2), the case `(1 << 30, 1 << 30)`
/// triggers the addition path (`(1<<31) + (1<<31) = 1<<32`); the case
/// `(1 << 31, 0)` triggers the multiplication path (`2 * (1<<31) = 1<<32`).
/// Both are rejected at parse time as `ReservedValue` (review 9, ext
/// R9-03); the fixture is emulation-escaped so the hostile ue(v) values
/// read back exactly as written.
#[test]
fn parse_sps_saturates_crop_on_adversarial_offsets() {
    for (conf_left, conf_right) in [(1u32 << 30, 1u32 << 30), (1u32 << 31, 0u32)] {
        let rbsp = build_sps_with_conf_window_offsets(conf_left, conf_right, 0, 0);
        match parse_sps(&escape_emulation(&rbsp)) {
            Err(CodecParseError::ReservedValue { field, .. }) => {
                assert!(field.starts_with("conf_win_left"), "{field}")
            }
            other => panic!("({conf_left}, {conf_right}) must be rejected, got {other:?}"),
        }
    }
}

/// Review 9 (ext R9-03): a conformance window that eats the whole picture
/// is rejected at parse time, so `coded_width()` / `coded_height()` can
/// never overflow (they panicked in debug and wrapped in release).
#[test]
fn hostile_crop_cannot_escape_into_an_overflowing_getter() {
    let valid = escape_emulation(&build_sps_with_conf_window_offsets(0, 0, 0, 0));
    let sps = parse_sps(&valid).unwrap();
    assert_eq!((sps.coded_width(), sps.coded_height()), (1920, 1088));
    // A legitimate window still crops: 4:2:0 doubles every offset.
    let cropped = escape_emulation(&build_sps_with_conf_window_offsets(2, 2, 0, 4));
    let sps = parse_sps(&cropped).unwrap();
    assert_eq!((sps.width, sps.height), (1912, 1080));
    assert_eq!((sps.coded_width(), sps.coded_height()), (1920, 1088));
    // Just under the raw width is still a (2-sample-wide) picture: accepted.
    let edge = escape_emulation(&build_sps_with_conf_window_offsets(959, 0, 0, 0));
    let sps = parse_sps(&edge).unwrap();
    assert_eq!(sps.width, 2);
    assert_eq!(sps.coded_width(), 1920);
    for (left, right, top, bottom) in [
        (1u32 << 30, 1u32 << 30, 0u32, 0u32), // sum wraps past u32
        (0, 0, 1 << 30, 1 << 30),
        (960, 0, 0, 0), // exactly the width: not < raw
        (0, 0, 0, 544), // exactly the height
    ] {
        let raw = build_sps_with_conf_window_offsets(left, right, top, bottom);
        match parse_sps(&escape_emulation(&raw)) {
            Err(CodecParseError::ReservedValue { field, .. }) => {
                assert!(field.starts_with("conf_win_"), "{field}")
            }
            other => panic!("({left},{right},{top},{bottom}) must be rejected, got {other:?}"),
        }
    }
}

/// Regression for the OOM-abort DoS: a crafted `num_short_term_ref_pic_sets`
/// ue(v) near 2^31 caused `Vec::with_capacity` in `walk_short_term_ref_pic_sets`
/// to request ~16 GB up front, aborting the process via `handle_alloc_error`.
/// H.265 Table A.8 caps `num_short_term_ref_pic_sets` at 64; values beyond
/// that must return `ReservedValue` immediately — NOT abort or attempt the
/// giant allocation.
///
/// The pre-fix behavior on this machine was `TruncatedRbsp` (the overcommit
/// allocation succeeded but the loop immediately exhausted the tiny RBSP
/// buffer). On systems without memory overcommit it aborts. Either way the
/// root cause is the unguarded `with_capacity`; the fix returns `ReservedValue`
/// before the allocation is attempted.
#[test]
fn parse_sps_rejects_oversized_num_short_term_ref_pic_sets() {
    // Use build_synthetic_sps so the RBSP prefix is valid up to the RPS field,
    // then write a ue(v) encoding 2^31 − 1. After the fix the parser must
    // return Err(ReservedValue) immediately, before touching the allocator.
    let rbsp = build_synthetic_sps(|bw| {
        bw.write_ue(u32::MAX / 2); // ~2^31 — far above the spec max of 64
    });
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "num_short_term_ref_pic_sets",
                ..
            })
        ),
        "expected ReservedValue for num_short_term_ref_pic_sets, got {result:?}"
    );
}

/// Boundary: num_short_term_ref_pic_sets = 64 (H.265 spec max per Table A.8)
/// must parse successfully. Each explicit RPS encodes num_negative=0,
/// num_positive=0; rps_idx > 0 additionally emits the
/// inter_ref_pic_set_prediction_flag=0 bit.
#[test]
fn parse_sps_accepts_num_short_term_ref_pic_sets_at_spec_max() {
    let rbsp = build_synthetic_sps(|bw| {
        bw.write_ue(64); // spec max — must NOT be rejected
        for rps_idx in 0u32..64 {
            if rps_idx > 0 {
                bw.write(0, 1); // inter_ref_pic_set_prediction_flag = 0 (explicit)
            }
            bw.write_ue(0); // num_negative_pics = 0
            bw.write_ue(0); // num_positive_pics = 0
        }
    });
    let result = parse_sps(&rbsp);
    assert!(
        result.is_ok(),
        "parse_sps must accept num_short_term_ref_pic_sets=64 (spec max); got {result:?}"
    );
}

#[test]
fn h265_sps_rejects_out_of_range_bottom_chroma_loc() {
    // H.265 Table E.1 bounds both chroma_sample_loc fields at 0..=5.
    let rbsp = build_synthetic_sps_with_vui_opts(|bw| bw.write_ue(0), Some((0, 6)));
    let result = parse_sps(&rbsp);
    assert!(
        matches!(
            result,
            Err(CodecParseError::ReservedValue {
                field: "chroma_sample_loc_type_bottom_field",
                value: 6
            })
        ),
        "expected ReservedValue for bottom_field=6, got {result:?}"
    );
}

#[test]
fn h265_sps_accepts_valid_chroma_loc() {
    let rbsp = build_synthetic_sps_with_vui_opts(|bw| bw.write_ue(0), Some((2, 3)));
    let _ = parse_sps(&rbsp).expect("chroma_sample_loc within 0..=5 is valid");
}

/// Full-walk structural-field check: same assertions as the old partial-parse
/// test, but the SPS is now fully parseable (includes RPS body and VUI).
/// VUI fields are asserted to be populated — the bit cursor reaches VUI.
#[test]
fn parse_sps_walks_past_num_st_rps_and_populates_vui() {
    let rbsp = h265_sps_with_num_st_rps(1);
    let sps = parse_sps(&rbsp).expect("parse should succeed with full RPS walk");

    // Structural fields populated correctly.
    assert_eq!(sps.width, 1920);
    assert_eq!(sps.height, 1080);
    assert_eq!(sps.coded_width(), 1920);
    assert_eq!(sps.coded_height(), 1088);
    assert_eq!(sps.crop_bottom, 8);
    assert_eq!(sps.bit_depth_luma, 10);
    assert_eq!(sps.bit_depth_chroma, 10);
    assert_eq!(sps.chroma_format, ChromaFormat::Yuv420);
    assert_eq!(sps.general_profile_idc, 2);
    assert!(sps.general_tier_flag);
    assert_eq!(sps.general_level_idc, 150);
    assert_ne!(
        sps.general_profile_compatibility_flags & (1u32 << 29),
        0,
        "Main10 compatibility bit (spec-bit 2 → 1<<29 MSB-first) must round-trip"
    );
    assert!(sps.general_progressive_source_flag);

    // VUI fields populated — full walk reaches past the RPS region.
    assert!(
        sps.frame_rate.is_some(),
        "VUI timing populated after RPS walk"
    );
    assert!(sps.color.is_some(), "VUI color populated after RPS walk");
}
