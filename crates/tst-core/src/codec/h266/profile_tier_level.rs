//! H.266 Profile/Tier/Level parser. Per H.266 V4 §7.3.3.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;

crate::binding_record! {
    /// H.266 profile/tier/level fields parsed from an SPS NAL unit.
    ///
    /// H.266 V4 §7.3.3 PTL carries fewer fields than H.265: only
    /// `general_profile_idc`, `general_tier_flag` and `general_level_idc` are
    /// surfaced here. The SPS exposes the same values through its
    /// `profile_tier_level` member.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct H266ProfileTierLevel {
        /// 7-bit `general_profile_idc` (H.266 V4 §7.3.3). Common values:
        /// 1=Main10, 2=MultilayerMain10 (H.266 V4 Annex A).
        pub general_profile_idc: u8,
        /// `general_tier_flag` — false = Main tier, true = High tier.
        pub general_tier_flag: bool,
        /// `general_level_idc` — H.266 V4 Annex A.4 level table. Encoded as the
        /// level value multiplied by 16: e.g. 64 = Level 4.0, 80 = Level 5.0.
        pub general_level_idc: u8,
    }
}

/// Walk the full PTL syntax via a caller-supplied bit cursor.
///
/// Per H.266 V4 §7.3.3.1. Surfaces only the headline three fields into
/// `out`; everything after (constraint info, sublayer level flags,
/// alignment padding, sub-profile loop) is walked-and-discarded so the
/// cursor lands at the next SPS/VPS field correctly.
///
/// Pre-condition: `out` is already default-initialized; this function
/// overwrites the headline fields when `profile_tier_present_flag` is
/// true and leaves them at their default-zero values otherwise.
pub(crate) fn parse_into(
    br: &mut BitReader<'_>,
    profile_tier_present_flag: bool,
    max_num_sub_layers_minus1: u8,
    out: &mut H266ProfileTierLevel,
) -> Result<(), CodecParseError> {
    if profile_tier_present_flag {
        out.general_profile_idc = br.read_u(7)? as u8;
        out.general_tier_flag = br.read_bool()?;
    }
    out.general_level_idc = br.read_u(8)? as u8;
    let _ptl_frame_only_constraint_flag = br.read_bool()?;
    let _ptl_multilayer_enabled_flag = br.read_bool()?;
    if profile_tier_present_flag {
        // §7.3.3.2 general_constraints_info() — when gci_present_flag=0,
        // the body is just the flag bit; otherwise a long fixed-shape
        // header (~71 bits) plus a variable additional-bits tail. We
        // walk both shapes so the cursor lands correctly on either.
        parse_general_constraints_info(br)?;
    }
    // Sublayer ptl_sublayer_level_present_flag[i] — i runs from
    // MaxNumSubLayersMinus1-1 down to 0, so for max=0 the loop is empty.
    let mut sublayer_present = [false; 7];
    if max_num_sub_layers_minus1 > 0 {
        for i in (0..max_num_sub_layers_minus1).rev() {
            sublayer_present[i as usize] = br.read_bool()?;
        }
    }
    // Byte-align via ptl_reserved_zero_bit u(1) padding.
    while !br.position().is_multiple_of(8) {
        let _ = br.read_bool()?;
    }
    // sublayer_level_idc[i] u(8) for any present sublayer (loop also
    // empty when max_num_sub_layers_minus1 == 0).
    if max_num_sub_layers_minus1 > 0 {
        for i in (0..max_num_sub_layers_minus1).rev() {
            if sublayer_present[i as usize] {
                let _sublayer_level_idc = br.read_u(8)?;
            }
        }
    }
    if profile_tier_present_flag {
        let ptl_num_sub_profiles = br.read_u(8)?;
        for _ in 0..ptl_num_sub_profiles {
            let _general_sub_profile_idc = br.read_u(32)?;
        }
    }
    Ok(())
}

