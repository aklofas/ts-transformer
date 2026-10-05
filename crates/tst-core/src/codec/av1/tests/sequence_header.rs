//! AV1 Sequence Header parser tests.

use crate::codec::av1::parse_sequence_header;
use crate::codec::{
    ChromaFormat, CodecParseError, ColourPrimaries, MatrixCoefficients, TransferCharacteristics,
};

// ---------------------------------------------------------------------------
// Shared helpers for timing-info payload builders
// ---------------------------------------------------------------------------

/// Write the sequence header prefix up to and including `equal_picture_interval=1`.
/// Callers write the uvlc-encoded `num_ticks_per_picture_minus_1` immediately after.
fn write_timing_prefix(bw: &mut BitWriter, num_units: u32, time_scale: u32) {
    bw.write(0, 3); // seq_profile = 0 (Main)
    bw.write(0, 1); // still_picture = 0
    bw.write(0, 1); // reduced_still_picture_header = 0
    bw.write(1, 1); // timing_info_present_flag = 1
    bw.write(u64::from(num_units), 32); // num_units_in_display_tick
    bw.write(u64::from(time_scale), 32); // time_scale
    bw.write(1, 1); // equal_picture_interval = 1
}

/// Write decoder_model_info_present_flag=0 and all subsequent syntax
/// elements through film_grain_params_present=0 — the invariant suffix
/// shared by all timing-info payload builders (Main profile, 8-bit 4:2:0,
/// 320×240, no color description, limited range, single operating point).
fn write_timing_suffix(bw: &mut BitWriter) {
    bw.write(0, 1); // decoder_model_info_present_flag = 0
    bw.write(0, 1); // initial_display_delay_present_flag = 0
    bw.write(0, 5); // operating_points_cnt_minus_1 = 0
    // i=0 operating point:
    bw.write(0, 12); // operating_point_idc[0] = 0
    bw.write(0, 5); // seq_level_idx[0] = 0 (level 2.0; ≤7 so no seq_tier)
    // decoder_model_present_for_this_op + initial_display_delay_present_for_this_op
    // both skipped (gate flags are 0).

    bw.write(8, 4); // frame_width_bits_minus_1 = 8 (9 bits → max 512-1)
    bw.write(7, 4); // frame_height_bits_minus_1 = 7 (8 bits → max 256-1)
    bw.write(319, 9); // max_frame_width_minus_1 = 319 → width 320
    bw.write(239, 8); // max_frame_height_minus_1 = 239 → height 240

    bw.write(0, 1); // frame_id_numbers_present_flag = 0
    bw.write(0, 1); // use_128x128_superblock = 0
    bw.write(0, 1); // enable_filter_intra = 0
    bw.write(0, 1); // enable_intra_edge_filter = 0

    // !reduced_still_picture_header → tool flags are coded:
    bw.write(0, 1); // enable_interintra_compound = 0
    bw.write(0, 1); // enable_masked_compound = 0
    bw.write(0, 1); // enable_warped_motion = 0
    bw.write(0, 1); // enable_dual_filter = 0
    bw.write(0, 1); // enable_order_hint = 0
    // enable_order_hint=0 → enable_jnt_comp + enable_ref_frame_mvs not coded
    bw.write(0, 1); // seq_choose_screen_content_tools = 0
    bw.write(0, 1); // seq_force_screen_content_tools = 0
    // seq_force_screen_content_tools=0 → integer-mv bits not coded
    // enable_order_hint=0 → order_hint_bits_minus_1 not coded

    bw.write(0, 1); // enable_superres = 0
    bw.write(0, 1); // enable_cdef = 0
    bw.write(0, 1); // enable_restoration = 0

    // color_config():
    bw.write(0, 1); // high_bitdepth = 0 → BitDepth = 8
    bw.write(0, 1); // mono_chrome = 0
    bw.write(0, 1); // color_description_present_flag = 0
    // !mono_chrome AND defaults CP_UNSPECIFIED ≠ CP_BT_709 → else branch:
    bw.write(0, 1); // color_range = 0 (limited)
    // profile == 0 → subsampling_x=1, subsampling_y=1 (NOT coded)
    bw.write(0, 2); // chroma_sample_position = 0 (CSP_UNKNOWN)
    bw.write(0, 1); // separate_uv_delta_q = 0

    bw.write(0, 1); // film_grain_params_present = 0
}

