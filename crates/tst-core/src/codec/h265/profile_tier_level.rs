//! `profile_tier_level()` parser per H.265 §7.3.3.
//!
//! Decoded fields used by VPS/SPS callers:
//! - `general_profile_space` (2 bits)
//! - `general_tier_flag` (1 bit)
//! - `general_profile_idc` (5 bits)
//! - `general_profile_compatibility_flags` (32 bits)
//! - `general_progressive_source_flag` (1 bit)
//! - `general_interlaced_source_flag` (1 bit)
//! - `general_non_packed_constraint_flag` (1 bit)
//! - `general_frame_only_constraint_flag` (1 bit)
//! - `general_level_idc` (8 bits)
//!
//! Per-sub-layer fields (when `maxNumSubLayersMinus1 > 0`) are skipped.

use crate::codec::CodecParseError;
use crate::codec::bitreader::BitReader;

crate::binding_record! {
    /// H.265 profile/tier/level fields parsed from a VPS or SPS NAL unit.
    ///
    /// These fields are decoded from the `profile_tier_level()` syntax structure
    /// at H.265 §7.3.3. The SPS and the VPS expose the same values through their
    /// `profile_tier_level` member, which returns this object; the fields are
    /// also available directly on both. `general_profile_space` is always 0 when
    /// reconstructed from an SPS or VPS, because they do not store that field
    /// separately (it is always 0 for all ITU-T registered profiles).
    #[must_use]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    #[non_exhaustive]
    pub struct H265ProfileTierLevel {
        /// 2-bit `general_profile_space` (§7.3.3). In practice always 0 for ITU-T
        /// registered profiles.
        pub general_profile_space: u8,
        /// `general_tier_flag` (§7.4.4): true = High tier, false = Main tier.
        pub general_tier_flag: bool,
        /// 5-bit `general_profile_idc` (§7.3.3). Common values: 1=Main, 2=Main10,
        /// 4=Rext, 5=HEVC-HM, 6=Multiview-Main, 7=Scalable-Main.
        pub general_profile_idc: u8,
        /// 32-bit `general_profile_compatibility_flags` (§7.3.3). Bit `i` set
        /// means the stream conforms to profile `i`. MSB-first: spec-bit j lives
        /// at `flags & (1 << (31 - j))`. Many real Main10 streams have
        /// `general_profile_idc=1` (Main) but bit 2 (= 1 << 29) set; ffmpeg uses
        /// it to disambiguate Main vs Main10 vs Main10-Intra.
        pub general_profile_compatibility_flags: u32,
        /// `general_progressive_source_flag` (§7.4.4): the stream is progressive.
        pub general_progressive_source_flag: bool,
        /// `general_interlaced_source_flag` (§7.4.4): the stream is interlaced.
        pub general_interlaced_source_flag: bool,
        /// `general_non_packed_constraint_flag` (§7.4.4): no frame-packing
        /// arrangement SEI in the bitstream.
        pub general_non_packed_constraint_flag: bool,
        /// `general_frame_only_constraint_flag` (§7.4.4): stream contains only
        /// frames (no field pictures).
        pub general_frame_only_constraint_flag: bool,
        /// `general_level_idc` (§7.3.3). Level encoded as `30 * level_major + 3 *
        /// level_minor` for levels up to 6.2: e.g. 120 = Level 4.0, 150 = Level
        /// 5.0, 180 = Level 6.0.
        pub general_level_idc: u8,
    }
}