/// §7.3.3.2 general_constraints_info(). Walk-and-discard.
fn parse_general_constraints_info(br: &mut BitReader<'_>) -> Result<(), CodecParseError> {
    let gci_present_flag = br.read_bool()?;
    if gci_present_flag {
        // Fixed-shape body — count by section per H.266 V4 §7.3.3.2 (PDF p.72-73).
        // - general:        3 bits (gci_intra_only / all_layers_independent / one_au_only)
        // - picture format: 6 bits (sixteen_minus_max_bitdepth u(4) + three_minus_max_chroma_format u(2))
        // - NAL related:    10 bits (no_mixed_nalu_types / no_trail / no_stsa / no_rasl / no_radl /
        //                            no_idr / no_cra / no_gdr / no_aps / no_idr_rpl)
        // - tile/slice:     6 bits (one_tile_per_pic / pic_header_in_slice_header /
        //                           one_slice_per_pic / no_rectangular_slice /
        //                           one_slice_per_subpic / no_subpic_info)
        // - CTU/block:      5 bits (three_minus_max_log2_ctu u(2) + three flags)
        // - intra:          6 bits (no_palette / no_ibc / no_isp / no_mrl / no_mip / no_cclm)
        // - inter:          16 bits (no_ref_pic_resampling / no_res_change_in_clvs /
        //                            no_weighted_prediction / no_ref_wraparound / no_temporal_mvp /
        //                            no_sbtmvp / no_amvr / no_bdof / no_smvd / no_dmvr / no_mmvd /
        //                            no_affine_motion / no_prof / no_bcw / no_ciip / no_gpm)
        // - transform/qp:   13 bits
        // - loop filter:    6 bits
        // = 71 fixed bits, then gci_num_additional_bits u(8) = 79 bits.
        br.skip(3)?;
        br.skip(4 + 2)?;
        br.skip(10)?;
        br.skip(6)?;
        br.skip(2 + 3)?;
        br.skip(6)?;
        br.skip(16)?;
        br.skip(13)?;
        br.skip(6)?;
        let gci_num_additional_bits = br.read_u(8)?;
        // First 6 of additional bits are named flags; remaining are
        // reserved bits — both walked the same.
        br.skip(gci_num_additional_bits)?;
    }
    // Byte-align via gci_alignment_zero_bit f(1) padding.
    while !br.position().is_multiple_of(8) {
        let _ = br.read_bool()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one PTL from its own bytes (the SPS parser calls `parse_into`
    /// with its shared cursor instead).
    fn parse_profile_tier_level(
        rbsp: &[u8],
        profile_tier_present_flag: bool,
        max_num_sub_layers_minus1: u8,
    ) -> Result<H266ProfileTierLevel, CodecParseError> {
        let mut br = BitReader::new(rbsp);
        let mut out = H266ProfileTierLevel::default();
        parse_into(
            &mut br,
            profile_tier_present_flag,
            max_num_sub_layers_minus1,
            &mut out,
        )?;
        Ok(out)
    }

    /// Parse a constructed PTL with profile_idc=1 (Main 10), tier=0, level=63 (4.0).
    /// MaxNumSubLayersMinus1=0, gci_present_flag=0, ptl_num_sub_profiles=0.
    /// Layout: profile_idc(7) | tier(1) | level(8) | frame_only(1) | multilayer(1) |
    ///         gci_present(1) | align(5) | num_sub_profiles(8) = 32 bits = 4 bytes.
    #[test]
    fn parse_ptl_main10_at_4_0() {
        // Byte 0: profile_idc=1 (0b0000001) | tier=0 → 0b00000010 = 0x02
        // Byte 1: level=63 → 0x3F
        // Byte 2: frame_only=0 | multilayer=0 | gci_present=0 | 5 align zeros → 0x00
        // Byte 3: num_sub_profiles=0 → 0x00
        let rbsp = vec![0x02, 0x3F, 0x00, 0x00];
        let ptl = parse_profile_tier_level(&rbsp, true, 0).expect("PTL should parse");
        assert_eq!(ptl.general_profile_idc, 1);
        assert!(!ptl.general_tier_flag);
        assert_eq!(ptl.general_level_idc, 63);
    }

    /// Regression test for the GCI fixed-body bit count.
    ///
    /// Per H.266 V4 §7.3.3.2 (PDF p.72-73), the GCI fixed body is 71 bits:
    ///   general 3 + picture-format 6 + NAL-related 10 + tile/slice 6 +
    ///   CTU/block 5 + intra 6 + inter 16 + transform/qp 13 + loop filter 6 = 71.
    /// Prior code read 70 (NAL group 11 instead of 10; inter group 14 instead
    /// of 16). Net cumulative cursor 1 bit too early — masked by all current
    /// tests because every test sets gci_present_flag = 0.
    ///
    /// This test sets gci_present_flag = 1 with a body crafted so that the
    /// spec-correct cursor lands on `ptl_num_sub_profiles = 0` (byte 14, no
    /// loop) while the buggy cursor lands on `ptl_num_sub_profiles = 255`
    /// (byte 13) and overflows the buffer reading 255 × u(32) sub_profile_idc.
    #[test]
    fn parse_ptl_with_gci_present_lands_at_byte_alignment() {
        // Bytes constructed so:
        //   byte 0:   profile_idc=1, tier=0
        //   byte 1:   level_idc=64
        //   byte 2:   frame_only=0 | multilayer=0 | gci_present=1 | 5 zero GCI bits
        //   bytes 3-10: 64 zero GCI body bits
        //   byte 11:  bit 6 = 1 → spec num_additional_bits read at bits 90-97 = 0x08;
        //             buggy num_additional_bits read at bits 89-96 = 0x04
        //   byte 12:  zero (skipped: 8 spec-additional or 4 buggy-additional)
        //   byte 13:  0xFF — buggy reads ptl_num_sub_profiles=255 here; spec doesn't
        //   byte 14:  0x00 — spec reads ptl_num_sub_profiles=0 here
        // Expected: spec returns Ok with num_sub_profiles=0; buggy returns Err
        // (buffer truncation reading sub_profile_idc loop).
        let rbsp = [
            0x02, 0x40, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0xFF,
            0x00,
        ];
        let result = parse_profile_tier_level(&rbsp, true, 0);
        assert!(
            result.is_ok(),
            "GCI-present PTL must parse: cursor must land at byte 14 \
             (spec) not byte 13 (buggy). Got {result:?}",
        );
        let ptl = result.unwrap();
        assert_eq!(ptl.general_profile_idc, 1);
        assert!(!ptl.general_tier_flag);
        assert_eq!(ptl.general_level_idc, 64);
    }
}
