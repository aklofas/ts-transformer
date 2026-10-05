#![no_main]
//! Fuzz target — H.264 slice header light parser, no-SPS and SPS-context paths.
//!
//! Exercises both the `None` SPS path and the `Some(&sps)` path, where
//! `frame_num` is read as a `u(v)` field with bit-width
//! `log2_max_frame_num_minus4 + 4` (∈ 4..=16) derived from the SPS.
//!
//! # Input layout
//!
//! ```text
//! [0]     selector byte:
//!           bit 7 = 0 → no-SPS path
//!           bit 7 = 1 → SPS-context path;
//!                        log2_max_frame_num_minus4 = sel % 13  (→ 0..=12,
//!                        folding the full byte — not just low bits)
//!           bit 0     → nal_unit_type: 0=non-IDR (type 1), 1=IDR (type 5)
//! [1..]   slice RBSP bytes (fuzz-driven)
//! ```
//!
//! // TODO(h265): H.265 parse_slice_header_light has the same
//! // log2_max_pic_order_cnt_lsb_minus4 SPS-context gap; deferred —
//! // H.265 SPS synthesis needs profile_tier_level + sub-layer arrays.

use libfuzzer_sys::fuzz_target;
use tst_core::codec::h264::{parse_slice_header_light, parse_sps};

// ---------------------------------------------------------------------------
// Minimal bit-writer for synthesising H.264 SPS RBSP bytes.
// ---------------------------------------------------------------------------

fn push_u8_bits(bits: &mut Vec<bool>, val: u8) {
    for i in (0..8u32).rev() {
        bits.push((val >> i) & 1 == 1);
    }
}

/// Exp-Golomb ue(v) per H.264 §9.1:
///   INFO = v + 1,  M = floor(log2(INFO))
///   code = M leading-zeros + '1' + M-bit suffix (INFO - 2^M)
fn push_ue_bits(bits: &mut Vec<bool>, val: u32) {
    let info = val + 1;
    let m = 31 - info.leading_zeros(); // floor(log2(info))
    for _ in 0..m {
        bits.push(false); // leading zeros
    }
    bits.push(true); // marker
    for i in (0..m).rev() {
        bits.push((info >> i) & 1 == 1); // suffix
    }
}

fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|chunk| {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                if bit {
                    b |= 0x80 >> i;
                }
            }
            b
        })
        .collect()
}

/// Build a minimal H.264 Baseline SPS RBSP for a given
/// `log2_max_frame_num_minus4` ∈ 0..=12. The output always passes
/// `parse_sps` — Baseline (profile_idc=66) has no chroma-info block,
/// and all other fields are set to their smallest valid values.
fn minimal_sps_rbsp(log2: u8) -> Vec<u8> {
    let mut b: Vec<bool> = Vec::new();
    push_u8_bits(&mut b, 66); // profile_idc = 66 (Baseline)
    push_u8_bits(&mut b, 0xC0); // constraint_set0 + constraint_set1 flags
    push_u8_bits(&mut b, 30); // level_idc = 30 (level 3.0)
    push_ue_bits(&mut b, 0); // seq_parameter_set_id = 0
    // Baseline not in high-profile family → no chroma_format / bit_depth block.
    push_ue_bits(&mut b, u32::from(log2)); // log2_max_frame_num_minus4
    push_ue_bits(&mut b, 0); // pic_order_cnt_type = 0
    push_ue_bits(&mut b, 0); // log2_max_pic_order_cnt_lsb_minus4 = 0
    push_ue_bits(&mut b, 1); // max_num_ref_frames = 1
    b.push(false); // gaps_in_frame_num_value_allowed_flag = 0
    push_ue_bits(&mut b, 7); // pic_width_in_mbs_minus1 = 7 (→ 128 px)
    push_ue_bits(&mut b, 7); // pic_height_in_map_units_minus1 = 7 (→ 128 px)
    b.push(true); // frame_mbs_only_flag = 1
    b.push(false); // direct_8x8_inference_flag = 0
    b.push(false); // frame_cropping_flag = 0
    b.push(false); // vui_parameters_present_flag = 0
    bits_to_bytes(&b)
}

// ---------------------------------------------------------------------------

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let sel = data[0];
    let rbsp = &data[1..];
    // Bit 0 of selector drives nal_unit_type to cover both IDR and non-IDR.
    let nal_type: u8 = if sel & 1 == 0 { 1 } else { 5 };

    if sel & 0x80 == 0 {
        // No-SPS path — exercises every bit-cursor branch without bit-width
        // SPS dependency (original coverage; most adversarial input shape).
        let _ = parse_slice_header_light(rbsp, None, nal_type);
    } else {
        // SPS-context path — exercises the frame_num read_u(bits) call with
        // fuzz-chosen bit-width bits = log2 + 4 ∈ 4..=16.
        // `sel % 13` folds the full selector byte (0..=255) to 0..=12.
        let log2 = sel % 13;
        let sps_rbsp = minimal_sps_rbsp(log2);
        // Safety net: if parse_sps rejects the synthesized bytes the if-let
        // below silently falls through — every exec is crash-free but the
        // Some(&sps) branch is never taken (coverage theater).
        // cargo-fuzz builds enable debug-assertions even in release mode, so
        // this fires on the very first exec if synthesis breaks.
        debug_assert!(
            parse_sps(&sps_rbsp).is_ok(),
            "minimal_sps_rbsp({log2}) rejected by parse_sps — SPS-context branch is dead"
        );
        if let Ok(sps) = parse_sps(&sps_rbsp) {
            let _ = parse_slice_header_light(rbsp, Some(&sps), nal_type);
        }
    }
});
