//! Bit-position tracer for the H.265 SPS RBSP walker. Maintainer tool only;
//! intended for diagnosing cursor-misalignment bugs surfaced by the
//! conformance test suite.
//!
//! Usage:
//!   cargo run -p tst-core --bin trace-h265-sps -- <path-to-sps.bin>
//!
//! Output (stdout) is a sequence of lines, one per major SPS field:
//!   <decimal-bit-position>  <field-name>  <value>
//!
//! Compare this line-for-line against the equivalent field block in
//! `ffmpeg -bsf:v trace_headers` output on the same source `.bit` file;
//! ffmpeg's trace is the ground truth.
//!
//! NOTE: `tst_core::codec::bitreader` is `pub(crate)` and
//! `validate_bit_depth_minus8` is `pub(crate)` — both unreachable from a
//! `[[bin]]` target (separate crate). This file mirrors `parse_sps` without
//! expanding the production crate's public API: we inline a minimal duplicate
//! of `BitReader` and the `validate_bit_depth_minus8` helper here. The inlined
//! `BitReader` is behaviorally equivalent for the SPS-reading paths exercised
//! by this tracer (it intentionally omits `read_se` and some explanatory
//! comments from the production copy). If either helper is ever promoted to
//! `pub`, replace the inlined copies with `use` imports.

use std::env;
use std::fs;
use std::process::ExitCode;

use tst_core::codec::CodecParseError;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: trace-h265-sps <path-to-sps-rbsp.bin>");
        return ExitCode::from(2);
    }
    let rbsp = match fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {e}", args[1]);
            return ExitCode::from(2);
        }
    };
    match trace(&rbsp) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("parse error: {e:?}");
            ExitCode::from(1)
        }
    }
}

