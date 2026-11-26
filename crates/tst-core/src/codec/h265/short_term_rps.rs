//! Short-term reference picture set walker per H.265 §7.3.7 / §7.4.8.
//!
//! This module advances the [`BitReader`] cursor past one or more
//! `st_ref_pic_set` syntax structures inside the SPS body. It does NOT
//! produce decoded RPS data — that's a decoder's job. The goal is just
//! to keep [`crate::codec::h265::parse_sps`] able to walk to the
//! long-term-ref-pic and VUI fields that live after the RPS region.
//!
//! Mirrors ffmpeg's `ff_hevc_decode_short_term_rps`
//! (`libavcodec/hevc/ps.c:1493-1503` in the audited tree). Keeps
//! `DeltaPocS0` / `DeltaPocS1` per RPS index: a predicted RPS
//! (`inter_ref_pic_set_prediction_flag`) reads one flag pair per entry of its
//! reference plus one, and holds only the entries equations (7-61) / (7-62)
//! give it (none where the predicted delta is zero), which a later predicted
//! RPS counts in turn.

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
    walk_sets(br, num_short_term_ref_pic_sets).map(|_| ())
}

/// `DeltaPocS0` then `DeltaPocS1` of one RPS (H.265 §7.4.8), in the
/// standard's index order: flag `j` of a later predicted RPS refers to them.
///
/// No cap beyond the explicit form's: a predicted RPS holds at most one entry
/// more than its reference and an SPS holds at most 64 RPS, so a list holds at
/// most 127 entries and an SPS's sets at most 6 112 in all. A delta is a sum of
/// at most 64 terms below 2^32: `i64` cannot overflow.
#[derive(Debug, PartialEq, Eq)]
struct RpsDeltas {
    s0: Vec<i64>,
    s1: Vec<i64>,
}

fn walk_sets(
    br: &mut BitReader,
    num_short_term_ref_pic_sets: u32,
) -> Result<Vec<RpsDeltas>, CodecParseError> {
    if num_short_term_ref_pic_sets > MAX_SHORT_TERM_RPS {
        return Err(CodecParseError::ReservedValue {
            field: "num_short_term_ref_pic_sets",
            value: num_short_term_ref_pic_sets,
        });
    }
    let mut sets: Vec<RpsDeltas> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for rps_idx in 0..num_short_term_ref_pic_sets {
        let set = walk_one_short_term_rps(br, rps_idx, &sets)?;
        sets.push(set);
    }
    Ok(sets)
}