/// Append-only bit writer for hand-crafting AV1 OBU payloads.
use crate::codec::test_util::BitWriter;

/// Build a minimal Sequence Header OBU body.
/// Main profile, non-still-picture, level 2.0, seq_tier 0, 320x240,
/// 8-bit 4:2:0, no color description, no timing info.
/// Walks §5.5.1 in order — every `bw.write` is a single named
/// syntax element from the spec.
fn minimal_sequence_header() -> Vec<u8> {
    let mut bw = BitWriter::new();

    bw.write(0, 3); // seq_profile = 0 (Main)
    bw.write(0, 1); // still_picture = 0
    bw.write(0, 1); // reduced_still_picture_header = 0
    bw.write(0, 1); // timing_info_present_flag = 0
    // (timing_info(), decoder_model_info_present_flag, decoder_model_info()
    // all skipped because timing_info_present_flag == 0.)
    bw.write(0, 1); // initial_display_delay_present_flag = 0
    bw.write(0, 5); // operating_points_cnt_minus_1 = 0
    bw.write(0, 12); // operating_point_idc[0] = 0
    bw.write(0, 5); // seq_level_idx[0] = 0 (level 2.0; <=7 so no seq_tier)
    // (decoder_model_present_for_this_op + initial_display_delay_present_for_this_op
    // both skipped since their gate flags are 0.)

    bw.write(8, 4); // frame_width_bits_minus_1 = 8 (n=9 fits 320-1=319=0x13F)
    bw.write(7, 4); // frame_height_bits_minus_1 = 7 (n=8 fits 240-1=239=0xEF)
    bw.write(319, 9); // max_frame_width_minus_1 = 319
    bw.write(239, 8); // max_frame_height_minus_1 = 239

    bw.write(0, 1); // frame_id_numbers_present_flag = 0
    bw.write(0, 1); // use_128x128_superblock = 0
    bw.write(0, 1); // enable_filter_intra = 0
    bw.write(0, 1); // enable_intra_edge_filter = 0

    // !reduced_still_picture_header → tool flags are coded:
    bw.write(0, 1); // enable_interintra_compound = 0
    bw.write(0, 1); // enable_masked_compound = 0
    bw.write(0, 1); // enable_warped_motion = 0
    bw.write(0, 1); // enable_dual_filter = 0
    bw.write(0, 1); // enable_order_hint = 0
    // enable_order_hint=0 → enable_jnt_comp + enable_ref_frame_mvs not coded
    bw.write(0, 1); // seq_choose_screen_content_tools = 0
    bw.write(0, 1); // seq_force_screen_content_tools = 0
    // seq_force_screen_content_tools == 0 → seq_choose_integer_mv +
    // seq_force_integer_mv NOT coded (SELECT_INTEGER_MV implicit).
    // enable_order_hint=0 → order_hint_bits_minus_1 NOT coded.

    bw.write(0, 1); // enable_superres = 0
    bw.write(0, 1); // enable_cdef = 0
    bw.write(0, 1); // enable_restoration = 0

    // color_config():
    bw.write(0, 1); // high_bitdepth = 0 → BitDepth = 8
    // profile != 1 → mono_chrome IS coded:
    bw.write(0, 1); // mono_chrome = 0
    bw.write(0, 1); // color_description_present_flag = 0
    // !mono_chrome AND defaults are CP_UNSPECIFIED ≠ CP_BT_709 → else branch:
    bw.write(0, 1); // color_range = 0 (limited)
    // profile == 0 → subsampling_x=1, subsampling_y=1 (NOT coded)
    // subsampling_x && subsampling_y → chroma_sample_position f(2):
    bw.write(0, 2); // chroma_sample_position = 0 (CSP_UNKNOWN)
    bw.write(0, 1); // separate_uv_delta_q = 0

    bw.write(0, 1); // film_grain_params_present = 0

    bw.bytes
}

