//! Short-term reference picture set walker per H.265 §7.3.7 / §7.4.8.
//!
//! This module advances the [`BitReader`] cursor past one or more
//! `st_ref_pic_set` syntax structures inside the SPS body. It does NOT
//! produce decoded RPS data — that's a decoder's job. The goal is just
//! to keep [`crate::codec::h265::parse_sps`] able to walk to the
//! long-term-ref-pic and VUI fields that live after the RPS region.
//!
//! Mirrors ffmpeg's `ff_hevc_decode_short_term_rps`
//! (`libavcodec/hevc/ps.c:1493-1503` in the audited tree). Tracks
//! `NumDeltaPocs[i]` per RPS index — required for the inter-prediction
//! `inter_ref_pic_set_prediction_flag` arm to know how many flag bits to
//! read for that RPS's reference set.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;
use alloc::vec::Vec;

/// Reasonable upper bound on `num_negative_pics` / `num_positive_pics`
/// per RPS. H.265 §A.4.2 levels cap the total reference picture count;
/// 32 is a safe ceiling for any conforming stream we'd see and bounds
/// the loops against fuzzed input.
const MAX_PICS_PER_SET: u32 = 32;

/// H.265 Table A.8 caps `num_short_term_ref_pic_sets` at 64 for all levels.
/// Anything beyond this is either reserved or crafted hostile input — reject
/// it before allocating to prevent a `Vec::with_capacity` OOM abort on
/// attacker-controlled values near 2^32.
const MAX_SHORT_TERM_RPS: u32 = 64;

/// Walk all `num_short_term_ref_pic_sets` RPSes in this SPS.
pub(crate) fn walk_short_term_ref_pic_sets(
    br: &mut BitReader,
    num_short_term_ref_pic_sets: u32,
) -> Result<(), CodecParseError> {
    if num_short_term_ref_pic_sets > MAX_SHORT_TERM_RPS {
        return Err(CodecParseError::ReservedValue {
            field: "num_short_term_ref_pic_sets",
            value: num_short_term_ref_pic_sets,
        });
    }
    let mut num_delta_pocs: Vec<u32> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for rps_idx in 0..num_short_term_ref_pic_sets {
        walk_one_short_term_rps(br, rps_idx, &mut num_delta_pocs)?;
    }
    Ok(())
}