fn trace(rbsp: &[u8]) -> Result<(), CodecParseError> {
    let mut br = BitReader::new(rbsp);

    macro_rules! snap {
        ($name:expr, $val:expr) => {
            println!("{:>5}  {:<48}  {:?}", br.position(), $name, $val);
        };
    }

    let sps_video_parameter_set_id = br.read_u(4)?;
    snap!(
        "sps_video_parameter_set_id (u4)",
        sps_video_parameter_set_id
    );
    let max_sub_layers_minus1 = br.read_u(3)?;
    snap!("max_sub_layers_minus1 (u3)", max_sub_layers_minus1);
    let temporal_id_nesting_flag = br.read_bool()?;
    snap!("temporal_id_nesting_flag (u1)", temporal_id_nesting_flag);

    trace_ptl(&mut br, max_sub_layers_minus1 as u8)?;

    let sps_seq_parameter_set_id = br.read_ue()?;
    snap!("sps_seq_parameter_set_id (ue)", sps_seq_parameter_set_id);
    let chroma_format_idc = br.read_ue()?;
    snap!("chroma_format_idc (ue)", chroma_format_idc);
    if chroma_format_idc == 3 {
        let separate_colour_plane_flag = br.read_bool()?;
        snap!(
            "separate_colour_plane_flag (u1)",
            separate_colour_plane_flag
        );
    }

    let pic_width_in_luma_samples = br.read_ue()?;
    snap!("pic_width_in_luma_samples (ue)", pic_width_in_luma_samples);
    let pic_height_in_luma_samples = br.read_ue()?;
    snap!(
        "pic_height_in_luma_samples (ue)",
        pic_height_in_luma_samples
    );

    let conformance_window_flag = br.read_bool()?;
    snap!("conformance_window_flag (u1)", conformance_window_flag);
    if conformance_window_flag {
        let conf_win_left_offset = br.read_ue()?;
        snap!("conf_win_left_offset (ue)", conf_win_left_offset);
        let conf_win_right_offset = br.read_ue()?;
        snap!("conf_win_right_offset (ue)", conf_win_right_offset);
        let conf_win_top_offset = br.read_ue()?;
        snap!("conf_win_top_offset (ue)", conf_win_top_offset);
        let conf_win_bottom_offset = br.read_ue()?;
        snap!("conf_win_bottom_offset (ue)", conf_win_bottom_offset);
    }

    let bit_depth_luma_minus8 = br.read_ue()?;
    snap!("bit_depth_luma_minus8 (ue)", bit_depth_luma_minus8);
    let _ = validate_bit_depth_minus8("bit_depth_luma_minus8", bit_depth_luma_minus8)?;
    let bit_depth_chroma_minus8 = br.read_ue()?;
    snap!("bit_depth_chroma_minus8 (ue)", bit_depth_chroma_minus8);
    let _ = validate_bit_depth_minus8("bit_depth_chroma_minus8", bit_depth_chroma_minus8)?;

    // Tracer omits production's `> 12` reject (see `parse_sps` in `sps.rs`) —
    // diagnostic should keep tracing past hostile/malformed values to
    // surface where the misalignment lands.
    let log2_max_pic_order_cnt_lsb_minus4 = br.read_ue()?;
    snap!(
        "log2_max_pic_order_cnt_lsb_minus4 (ue)",
        log2_max_pic_order_cnt_lsb_minus4
    );

    let sub_layer_ordering_info_present_flag = br.read_bool()?;
    snap!(
        "sub_layer_ordering_info_present_flag (u1)",
        sub_layer_ordering_info_present_flag
    );
    let layers_to_read = if sub_layer_ordering_info_present_flag {
        max_sub_layers_minus1 as usize + 1
    } else {
        1
    };
    for layer in 0..layers_to_read {
        for which in [
            "max_dec_pic_buffering_minus1",
            "max_num_reorder_pics",
            "max_latency_increase_plus1",
        ] {
            let v = br.read_ue()?;
            snap!(format!("[layer {layer}] {which} (ue)"), v);
        }
    }

    for name in [
        "log2_min_luma_coding_block_size_minus3",
        "log2_diff_max_min_luma_coding_block_size",
        "log2_min_luma_transform_block_size_minus2",
        "log2_diff_max_min_luma_transform_block_size",
        "max_transform_hierarchy_depth_inter",
        "max_transform_hierarchy_depth_intra",
    ] {
        let v = br.read_ue()?;
        snap!(name, v);
    }

    let scaling_list_enabled_flag = br.read_bool()?;
    snap!("scaling_list_enabled_flag (u1)", scaling_list_enabled_flag);
    if scaling_list_enabled_flag {
        let sps_scaling_list_data_present_flag = br.read_bool()?;
        snap!(
            "sps_scaling_list_data_present_flag (u1)",
            sps_scaling_list_data_present_flag
        );
        if sps_scaling_list_data_present_flag {
            eprintln!("trace bailed: scaling_list_data present, production parser bails here");
            return Ok(());
        }
    }

    let amp_enabled_flag = br.read_bool()?;
    snap!("amp_enabled_flag (u1)", amp_enabled_flag);
    let sample_adaptive_offset_enabled_flag = br.read_bool()?;
    snap!(
        "sample_adaptive_offset_enabled_flag (u1)",
        sample_adaptive_offset_enabled_flag
    );
    let pcm_enabled_flag = br.read_bool()?;
    snap!("pcm_enabled_flag (u1)", pcm_enabled_flag);
    if pcm_enabled_flag {
        let pcm_sample_bit_depth_luma_minus1 = br.read_u(4)?;
        snap!(
            "pcm_sample_bit_depth_luma_minus1 (u4)",
            pcm_sample_bit_depth_luma_minus1
        );
        let pcm_sample_bit_depth_chroma_minus1 = br.read_u(4)?;
        snap!(
            "pcm_sample_bit_depth_chroma_minus1 (u4)",
            pcm_sample_bit_depth_chroma_minus1
        );
        let log2_min_pcm_luma_coding_block_size_minus3 = br.read_ue()?;
        snap!(
            "log2_min_pcm_luma_coding_block_size_minus3 (ue)",
            log2_min_pcm_luma_coding_block_size_minus3
        );
        let log2_diff_max_min_pcm_luma_coding_block_size = br.read_ue()?;
        snap!(
            "log2_diff_max_min_pcm_luma_coding_block_size (ue)",
            log2_diff_max_min_pcm_luma_coding_block_size
        );
        let pcm_loop_filter_disabled_flag = br.read_bool()?;
        snap!(
            "pcm_loop_filter_disabled_flag (u1)",
            pcm_loop_filter_disabled_flag
        );
    }

    let num_short_term_ref_pic_sets = br.read_ue()?;
    snap!(
        "num_short_term_ref_pic_sets (ue)",
        num_short_term_ref_pic_sets
    );

    let mut num_delta_pocs: Vec<u32> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for rps_idx in 0..num_short_term_ref_pic_sets {
        let inter = if rps_idx > 0 { br.read_bool()? } else { false };
        snap!(
            format!("[RPS {rps_idx}] inter_ref_pic_set_prediction_flag"),
            inter
        );
        if inter {
            // delta_idx_minus1 is signaled ONLY when stRpsIdx == num_short_term_ref_pic_sets
            // (H.265 §7.3.7), which can only happen in slice-header context. In SPS
            // context (this tracer's only call site), delta_idx_minus1 is inferred
            // to 0, so ref_rps_idx = rps_idx - 1. Matches ffmpeg
            // cbs_h265_syntax_template.c:536-541.
            let delta_rps_sign = br.read_bool()?;
            snap!(format!("[RPS {rps_idx}] delta_rps_sign"), delta_rps_sign);
            let abs_delta_rps_minus1 = br.read_ue()?;
            snap!(
                format!("[RPS {rps_idx}] abs_delta_rps_minus1"),
                abs_delta_rps_minus1
            );
            let ref_rps_idx = rps_idx - 1;
            let num_at_ref = num_delta_pocs[ref_rps_idx as usize];
            let mut new_num_delta = 0u32;
            for j in 0..=num_at_ref {
                let used = br.read_bool()?;
                snap!(format!("[RPS {rps_idx}.{j}] used_by_curr_pic_flag"), used);
                let use_delta = if !used {
                    let u = br.read_bool()?;
                    snap!(format!("[RPS {rps_idx}.{j}] use_delta_flag"), u);
                    u
                } else {
                    true
                };
                if used || use_delta {
                    new_num_delta += 1;
                }
            }
            num_delta_pocs.push(new_num_delta);
        } else {
            // Tracer omits production's `MAX_PICS_PER_SET = 32` cap (see
            // `walk_one_short_term_rps` in `short_term_rps.rs`) — diagnostic intentionally keeps
            // walking past spec-violating values.
            let num_negative = br.read_ue()?;
            snap!(
                format!("[RPS {rps_idx}] num_negative_pics (ue)"),
                num_negative
            );
            let num_positive = br.read_ue()?;
            snap!(
                format!("[RPS {rps_idx}] num_positive_pics (ue)"),
                num_positive
            );
            for j in 0..num_negative {
                let d = br.read_ue()?;
                snap!(
                    format!("[RPS {rps_idx}.s0.{j}] delta_poc_s0_minus1 (ue)"),
                    d
                );
                let u = br.read_bool()?;
                snap!(
                    format!("[RPS {rps_idx}.s0.{j}] used_by_curr_pic_s0_flag"),
                    u
                );
            }
            for j in 0..num_positive {
                let d = br.read_ue()?;
                snap!(
                    format!("[RPS {rps_idx}.s1.{j}] delta_poc_s1_minus1 (ue)"),
                    d
                );
                let u = br.read_bool()?;
                snap!(
                    format!("[RPS {rps_idx}.s1.{j}] used_by_curr_pic_s1_flag"),
                    u
                );
            }
            num_delta_pocs.push(num_negative + num_positive);
        }
    }

    // Stop here: the bug under investigation lives in the RPS region.
    // Production `parse_sps` continues into `long_term_ref_pics_present_flag`,
    // `sps_temporal_mvp_enabled_flag`, and the VUI; extend
    // the tracer if a future bug surfaces past `num_short_term_ref_pic_sets`.
    Ok(())
}