#[test]
fn parse_sequence_header_main_320x240() {
    let payload = minimal_sequence_header();
    let seq = parse_sequence_header(&payload).expect("should parse");
    assert_eq!(seq.profile, 0);
    assert_eq!(seq.level, 0);
    assert_eq!(seq.tier, 0);
    assert_eq!(seq.max_frame_width, 320);
    assert_eq!(seq.max_frame_height, 240);
    assert_eq!(seq.bit_depth, 8);
    assert!(!seq.monochrome);
    assert_eq!(seq.chroma_format, ChromaFormat::Yuv420);
    assert!(!seq.still_picture);
    assert!(!seq.reduced_still_picture_header);
    // Even though color_description_present_flag == 0, the wire-format still
    // carries the color_range bit per AV1 §6.4.2; we surface it via ColorInfo
    // with UNSPECIFIED primaries/transfer/matrix so callers can read the
    // dynamic-range signal. minimal_sequence_header() encodes color_range = 0.
    let ci = seq
        .color_info
        .as_ref()
        .expect("color_info should be Some so full_range is surfaced");
    assert_eq!(ci.primaries, ColourPrimaries::Unspecified);
    assert_eq!(ci.transfer, TransferCharacteristics::Unspecified);
    assert_eq!(ci.matrix, MatrixCoefficients::Unspecified);
    assert!(!ci.full_range);
    assert_eq!(seq.frame_rate, None);
}

/// Same shape as the minimal payload, but `color_range = 1` (full range).
/// Verifies the implicit-color-description branch still threads the
/// wire-read color_range bit through to `ColorInfo.full_range`.
fn minimal_sequence_header_full_range() -> Vec<u8> {
    let mut bw = BitWriter::new();

    bw.write(0, 3); // seq_profile = 0
    bw.write(0, 1); // still_picture
    bw.write(0, 1); // reduced_still_picture_header
    bw.write(0, 1); // timing_info_present_flag
    bw.write(0, 1); // initial_display_delay_present_flag
    bw.write(0, 5); // operating_points_cnt_minus_1
    bw.write(0, 12); // operating_point_idc[0]
    bw.write(0, 5); // seq_level_idx[0]

    bw.write(8, 4); // frame_width_bits_minus_1
    bw.write(7, 4); // frame_height_bits_minus_1
    bw.write(319, 9); // max_frame_width_minus_1
    bw.write(239, 8); // max_frame_height_minus_1

    bw.write(0, 1); // frame_id_numbers_present_flag
    bw.write(0, 1); // use_128x128_superblock
    bw.write(0, 1); // enable_filter_intra
    bw.write(0, 1); // enable_intra_edge_filter

    bw.write(0, 1); // enable_interintra_compound
    bw.write(0, 1); // enable_masked_compound
    bw.write(0, 1); // enable_warped_motion
    bw.write(0, 1); // enable_dual_filter
    bw.write(0, 1); // enable_order_hint
    bw.write(0, 1); // seq_choose_screen_content_tools
    bw.write(0, 1); // seq_force_screen_content_tools

    bw.write(0, 1); // enable_superres
    bw.write(0, 1); // enable_cdef
    bw.write(0, 1); // enable_restoration

    bw.write(0, 1); // high_bitdepth
    bw.write(0, 1); // mono_chrome
    bw.write(0, 1); // color_description_present_flag = 0
    bw.write(1, 1); // color_range = 1 (full range)
    bw.write(0, 2); // chroma_sample_position
    bw.write(0, 1); // separate_uv_delta_q

    bw.write(0, 1); // film_grain_params_present
    bw.bytes
}

