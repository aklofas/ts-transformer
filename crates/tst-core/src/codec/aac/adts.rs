//! ADTS bit-field decode helpers (private).
//!
//! Spec: ISO/IEC 13818-7 §1.A Tables 6–7.

use super::tables::{decode_channels, decode_profile, decode_sample_rate};
use super::{AacChannelLayout, AacProfile, MpegVersion};
use crate::codec::CodecParseError;

/// Decoded view of the 7- or 9-byte ADTS header (no body slice yet).
#[derive(Debug)]
pub(super) struct Header {
    pub profile: AacProfile,
    pub mpeg_version: MpegVersion,
    pub sample_rate_hz: u32,
    pub channel_configuration: u8,
    pub channel_layout: AacChannelLayout,
    pub frame_length_bytes: u32,
    pub samples_per_frame: u16,
    pub num_raw_data_blocks: u8,
    pub has_crc: bool,
    pub raw_header_len: usize,
}

/// Decode a 7- or 9-byte ADTS header.
///
/// Field layout (MSB first), 7 bytes total (9 with CRC):
///   syncword:                12 bits (must be 0xFFF)
///   ID (mpeg_version):        1 bit  (0 = MPEG-4, 1 = MPEG-2)
///   layer:                    2 bits (must be 0b00)
///   protection_absent:        1 bit  (0 = CRC follows, 1 = no CRC)
///   profile:                  2 bits
///   sampling_frequency_index: 4 bits
///   private_bit:              1 bit
///   channel_configuration:    3 bits
///   original_copy:            1 bit
///   home:                     1 bit
///   copyright_id_bit:         1 bit
///   copyright_id_start:       1 bit
///   aac_frame_length:         13 bits (header + body bytes total)
///   adts_buffer_fullness:     11 bits
///   number_of_raw_data_blocks_in_frame: 2 bits (wire 0..=3 → logical 1..=4)
pub(super) fn parse_header(bytes: &[u8]) -> Result<Header, CodecParseError> {
    if bytes.len() < 7 {
        return Err(CodecParseError::Truncated {
            needed: 7,
            had: bytes.len() as u32,
        });
    }

    // Sync word: 12 bits = 0xFFF
    let sync = ((bytes[0] as u16) << 4) | (((bytes[1] as u16) >> 4) & 0x0F);
    if sync != 0xFFF {
        return Err(CodecParseError::BadSyncWord {
            expected: 0xFFF,
            found: sync,
        });
    }

    // bytes[1] low nibble: ID(1) layer(2) protection_absent(1)
    let id_bit = (bytes[1] >> 3) & 1;
    let mpeg_version = if id_bit == 0 {
        MpegVersion::Mpeg4
    } else {
        MpegVersion::Mpeg2
    };

    let layer = (bytes[1] >> 1) & 0b11;
    if layer != 0 {
        return Err(CodecParseError::Forbidden {
            field: "adts_layer",
        });
    }

    let protection_absent = bytes[1] & 1;
    let has_crc = protection_absent == 0;

    // bytes[2]: profile(2) sample_rate_index(4) private(1) channel_config(MSB 1)
    //
    // Per ISO/IEC 13818-7 §1.A Table 8, `profile == 3` is reserved when
    // ID=1 (MPEG-2) and only valid as LongTermPrediction when ID=0
    // (MPEG-4). `decode_profile` handles the gating; reserved values
    // surface as `CodecParseError::ReservedValue`.
    let profile_bits = (bytes[2] >> 6) & 0b11;
    let profile = decode_profile(profile_bits, mpeg_version)?;

    let sample_rate_index = (bytes[2] >> 2) & 0b1111;
    let sample_rate_hz = decode_sample_rate(sample_rate_index)?;

    // channel_config spans bytes[2] bit0 + bytes[3] bits 7-6
    let channel_configuration = ((bytes[2] & 1) << 2) | ((bytes[3] >> 6) & 0b11);
    let channel_layout = decode_channels(channel_configuration)?;

    // bytes[3] bits 5-2: original/home/copyright bits (don't care)
    // bytes[3] bits 1-0 + bytes[4] + bytes[5] high bits: aac_frame_length (13 bits)
    let aac_frame_length: u32 = (((bytes[3] & 0b11) as u32) << 11)
        | ((bytes[4] as u32) << 3)
        | (((bytes[5] >> 5) & 0b111) as u32);

    // bytes[5] low 5 + bytes[6] high 6: adts_buffer_fullness (11 bits) — don't care
    // bytes[6] low 2: number_of_raw_data_blocks_in_frame (wire 0..=3)
    let num_blocks_wire = bytes[6] & 0b11;
    let num_raw_data_blocks = num_blocks_wire + 1;
    let samples_per_frame = 1024u16 * num_raw_data_blocks as u16;

    let raw_header_len: usize = if has_crc { 9 } else { 7 };

    if has_crc && bytes.len() < 9 {
        return Err(CodecParseError::Truncated {
            needed: 9,
            had: bytes.len() as u32,
        });
    }

    if aac_frame_length < raw_header_len as u32 {
        return Err(CodecParseError::Truncated {
            needed: raw_header_len as u32,
            had: aac_frame_length,
        });
    }

    Ok(Header {
        profile,
        mpeg_version,
        sample_rate_hz,
        channel_configuration,
        channel_layout,
        frame_length_bytes: aac_frame_length,
        samples_per_frame,
        num_raw_data_blocks,
        has_crc,
        raw_header_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a 7-byte ADTS header with MPEG-2 ID bit.
    /// Defaults: MPEG-2 ID, no CRC, AAC-LC profile, 44.1 kHz, stereo.
    fn build_header(
        profile: u8,               // 2 bits
        sample_rate_index: u8,     // 4 bits
        channel_configuration: u8, // 3 bits
        aac_frame_length: u32,     // 13 bits
        num_blocks_wire: u8,       // 2 bits
        protection_absent: bool,   // true = no CRC
    ) -> Vec<u8> {
        build_header_with_id(
            1, // MPEG-2
            profile,
            sample_rate_index,
            channel_configuration,
            aac_frame_length,
            num_blocks_wire,
            protection_absent,
        )
    }

    /// Helper: build a 7-byte ADTS header with explicit `ID` bit
    /// (`0` = MPEG-4, `1` = MPEG-2). Used by the profile tests that need to
    /// exercise the MPEG version gating in `decode_profile`.
    fn build_header_with_id(
        id_bit: u8,                // 1 bit (0 = MPEG-4, 1 = MPEG-2)
        profile: u8,               // 2 bits
        sample_rate_index: u8,     // 4 bits
        channel_configuration: u8, // 3 bits
        aac_frame_length: u32,     // 13 bits
        num_blocks_wire: u8,       // 2 bits
        protection_absent: bool,   // true = no CRC
    ) -> Vec<u8> {
        let mut h = vec![0u8; 7];
        // bytes[0] + bytes[1] high nibble: 0xFFF sync
        h[0] = 0xFF;
        // bytes[1]: 1111 (sync low) | ID(1) | 00 (layer) | protection_absent
        let pa = if protection_absent { 1 } else { 0 };
        h[1] = 0b1111_0000 | ((id_bit & 1) << 3) | pa;
        // bytes[2]: profile(2) | sample_rate_idx(4) | private(0) | chan_cfg MSB
        h[2] =
            (profile << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_configuration >> 2) & 1);
        // bytes[3]: chan_cfg low 2 | original(0) | home(0) | copyright(0,0) | frame_length high 2
        h[3] = ((channel_configuration & 0b11) << 6) | (((aac_frame_length >> 11) & 0b11) as u8);
        // bytes[4]: frame_length middle 8
        h[4] = ((aac_frame_length >> 3) & 0xFF) as u8;
        // bytes[5]: frame_length low 3 | adts_buffer_fullness high 5
        h[5] = (((aac_frame_length & 0b111) as u8) << 5) | 0b1_1111;
        // bytes[6]: adts_buffer_fullness low 6 | num_blocks_wire low 2
        h[6] = (0b11_1111 << 2) | (num_blocks_wire & 0b11);
        h
    }

    #[test]
    fn parse_header_lc_44100_stereo_no_crc() {
        let bytes = build_header(1, 4, 2, 7 + 100, 0, true);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.profile, AacProfile::Lc);
        assert_eq!(h.sample_rate_hz, 44100);
        assert_eq!(h.channel_configuration, 2);
        assert_eq!(h.channel_layout, AacChannelLayout::Channels(2));
        assert_eq!(h.frame_length_bytes, 107);
        assert_eq!(h.num_raw_data_blocks, 1);
        assert_eq!(h.samples_per_frame, 1024);
        assert!(!h.has_crc);
        assert_eq!(h.raw_header_len, 7);
    }

    /// `channel_configuration == 0` is a valid AAC streaming shape:
    /// channel layout is carried in a Program Config Element (PCE) inside
    /// the raw_data_block. The header must parse successfully and surface
    /// `AacChannelLayout::PceDefined`.
    #[test]
    fn parse_header_pce_defined_channel_configuration_0() {
        let bytes = build_header(1, 4, 0, 7 + 100, 0, true);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.channel_configuration, 0);
        assert_eq!(h.channel_layout, AacChannelLayout::PceDefined);
    }

    #[test]
    fn parse_header_layer_nonzero_is_forbidden() {
        let mut bytes = build_header(1, 4, 2, 100, 0, true);
        bytes[1] |= 0b0000_0010; // set layer bit
        assert!(matches!(
            parse_header(&bytes).unwrap_err(),
            CodecParseError::Forbidden {
                field: "adts_layer"
            }
        ));
    }

    #[test]
    fn parse_header_short_buffer_truncated() {
        assert!(matches!(
            parse_header(&[0xFF; 4]).unwrap_err(),
            CodecParseError::Truncated { needed: 7, had: 4 }
        ));
    }

    #[test]
    fn parse_header_bad_sync() {
        assert!(matches!(
            parse_header(&[0xAB, 0xCD, 0, 0, 0, 0, 0]).unwrap_err(),
            CodecParseError::BadSyncWord { .. }
        ));
    }

    #[test]
    fn parse_header_with_crc_short_yields_truncated() {
        let mut bytes = build_header(1, 4, 2, 7 + 100, 0, false);
        bytes.truncate(7); // protection_absent=false but only 7 bytes
        assert!(matches!(
            parse_header(&bytes).unwrap_err(),
            CodecParseError::Truncated { needed: 9, had: 7 }
        ));
    }

    #[test]
    fn parse_header_frame_length_lt_header_yields_truncated() {
        let bytes = build_header(1, 4, 2, 5, 0, true); // length 5 < 7
        assert!(matches!(
            parse_header(&bytes).unwrap_err(),
            CodecParseError::Truncated { needed: 7, had: 5 }
        ));
    }

    #[test]
    fn parse_header_multi_block_4_yields_4096_samples() {
        let bytes = build_header(1, 4, 2, 7 + 100, 3, true); // wire 3 → logical 4 blocks
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.num_raw_data_blocks, 4);
        assert_eq!(h.samples_per_frame, 4096);
    }

    /// When ID=0 (MPEG-4) the ADTS `profile` field carries an MPEG-4
    /// audio object type minus one; value `3` decodes to LongTermPrediction
    /// (AOT 4) and the header parses successfully.
    #[test]
    fn parse_header_profile_3_mpeg4_is_long_term_prediction() {
        let bytes = build_header_with_id(0, 3, 4, 2, 7 + 100, 0, true);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.mpeg_version, MpegVersion::Mpeg4);
        assert_eq!(h.profile, AacProfile::LongTermPrediction);
    }

    /// When ID=1 (MPEG-2) profile=3 is reserved per ISO/IEC 13818-7
    /// §1.A Table 8 and must surface as a typed parse error rather than
    /// the misleading `LongTermPrediction` MPEG-4 enum value.
    #[test]
    fn parse_header_profile_3_mpeg2_is_reserved() {
        let bytes = build_header_with_id(1, 3, 4, 2, 7 + 100, 0, true);
        assert!(matches!(
            parse_header(&bytes).unwrap_err(),
            CodecParseError::ReservedValue {
                field: "adts_profile",
                value: 3,
            }
        ));
    }

    /// Profiles `0..=2` (Main / LC / SSR) are valid under both ID
    /// values; verify the MPEG-2 path still decodes them correctly.
    #[test]
    fn parse_header_profile_0_1_2_mpeg2_decode() {
        for (profile_bits, expected) in [
            (0u8, AacProfile::Main),
            (1, AacProfile::Lc),
            (2, AacProfile::Ssr),
        ] {
            let bytes = build_header_with_id(1, profile_bits, 4, 2, 7 + 100, 0, true);
            let h = parse_header(&bytes).unwrap();
            assert_eq!(h.mpeg_version, MpegVersion::Mpeg2);
            assert_eq!(h.profile, expected, "profile_bits={profile_bits}");
        }
    }
}