fn walk_one_short_term_rps(
    br: &mut BitReader,
    rps_idx: u32,
    sets: &[RpsDeltas],
) -> Result<RpsDeltas, CodecParseError> {
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
        let delta_rps_sign = br.read_bool()?;
        let abs_delta_rps_minus1 = br.read_ue()?;
        let delta_rps = if delta_rps_sign { -1 } else { 1 } * (i64::from(abs_delta_rps_minus1) + 1);

        let r = &sets[(rps_idx - 1) as usize];
        let n_neg = r.s0.len();
        let n_ref = n_neg + r.s1.len();
        // For j in 0..=NumDeltaPocs[RefRpsIdx]: used_by_curr_pic_flag[j],
        // then use_delta_flag[j] when the first is 0 (inferred 1 otherwise).
        // Entry j < n_neg is DeltaPocS0[j], then DeltaPocS1, and the last one
        // is the reference picture itself.
        let mut use_delta = Vec::with_capacity(n_ref + 1);
        for _ in 0..=n_ref {
            let used = br.read_bool()?;
            use_delta.push(used || br.read_bool()?);
        }
        // Equations (7-61) and (7-62): an entry is added only under its
        // use_delta_flag and only where the predicted delta is not zero.
        let mut s0 = Vec::new();
        for (j, &d) in r.s1.iter().enumerate().rev() {
            let d = d + delta_rps;
            if d < 0 && use_delta[n_neg + j] {
                s0.push(d);
            }
        }
        if delta_rps < 0 && use_delta[n_ref] {
            s0.push(delta_rps);
        }
        for (j, &d) in r.s0.iter().enumerate() {
            let d = d + delta_rps;
            if d < 0 && use_delta[j] {
                s0.push(d);
            }
        }
        let mut s1 = Vec::new();
        for (j, &d) in r.s0.iter().enumerate().rev() {
            let d = d + delta_rps;
            if d > 0 && use_delta[j] {
                s1.push(d);
            }
        }
        if delta_rps > 0 && use_delta[n_ref] {
            s1.push(delta_rps);
        }
        for (j, &d) in r.s1.iter().enumerate() {
            let d = d + delta_rps;
            if d > 0 && use_delta[n_neg + j] {
                s1.push(d);
            }
        }
        Ok(RpsDeltas { s0, s1 })
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
        // (7-67) to (7-70): each delta is the previous one moved by
        // delta_poc_sX_minus1 + 1, away from the current picture.
        let mut s0 = Vec::with_capacity(num_negative as usize);
        let mut poc = 0i64;
        for _ in 0..num_negative {
            poc -= i64::from(br.read_ue()?) + 1; // delta_poc_s0_minus1
            let _used_by_curr_pic_s0_flag = br.read_bool()?;
            s0.push(poc);
        }
        let mut s1 = Vec::with_capacity(num_positive as usize);
        let mut poc = 0i64;
        for _ in 0..num_positive {
            poc += i64::from(br.read_ue()?) + 1; // delta_poc_s1_minus1
            let _used_by_curr_pic_s1_flag = br.read_bool()?;
            s1.push(poc);
        }
        Ok(RpsDeltas { s0, s1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::bitreader::BitReader;

    /// RPS0 = {-1}; RPS1 predicted with deltaRps +1 keeps only +1 (the
    /// candidate at 0 is no entry, equations (7-61) / (7-62)), so RPS2 reads
    /// two flag pairs. Bits: 010111 10111 10111, then the sentinel `10`.
    #[test]
    fn inter_rps_drops_the_zero_delta_entry() {
        let bytes = [0x5E, 0xF7, 0x80];
        let mut br = BitReader::new(&bytes);
        walk_short_term_ref_pic_sets(&mut br, 3).unwrap();
        assert!(br.read_bool().unwrap(), "sentinel bit 1 untouched");
        assert!(!br.read_bool().unwrap(), "sentinel bit 2 untouched");
    }

    /// An independent statement of (7-61) / (7-62): every candidate (the
    /// reference's S0 entries, its S1 entries, then the reference picture
    /// itself at 0) moved by `delta_rps`, kept where its flag is set and it is
    /// not zero; negatives nearest first, positives nearest first.
    fn model_predict(r: &RpsDeltas, delta_rps: i64, use_delta: &[bool]) -> RpsDeltas {
        let kept: Vec<i64> =
            r.s0.iter()
                .chain(&r.s1)
                .chain(&[0])
                .zip(use_delta)
                .filter(|&(&d, &u)| u && d + delta_rps != 0)
                .map(|(&d, _)| d + delta_rps)
                .collect();
        let mut s0: Vec<i64> = kept.iter().copied().filter(|&d| d < 0).collect();
        s0.sort_unstable_by(|a, b| b.cmp(a));
        let mut s1: Vec<i64> = kept.iter().copied().filter(|&d| d > 0).collect();
        s1.sort_unstable();
        RpsDeltas { s0, s1 }
    }

    /// One predicted set as written: `delta_rps_sign`, `abs_delta_rps_minus1`,
    /// then `(used_by_curr_pic_flag, use_delta_flag)` per candidate.
    type Predicted = (bool, u32, Vec<(bool, bool)>);

    /// Writes an explicit RPS 0 (`delta_poc_sX_minus1` per entry) and
    /// `n_predicted` sets after it, each from `next(entries of its reference)`;
    /// returns the bytes (a `10` sentinel after the last set) and the model's
    /// lists.
    fn chain(
        neg: &[u32],
        pos: &[u32],
        n_predicted: usize,
        mut next: impl FnMut(usize) -> Predicted,
    ) -> (Vec<u8>, Vec<RpsDeltas>) {
        use crate::codec::test_util::BitWriter;
        let mut bw = BitWriter::new();
        bw.write_ue(neg.len() as u32);
        bw.write_ue(pos.len() as u32);
        let (mut s0, mut poc) = (Vec::new(), 0i64);
        for &m in neg {
            bw.write_ue(m);
            bw.write(1, 1); // used_by_curr_pic_s0_flag
            poc -= i64::from(m) + 1;
            s0.push(poc);
        }
        let (mut s1, mut poc) = (Vec::new(), 0i64);
        for &m in pos {
            bw.write_ue(m);
            bw.write(1, 1); // used_by_curr_pic_s1_flag
            poc += i64::from(m) + 1;
            s1.push(poc);
        }
        let mut model = vec![RpsDeltas { s0, s1 }];
        for _ in 0..n_predicted {
            let r = model.last().unwrap();
            let (sign, abs_minus1, flags) = next(r.s0.len() + r.s1.len());
            assert_eq!(flags.len(), r.s0.len() + r.s1.len() + 1);
            bw.write(1, 1); // inter_ref_pic_set_prediction_flag
            bw.write(u64::from(sign), 1);
            bw.write_ue(abs_minus1);
            for &(used, use_delta) in &flags {
                bw.write(u64::from(used), 1);
                if !used {
                    bw.write(u64::from(use_delta), 1);
                }
            }
            let use_delta: Vec<bool> = flags.iter().map(|&(u, d)| u || d).collect();
            let delta_rps = if sign { -1 } else { 1 } * (i64::from(abs_minus1) + 1);
            let predicted = model_predict(r, delta_rps, &use_delta);
            model.push(predicted);
        }
        bw.write(1, 1);
        bw.write(0, 1);
        bw.end_rbsp();
        (bw.bytes, model)
    }

    fn walk_and_check(bytes: &[u8], model: &[RpsDeltas]) {
        let mut br = BitReader::new(bytes);
        let sets = walk_sets(&mut br, model.len() as u32).unwrap();
        assert_eq!(sets.len(), model.len());
        for (i, (got, want)) in sets.iter().zip(model).enumerate() {
            assert_eq!(got, want, "RPS {i}");
        }
        assert!(br.read_bool().unwrap(), "sentinel bit 1");
        assert!(!br.read_bool().unwrap(), "sentinel bit 2");
    }

    /// A fixed-seed xorshift: the chains below are the same on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// The walker's lists equal the model's, set by set: on the vector above,
    /// on fixed-seed chains of 64 sets with small deltas (so that candidates
    /// land on 0) and random flags, and on the largest chain an SPS can hold.
    #[test]
    fn inter_rps_lists_match_the_equations() {
        let (bytes, model) = chain(&[0], &[], 2, |n| (false, 0, vec![(true, true); n + 1]));
        assert_eq!(bytes[..2], [0x5E, 0xF7]);
        assert_eq!(
            model[1],
            RpsDeltas {
                s0: vec![],
                s1: vec![1]
            }
        );
        assert_eq!(
            model[2],
            RpsDeltas {
                s0: vec![],
                s1: vec![1, 2]
            }
        );
        walk_and_check(&bytes, &model);

        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..200 {
            let neg: Vec<u32> = (0..rng.below(5)).map(|_| rng.below(3) as u32).collect();
            let pos: Vec<u32> = (0..rng.below(5)).map(|_| rng.below(3) as u32).collect();
            let (bytes, model) = chain(&neg, &pos, 63, |n| {
                let sign = rng.below(2) == 0;
                let abs_minus1 = rng.below(3) as u32;
                let flags = (0..=n)
                    .map(|_| (rng.below(2) == 0, rng.below(4) != 0))
                    .collect();
                (sign, abs_minus1, flags)
            });
            walk_and_check(&bytes, &model);
        }

        // 32 + 32 entries, then 63 sets that move every candidate by -100 and
        // keep each one: set i holds 64 + i entries, all but the first set's
        // in S0.
        let (bytes, model) = chain(&[0; 32], &[0; 32], 63, |n| {
            (true, 99, vec![(true, true); n + 1])
        });
        walk_and_check(&bytes, &model);
        let peak = model.iter().map(|r| r.s0.len().max(r.s1.len())).max();
        let total: usize = model.iter().map(|r| r.s0.len() + r.s1.len()).sum();
        assert_eq!((peak, total), (Some(127), 6112));
    }

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