#[test]
fn parse_sequence_header_implicit_color_range_full() {
    // When color_description_present_flag=0
    // the parser must still read the color_range bit and surface it via
    // ColorInfo.full_range — not discard it.
    let payload = minimal_sequence_header_full_range();
    let seq = parse_sequence_header(&payload).expect("should parse");
    let ci = seq
        .color_info
        .as_ref()
        .expect("color_info should be Some even without explicit color description");
    assert_eq!(ci.primaries, ColourPrimaries::Unspecified);
    assert_eq!(ci.transfer, TransferCharacteristics::Unspecified);
    assert_eq!(ci.matrix, MatrixCoefficients::Unspecified);
    assert!(
        ci.full_range,
        "wire color_range=1 should map to full_range=true"
    );
}

/// Reduced still-picture variant — single op, seq_level_idx[0] f(5)
/// is coded directly (no operating_points_cnt_minus_1, no
/// operating_point_idc[], no seq_tier).
fn reduced_still_picture_header_payload() -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.write(0, 3); // seq_profile
    bw.write(1, 1); // still_picture = 1
    bw.write(1, 1); // reduced_still_picture_header = 1
    bw.write(0, 5); // seq_level_idx[0] = 0
    bw.write(8, 4); // frame_width_bits_minus_1 = 8
    bw.write(7, 4); // frame_height_bits_minus_1 = 7
    bw.write(319, 9); // max_frame_width_minus_1
    bw.write(239, 8); // max_frame_height_minus_1
    // reduced → frame_id_numbers_present NOT coded.
    bw.write(0, 1); // use_128x128_superblock
    bw.write(0, 1); // enable_filter_intra
    bw.write(0, 1); // enable_intra_edge_filter
    // reduced → tool flags + screen-content + integer-mv + order-hint NOT coded.
    bw.write(0, 1); // enable_superres
    bw.write(0, 1); // enable_cdef
    bw.write(0, 1); // enable_restoration
    bw.write(0, 1); // high_bitdepth = 0
    bw.write(0, 1); // mono_chrome = 0
    bw.write(0, 1); // color_description_present_flag
    bw.write(0, 1); // color_range
    bw.write(0, 2); // chroma_sample_position
    bw.write(0, 1); // separate_uv_delta_q
    // reduced → film_grain_params_present NOT coded.
    bw.bytes
}

#[test]
fn parse_sequence_header_reduced_still_picture() {
    let payload = reduced_still_picture_header_payload();
    let seq = parse_sequence_header(&payload).expect("should parse");
    assert!(seq.still_picture);
    assert!(seq.reduced_still_picture_header);
    assert_eq!(seq.max_frame_width, 320);
    assert_eq!(seq.max_frame_height, 240);
    assert_eq!(seq.chroma_format, ChromaFormat::Yuv420);
}

/// Same as the minimal payload but with a color_description block
/// asserting BT.2020 + PQ + BT.2020-NCL.
fn payload_with_color_description() -> Vec<u8> {
    let mut bw = BitWriter::new();

    bw.write(0, 3); // seq_profile
    bw.write(0, 1); // still_picture
    bw.write(0, 1); // reduced_still_picture_header
    bw.write(0, 1); // timing_info_present_flag
    bw.write(0, 1); // initial_display_delay_present_flag
    bw.write(0, 5); // operating_points_cnt_minus_1
    bw.write(0, 12); // operating_point_idc[0]
    bw.write(0, 5); // seq_level_idx[0]

    bw.write(8, 4);
    bw.write(7, 4);
    bw.write(319, 9);
    bw.write(239, 8);

    bw.write(0, 1); // frame_id_numbers_present
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1); // enable_order_hint
    bw.write(0, 1); // seq_choose_screen_content_tools
    bw.write(0, 1); // seq_force_screen_content_tools

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);

    // color_config:
    bw.write(0, 1); // high_bitdepth = 0 → BitDepth=8
    bw.write(0, 1); // mono_chrome = 0
    bw.write(1, 1); // color_description_present_flag = 1
    bw.write(9, 8); // color_primaries = 9 (BT.2020)
    bw.write(16, 8); // transfer_characteristics = 16 (PQ)
    bw.write(9, 8); // matrix_coefficients = 9 (BT.2020 NCL)
    // !mono_chrome, AND not the special BT.709/sRGB/Identity case → else:
    bw.write(1, 1); // color_range = 1 (full)
    // profile == 0 → subsampling 1,1 (not coded)
    bw.write(0, 2); // chroma_sample_position
    bw.write(0, 1); // separate_uv_delta_q

    bw.write(0, 1); // film_grain_params_present
    bw.bytes
}