fn trace_ptl(br: &mut BitReader, max_sub_layers_minus1: u8) -> Result<(), CodecParseError> {
    macro_rules! snap {
        ($name:expr, $val:expr) => {
            println!("{:>5}  {:<48}  {:?}", br.position(), $name, $val);
        };
    }
    let general_profile_space = br.read_u(2)?;
    snap!("ptl: general_profile_space (u2)", general_profile_space);
    let general_tier_flag = br.read_bool()?;
    snap!("ptl: general_tier_flag (u1)", general_tier_flag);
    let general_profile_idc = br.read_u(5)?;
    snap!("ptl: general_profile_idc (u5)", general_profile_idc);
    let general_profile_compatibility_flags = br.read_u(32)?;
    snap!(
        "ptl: general_profile_compatibility_flags (u32)",
        format!("0x{:08x}", general_profile_compatibility_flags)
    );
    let general_progressive_source_flag = br.read_bool()?;
    snap!(
        "ptl: general_progressive_source_flag (u1)",
        general_progressive_source_flag
    );
    let general_interlaced_source_flag = br.read_bool()?;
    snap!(
        "ptl: general_interlaced_source_flag (u1)",
        general_interlaced_source_flag
    );
    let general_non_packed_constraint_flag = br.read_bool()?;
    snap!(
        "ptl: general_non_packed_constraint_flag (u1)",
        general_non_packed_constraint_flag
    );
    let general_frame_only_constraint_flag = br.read_bool()?;
    snap!(
        "ptl: general_frame_only_constraint_flag (u1)",
        general_frame_only_constraint_flag
    );
    br.skip(44)?;
    snap!("ptl: <44 reserved-flag bits skipped>", ());
    let general_level_idc = br.read_u(8)?;
    snap!("ptl: general_level_idc (u8)", general_level_idc);

    let mut sub_layer_profile_present = [false; 8];
    let mut sub_layer_level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = br.read_bool()?;
        snap!(
            format!("ptl: sub_layer_profile_present_flag[{i}]"),
            sub_layer_profile_present[i]
        );
        sub_layer_level_present[i] = br.read_bool()?;
        snap!(
            format!("ptl: sub_layer_level_present_flag[{i}]"),
            sub_layer_level_present[i]
        );
    }
    if max_sub_layers_minus1 > 0 {
        for i in max_sub_layers_minus1..8 {
            br.skip(2)?;
            snap!(format!("ptl: reserved_zero_2bits[{i}]"), ());
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] {
            br.skip(2 + 1 + 5 + 32 + 48)?;
            snap!(format!("ptl: <skipped sub_layer_profile[{i}] 88 bits>"), ());
        }
        if sub_layer_level_present[i] {
            br.skip(8)?;
            snap!(format!("ptl: sub_layer_level_idc[{i}]"), ());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Inlined BitReader + validate_bit_depth_minus8.
//
// Both are `pub(crate)` in `tst_core::codec` and so unreachable from a
// `[[bin]]` target (which is a separate crate from the library). The copies
// below are behaviorally equivalent for the SPS-reading paths exercised by
// this tracer — derived from `crates/tst-core/src/codec/bitreader.rs` and
// `crates/tst-core/src/codec/mod.rs::validate_bit_depth_minus8`. They
// intentionally omit `read_se` (unused here) and some inline comments. If
// either helper is ever promoted to `pub`, replace these with `use` imports.
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    bytes: &'a [u8],
    /// Bit position within the input, counting bits skipped over EP bytes
    /// as if they were not there.
    bit_pos: u32,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn position(&self) -> u32 {
        self.bit_pos
    }

    fn byte_at(&self, idx: usize) -> Option<u8> {
        self.bytes.get(idx).copied()
    }

    /// Read `n` bits (n ≤ 32). RBSP reading: if the previous two bytes
    /// were `00 00`, skip a single `03` byte before reading further.
    fn read_u(&mut self, n: u32) -> Result<u32, CodecParseError> {
        if n > 32 {
            return Err(CodecParseError::EngineError(format!("read_u({n}) > 32")));
        }
        let mut acc = 0u32;
        for _ in 0..n {
            acc = (acc << 1) | self.read_one_bit()? as u32;
        }
        Ok(acc)
    }

    fn read_bool(&mut self) -> Result<bool, CodecParseError> {
        Ok(self.read_one_bit()? != 0)
    }

    fn read_one_bit(&mut self) -> Result<u8, CodecParseError> {
        loop {
            let byte_idx = (self.bit_pos / 8) as usize;
            let bit_in_byte = self.bit_pos % 8;
            // EP-byte detection: at the start of a byte, if the prior two
            // bytes are 00 00 and the current byte is 03, skip it.
            if bit_in_byte == 0
                && byte_idx >= 2
                && self.bytes.get(byte_idx) == Some(&0x03)
                && self.bytes.get(byte_idx - 1) == Some(&0x00)
                && self.bytes.get(byte_idx - 2) == Some(&0x00)
            {
                self.bit_pos += 8;
                continue;
            }
            let b = self
                .byte_at(byte_idx)
                .ok_or(CodecParseError::TruncatedRbsp {
                    offset_bits: self.bit_pos,
                    needed_bits: 1,
                })?;
            let bit = (b >> (7 - bit_in_byte)) & 1;
            self.bit_pos += 1;
            return Ok(bit);
        }
    }

    /// Unsigned Exp-Golomb (ue(v)) per H.265 §9.2.2.
    fn read_ue(&mut self) -> Result<u32, CodecParseError> {
        let start = self.bit_pos;
        let mut zeros = 0u32;
        loop {
            if zeros >= 32 {
                return Err(CodecParseError::InvalidGolomb { offset_bits: start });
            }
            let b = self.read_one_bit()?;
            if b == 1 {
                break;
            }
            zeros += 1;
        }
        let suffix = if zeros == 0 { 0 } else { self.read_u(zeros)? };
        Ok((1u32 << zeros).saturating_sub(1).saturating_add(suffix))
    }

    fn skip(&mut self, n: u32) -> Result<(), CodecParseError> {
        for _ in 0..n {
            self.read_one_bit()?;
        }
        Ok(())
    }
}

/// Mirror of `tst_core::codec::validate_bit_depth_minus8` (which is
/// `pub(crate)`). See note above the inlined `BitReader` for rationale.
/// Per the normative syntax range (H.265 §7.4.3.2.1), bit_depth_*_minus8
/// must be in 0..=8; values > 8 indicate a malformed parameter set.
fn validate_bit_depth_minus8(field: &'static str, value: u32) -> Result<u8, CodecParseError> {
    if value > 8 {
        return Err(CodecParseError::ReservedValue { field, value });
    }
    Ok(8 + value as u8)
}