pub(crate) fn parse(
    br: &mut BitReader<'_>,
    max_num_sub_layers_minus1: u8,
) -> Result<H265ProfileTierLevel, CodecParseError> {
    let general_profile_space = br.read_u(2)? as u8;
    let general_tier_flag = br.read_bool()?;
    let general_profile_idc = br.read_u(5)? as u8;
    let general_profile_compatibility_flags = br.read_u(32)?;
    // §7.4.4: the 48 bits after `general_profile_compatibility_flags` start
    // with four source/constraint flags, followed by 43 profile-specific
    // constraint bits + `general_inbld_flag`. We surface the four leading
    // flags (consumers need them to detect interlaced / progressive / frame-
    // only sources independently of the profile_idc) and skip the rest.
    let general_progressive_source_flag = br.read_bool()?;
    let general_interlaced_source_flag = br.read_bool()?;
    let general_non_packed_constraint_flag = br.read_bool()?;
    let general_frame_only_constraint_flag = br.read_bool()?;
    br.skip(44)?;
    let general_level_idc = br.read_u(8)? as u8;

    // Per sub-layer: read sub_layer_profile_present_flag and sub_layer_level_present_flag
    // for each layer i in [0..max_num_sub_layers_minus1).
    let mut sub_layer_profile_present = [false; 8];
    let mut sub_layer_level_present = [false; 8];
    for i in 0..max_num_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = br.read_bool()?;
        sub_layer_level_present[i] = br.read_bool()?;
    }
    if max_num_sub_layers_minus1 > 0 {
        // Reserved zero bits to byte-align (2 bits per missing sub-layer
        // up to 8 sub-layers total).
        for _ in max_num_sub_layers_minus1..8 {
            br.skip(2)?;
        }
    }
    // Skip per-sub-layer profile/level info we don't expose.
    for i in 0..max_num_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] {
            br.skip(2 + 1 + 5 + 32 + 48)?;
        }
        if sub_layer_level_present[i] {
            br.skip(8)?;
        }
    }

    Ok(H265ProfileTierLevel {
        general_profile_space,
        general_tier_flag,
        general_profile_idc,
        general_profile_compatibility_flags,
        general_progressive_source_flag,
        general_interlaced_source_flag,
        general_non_packed_constraint_flag,
        general_frame_only_constraint_flag,
        general_level_idc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_no_sub_layers() {
        // Byte 0: profile_space=0b00, tier_flag=1, profile_idc=0b00001 → 0b0010_0001
        let buf: [u8; 12] = [0b0010_0001, 0x60, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 120];
        let mut br = BitReader::new(&buf);
        let ptl = parse(&mut br, 0).unwrap();
        assert!(ptl.general_tier_flag);
        assert_eq!(ptl.general_profile_idc, 1);
        assert_eq!(ptl.general_profile_compatibility_flags, 0x60000000);
        assert_eq!(ptl.general_level_idc, 120);
        // All four source/constraint flags are zero in this synthetic buffer.
        assert!(!ptl.general_progressive_source_flag);
        assert!(!ptl.general_interlaced_source_flag);
        assert!(!ptl.general_non_packed_constraint_flag);
        assert!(!ptl.general_frame_only_constraint_flag);
    }

    #[test]
    fn parse_surfaces_source_constraint_flags() {
        // Byte 0: profile_space=0, tier=0, profile_idc=1 → 0b0000_0001.
        // Bytes 1-4: profile_compatibility_flags = 0x60000000 (Main + Main10).
        // Byte 5: top nibble 0b1011 → progressive=1, interlaced=0,
        // non_packed=1, frame_only=1; remaining bits zero.
        // Bytes 5..11: 48 bits of source/constraint/reserved space.
        // Byte 11: general_level_idc = 150.
        let buf: [u8; 12] = [
            0b0000_0001,
            0x60,
            0x00,
            0x00,
            0x00,
            0b1011_0000,
            0,
            0,
            0,
            0,
            0,
            150,
        ];
        let mut br = BitReader::new(&buf);
        let ptl = parse(&mut br, 0).unwrap();
        assert_eq!(ptl.general_profile_idc, 1);
        assert_eq!(ptl.general_profile_compatibility_flags, 0x60000000);
        assert!(ptl.general_progressive_source_flag);
        assert!(!ptl.general_interlaced_source_flag);
        assert!(ptl.general_non_packed_constraint_flag);
        assert!(ptl.general_frame_only_constraint_flag);
        assert_eq!(ptl.general_level_idc, 150);
    }
}