#[test]
fn parse_sequence_header_color_description_bt2020_pq() {
    let payload = payload_with_color_description();
    let seq = parse_sequence_header(&payload).expect("should parse");
    let ci = seq.color_info.expect("color_info should be Some");
    assert_eq!(ci.primaries, ColourPrimaries::Bt2020);
    assert_eq!(ci.transfer, TransferCharacteristics::SmpteSt2084);
    assert_eq!(ci.matrix, MatrixCoefficients::Bt2020NonConstant);
    assert!(ci.full_range);
}

/// Timing info present with `equal_picture_interval=1` → frame_rate
/// is surfaced as time_scale / num_units_in_display_tick.
/// 30000 / 1001 = 29.97 fps.
fn payload_with_timing_info() -> Vec<u8> {
    let mut bw = BitWriter::new();

    bw.write(0, 3); // seq_profile
    bw.write(0, 1); // still_picture
    bw.write(0, 1); // reduced_still_picture_header
    bw.write(1, 1); // timing_info_present_flag = 1
    bw.write(1001, 32); // num_units_in_display_tick
    bw.write(30000, 32); // time_scale
    bw.write(1, 1); // equal_picture_interval = 1
    bw.write(1, 1); // num_ticks_per_picture_minus_1 = 0 (uvlc encoding)
    bw.write(0, 1); // decoder_model_info_present_flag = 0
    bw.write(0, 1); // initial_display_delay_present_flag = 0
    bw.write(0, 5); // operating_points_cnt_minus_1
    bw.write(0, 12); // operating_point_idc[0]
    bw.write(0, 5); // seq_level_idx[0]

    bw.write(8, 4);
    bw.write(7, 4);
    bw.write(319, 9);
    bw.write(239, 8);

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);

    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 1);
    bw.write(0, 2);
    bw.write(0, 1);

    bw.write(0, 1);
    bw.bytes
}

#[test]
fn parse_sequence_header_timing_info_surfaces_frame_rate() {
    let payload = payload_with_timing_info();
    let seq = parse_sequence_header(&payload).expect("should parse");
    let fr = seq.frame_rate.expect("frame_rate should be Some");
    assert_eq!(fr.num, 30000);
    assert_eq!(fr.den, 1001);
}

#[test]
fn truncated_payload_returns_err() {
    let payload = vec![0u8; 2]; // Way too short.
    let r = parse_sequence_header(&payload);
    assert!(r.is_err());
}

#[test]
fn seq_profile_above_2_is_reserved() {
    // First 3 bits = seq_profile = 3 (0b011). Remaining bits arbitrary;
    // parse must reject before consuming them.
    let payload = [0b0110_0000u8, 0x00, 0x00, 0x00];
    let r = parse_sequence_header(&payload);
    assert!(matches!(
        r,
        Err(CodecParseError::ReservedValue {
            field: "seq_profile",
            value: 3
        })
    ));
}

// ---------------------------------------------------------------------------
// Correct constant frame-rate denominator + reject zero timing
// ---------------------------------------------------------------------------