fn walk_one_short_term_rps(
    br: &mut BitReader,
    rps_idx: u32,
    num_delta_pocs: &mut Vec<u32>,
) -> Result<(), CodecParseError> {
    // Per H.265 §7.3.7: inter_ref_pic_set_prediction_flag is present
    // only when stRpsIdx != 0 (in SPS context — the slice-header context
    // also distinguishes stRpsIdx == num_short_term_ref_pic_sets, but
    // that case never fires inside an SPS RPS walk).
    let inter = if rps_idx > 0 { br.read_bool()? } else { false };

    if inter {
        // delta_idx_minus1 is signaled ONLY when stRpsIdx == num_short_term_ref_pic_sets
        // (H.265 §7.3.7), which can only happen in slice-header context. In SPS
        // context (the only caller of walk_short_term_ref_pic_sets), delta_idx_minus1
        // is inferred to 0, so ref_rps_idx = rps_idx - 1. Matches ffmpeg
        // cbs_h265_syntax_template.c:536-541.
        let _delta_rps_sign = br.read_bool()?;
        let _abs_delta_rps_minus1 = br.read_ue()?;

        let ref_rps_idx = rps_idx - 1;
        let num_at_ref = num_delta_pocs[ref_rps_idx as usize];

        // For j in 0..=NumDeltaPocs[RIdx]:
        //     used_by_curr_pic_flag[j] (1 bit)
        //     if !used_by_curr_pic_flag[j]: use_delta_flag[j] (1 bit)
        // Track NumDeltaPocs[stRpsIdx] = count of (used || use_delta).
        // Per §7.4.8 derivation: a picture from the reference set is
        // copied into the current set iff (used_by_curr_pic_flag[j] ||
        // use_delta_flag[j]) is true.
        let mut new_num_delta = 0u32;
        for _ in 0..=num_at_ref {
            let used = br.read_bool()?;
            let use_delta = if !used { br.read_bool()? } else { true };
            if used || use_delta {
                new_num_delta += 1;
            }
        }
        num_delta_pocs.push(new_num_delta);
    } else {
        // Explicit form: read num_negative_pics + num_positive_pics
        // then per-pic delta_poc + used_by_curr_pic flags.
        let num_negative = br.read_ue()?;
        let num_positive = br.read_ue()?;
        if num_negative > MAX_PICS_PER_SET || num_positive > MAX_PICS_PER_SET {
            return Err(CodecParseError::ReservedValue {
                field: "num_negative_pics_or_num_positive_pics",
                value: num_negative.max(num_positive),
            });
        }
        for _ in 0..num_negative {
            let _delta_poc_s0_minus1 = br.read_ue()?;
            let _used_by_curr_pic_s0_flag = br.read_bool()?;
        }
        for _ in 0..num_positive {
            let _delta_poc_s1_minus1 = br.read_ue()?;
            let _used_by_curr_pic_s1_flag = br.read_bool()?;
        }
        num_delta_pocs.push(num_negative + num_positive);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::bitreader::BitReader;

    #[test]
    fn walk_zero_rps_is_noop() {
        let bytes = [0x00u8];
        let mut br = BitReader::new(&bytes);
        walk_short_term_ref_pic_sets(&mut br, 0).expect("zero RPSes is a no-op");
    }

    /// Regression for the bug surfaced by JCT-VC conformance vector
    /// DBLK_A_MAIN10_VIXS_4: the SPS-context RPS walker used to
    /// unconditionally read `delta_idx_minus1` inside the inter arm, but
    /// per H.265 §7.3.7 it is only signaled when stRpsIdx ==
    /// num_short_term_ref_pic_sets (slice-header context). In SPS context
    /// the field is inferred to 0; reading it consumed bits that belonged
    /// to delta_rps_sign + abs_delta_rps_minus1, throwing the cursor off
    /// for the rest of the walk.
    #[test]
    fn inter_rps_in_sps_context_does_not_consume_delta_idx_minus1() {
        // Bits encoded left-to-right, MSB-first:
        // RPS 0 (explicit form, inter=false because rps_idx==0):
        //   num_negative_pics    ue=1 → "010"  (3 bits)
        //   num_positive_pics    ue=0 → "1"    (1 bit)
        //   delta_poc_s0_minus1  ue=0 → "1"    (1 bit)
        //   used_by_curr_pic_s0  bool=1 → "1"  (1 bit)
        //   ⇒ num_delta_pocs[0] = 1
        // RPS 1 (inter form):
        //   inter_ref_pic_set_prediction_flag bool=1 → "1"  (1 bit)
        //   delta_rps_sign       bool=0 → "0"  (1 bit)
        //   abs_delta_rps_minus1 ue=0 → "1"    (1 bit)
        //   (loop j in 0..=num_delta_pocs[0]=1 → 2 iterations)
        //   used_by_curr_pic_flag[0] bool=1 → "1"  (1 bit)
        //   used_by_curr_pic_flag[1] bool=1 → "1"  (1 bit)
        // Total = 11 bits: 0 1 0 1 1 1 1 0 1 1 1 (then 5 don't-care padding bits)
        // Byte 0: 0101_1110 = 0x5E
        // Byte 1: 1110_0000 = 0xE0
        //
        // The buggy pre-fix code interpreted bits 8.. as starting a ue(v)
        // for delta_idx_minus1: leading_zero "0", sentinel "1", then 1 more
        // bit "1" → code_num=3 → delta_idx_minus1=2 → 2+1 > rps_idx(1) →
        // returned ReservedValue { field: "delta_idx_minus1", value: 2 }.
        // Verified to fail pre-fix and pass post-fix.
        let bytes = [0x5Eu8, 0xE0];
        let mut br = BitReader::new(&bytes);
        walk_short_term_ref_pic_sets(&mut br, 2)
            .expect("two-RPS SPS walk should succeed in SPS context");
        assert_eq!(br.position(), 11);
    }

    #[test]
    fn rejects_unrealistic_num_pics_per_set() {
        // num_negative=33 — exceeds MAX_PICS_PER_SET. Encode ue(33):
        // 33 + 1 = 34 = 0b100010; leading zeros = 5; total 11 bits:
        // 00000 1 00010.
        let bytes = encode_ue_to_bytes(33);
        // Also need ue(0) for num_positive_pics. Append 0x80 (ue=0).
        let mut full = bytes;
        full.push(0x80);
        let mut br = BitReader::new(&full);
        let err = walk_short_term_ref_pic_sets(&mut br, 1).unwrap_err();
        assert!(matches!(err, CodecParseError::ReservedValue { .. }));
    }

    fn encode_ue_to_bytes(v: u32) -> Vec<u8> {
        let code_num = v + 1;
        let leading_zeros = 31 - code_num.leading_zeros();
        let total_bits = (leading_zeros * 2 + 1) as usize;
        let mut bits: Vec<u8> = Vec::with_capacity(total_bits);
        bits.extend(core::iter::repeat_n(0, leading_zeros as usize));
        for i in (0..=leading_zeros).rev() {
            bits.push(((code_num >> i) & 1) as u8);
        }
        let byte_count = total_bits.div_ceil(8);
        let mut bytes = vec![0u8; byte_count];
        for (i, b) in bits.iter().enumerate() {
            if *b == 1 {
                bytes[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        bytes
    }
}