/// ticks_minus_1 = 1 (uvlc `010`, 3 bits): denominator doubles.
/// 60000 / (1000 * 2) = 30/1 after GCD reduction.
#[test]
fn timing_info_ticks_1_halves_frame_rate() {
    let mut bw = BitWriter::new();
    write_timing_prefix(&mut bw, 1000, 60000);
    // uvlc(1): 1 leading zero + marker 1 + 1 extra bit = 0 → `010`
    bw.write(0b010, 3); // num_ticks_per_picture_minus_1 = 1
    write_timing_suffix(&mut bw);

    let seq = parse_sequence_header(&bw.bytes).expect("should parse");
    let fr = seq.frame_rate.expect("frame_rate should be Some");
    // ticks = 2, den = 1000 * 2 = 2000, gcd(60000, 2000) = 2000 → 30/1
    assert_eq!(fr.num, 30);
    assert_eq!(fr.den, 1);
}

/// ticks_minus_1 = 99 (uvlc: 6 leading zeros + marker + 6 extra bits = 36):
/// exercises the checked-multiply path without overflow.
/// 30000 / (1000 * 100) → gcd(30000, 100000) = 10000 → 3/10.
#[test]
fn timing_info_large_ticks_reduces_correctly() {
    let mut bw = BitWriter::new();
    write_timing_prefix(&mut bw, 1000, 30000);
    // uvlc(99): 99 = (1<<6) - 1 + 36, so 6 leading zeros + marker + 6-bit extra = 36
    bw.write(0, 6); // 6 leading zeros
    bw.write(1, 1); // marker bit
    bw.write(36, 6); // extra bits → num_ticks_per_picture_minus_1 = 99
    write_timing_suffix(&mut bw);

    let seq = parse_sequence_header(&bw.bytes).expect("should parse");
    let fr = seq.frame_rate.expect("frame_rate should be Some");
    // ticks = 100, den = 1000 * 100 = 100000, gcd(30000, 100000) = 10000 → 3/10
    assert_eq!(fr.num, 3);
    assert_eq!(fr.den, 10);
}

/// AV1 §6.4.3: num_units_in_display_tick == 0 is forbidden → ReservedValue.
#[test]
fn timing_info_zero_num_units_is_reserved() {
    let mut bw = BitWriter::new();
    bw.write(0, 3); // seq_profile = 0
    bw.write(0, 1); // still_picture = 0
    bw.write(0, 1); // reduced_still_picture_header = 0
    bw.write(1, 1); // timing_info_present_flag = 1
    bw.write(0, 32); // num_units_in_display_tick = 0  ← forbidden
    bw.write(30000, 32); // time_scale (irrelevant; parse rejects before here)
    // Parser returns before reading equal_picture_interval; no further bits needed.
    bw.write(0, 8); // padding so the reader doesn't hit TruncatedRbsp first

    let r = parse_sequence_header(&bw.bytes);
    assert!(
        matches!(
            r,
            Err(CodecParseError::ReservedValue {
                field: "num_units_in_display_tick",
                value: 0
            })
        ),
        "expected ReservedValue for num_units=0, got {r:?}"
    );
}

/// AV1 §6.4.3: time_scale == 0 is forbidden → ReservedValue.
#[test]
fn timing_info_zero_time_scale_is_reserved() {
    let mut bw = BitWriter::new();
    bw.write(0, 3); // seq_profile = 0
    bw.write(0, 1); // still_picture = 0
    bw.write(0, 1); // reduced_still_picture_header = 0
    bw.write(1, 1); // timing_info_present_flag = 1
    bw.write(1001, 32); // num_units_in_display_tick (nonzero)
    bw.write(0, 32); // time_scale = 0  ← forbidden
    // Parser returns before reading equal_picture_interval; no further bits needed.
    bw.write(0, 8); // padding so the reader doesn't hit TruncatedRbsp first

    let r = parse_sequence_header(&bw.bytes);
    assert!(
        matches!(
            r,
            Err(CodecParseError::ReservedValue {
                field: "time_scale",
                value: 0
            })
        ),
        "expected ReservedValue for time_scale=0, got {r:?}"
    );
}
