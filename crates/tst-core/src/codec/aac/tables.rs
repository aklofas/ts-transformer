//! ADTS field decode tables.
//!
//! Spec: ISO/IEC 13818-7 Annex 1.A (ADTS framing) + ISO/IEC 14496-3
//! Tables 1.16 (sampling frequency) and 1.19 (channel configuration).

use crate::codec::CodecParseError;

/// Sampling frequency table per ISO 14496-3 Table 1.16.
/// Index 13/14 are reserved; index 15 is "explicit" (not meaningful in ADTS).
const SAMPLING_FREQUENCY: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Decode `sampling_frequency_index` (4 bits) to sample rate (Hz).
/// Errors `ReservedValue` for indices 13/14/15.
pub(crate) fn decode_sample_rate(idx: u8) -> Result<u32, CodecParseError> {
    if idx >= 13 {
        return Err(CodecParseError::ReservedValue {
            field: "sampling_frequency_index",
            value: idx as u32,
        });
    }
    Ok(SAMPLING_FREQUENCY[idx as usize])
}

/// Decode `channel_configuration` (3 bits) to a typed channel layout
/// per ISO 14496-3 Table 1.19.
///
/// `channel_configuration == 0` indicates the channel layout is defined
/// by a Program Config Element (PCE) inside the raw_data_block — a valid
/// AAC streaming shape (used by some encoders to carry 7.1+ or otherwise
/// non-canonical layouts). We don't walk the PCE; the iterator surfaces
/// [`AacChannelLayout::PceDefined`] so callers know "channel count not
/// derivable from the ADTS header alone".
///
/// Indices `1..=7` map to the canonical Table 1.19 channel counts.
/// Index `7` → 8 channels (7.1).
pub(crate) fn decode_channels(
    channel_config: u8,
) -> Result<super::AacChannelLayout, CodecParseError> {
    match channel_config {
        0 => Ok(super::AacChannelLayout::PceDefined),
        1 => Ok(super::AacChannelLayout::Channels(1)),
        2 => Ok(super::AacChannelLayout::Channels(2)),
        3 => Ok(super::AacChannelLayout::Channels(3)),
        4 => Ok(super::AacChannelLayout::Channels(4)),
        5 => Ok(super::AacChannelLayout::Channels(5)),
        6 => Ok(super::AacChannelLayout::Channels(6)),
        7 => Ok(super::AacChannelLayout::Channels(8)),
        _ => Err(CodecParseError::ReservedValue {
            field: "channel_configuration",
            value: channel_config as u32,
        }),
    }
}

/// Decode `profile` (2 bits, ADTS Annex 1.A) to typed profile, gated on
/// the ADTS `ID` bit (MPEG version).
///
/// Per ISO/IEC 13818-7 §1.A Table 8, when `ID == 1` (MPEG-2) the `profile`
/// field is the MPEG-2 audio Profile and value `3` is **reserved**. When
/// `ID == 0` (MPEG-4) the field carries an MPEG-4 audio object type minus
/// one, where value `3` decodes to LongTermPrediction (AOT 4). Accepting
/// `profile == 3` unconditionally would surface a misleading enum value
/// for MPEG-2 ADTS streams.
///
/// # Errors
///
/// Returns [`CodecParseError::ReservedValue`] when `profile == 3` and
/// `mpeg_version == MpegVersion::Mpeg2`.
pub(crate) fn decode_profile(
    profile: u8,
    mpeg_version: super::MpegVersion,
) -> Result<super::AacProfile, CodecParseError> {
    match (profile & 0b11, mpeg_version) {
        (0, _) => Ok(super::AacProfile::Main),
        (1, _) => Ok(super::AacProfile::Lc),
        (2, _) => Ok(super::AacProfile::Ssr),
        (3, super::MpegVersion::Mpeg4) => Ok(super::AacProfile::LongTermPrediction),
        (3, super::MpegVersion::Mpeg2) => Err(CodecParseError::ReservedValue {
            field: "adts_profile",
            value: 3,
        }),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::aac::AacProfile;

    #[test]
    fn sample_rate_44100() {
        assert_eq!(decode_sample_rate(4).unwrap(), 44100);
    }
    #[test]
    fn sample_rate_index_13_reserved() {
        assert!(matches!(
            decode_sample_rate(13).unwrap_err(),
            CodecParseError::ReservedValue {
                field: "sampling_frequency_index",
                value: 13
            }
        ));
    }
    #[test]
    fn channels_stereo_is_2() {
        use crate::codec::aac::AacChannelLayout;
        assert_eq!(decode_channels(2).unwrap(), AacChannelLayout::Channels(2));
    }
    #[test]
    fn channels_7_1_is_8() {
        use crate::codec::aac::AacChannelLayout;
        assert_eq!(decode_channels(7).unwrap(), AacChannelLayout::Channels(8));
    }
    /// `channel_configuration == 0` indicates the channel layout is carried
    /// in a Program Config Element (PCE) inside the raw_data_block, not
    /// derivable from the ADTS header. Per ISO/IEC 14496-3 Table 1.19 this is
    /// a valid streaming shape; returning `ReservedValue` here would terminate
    /// the iterator and drop all subsequent frames.
    #[test]
    fn channels_pce_defined_value_0() {
        use crate::codec::aac::AacChannelLayout;
        assert_eq!(decode_channels(0).unwrap(), AacChannelLayout::PceDefined);
    }
    #[test]
    fn channels_reserved_8_to_15() {
        for v in 8u8..=15 {
            assert!(matches!(
                decode_channels(v).unwrap_err(),
                CodecParseError::ReservedValue {
                    field: "channel_configuration",
                    ..
                }
            ));
        }
    }
    #[test]
    fn profile_lc() {
        use crate::codec::aac::MpegVersion;
        assert_eq!(
            decode_profile(1, MpegVersion::Mpeg4).unwrap(),
            AacProfile::Lc
        );
        assert_eq!(
            decode_profile(1, MpegVersion::Mpeg2).unwrap(),
            AacProfile::Lc
        );
    }

    /// Per ISO/IEC 13818-7 §1.A Table 8, profile=3 is reserved when
    /// ID=1 (MPEG-2). Only valid as LongTermPrediction when ID=0 (MPEG-4).
    #[test]
    fn profile_3_mpeg4_is_long_term_prediction() {
        use crate::codec::aac::MpegVersion;
        assert_eq!(
            decode_profile(3, MpegVersion::Mpeg4).unwrap(),
            AacProfile::LongTermPrediction
        );
    }

    #[test]
    fn profile_3_mpeg2_is_reserved() {
        use crate::codec::aac::MpegVersion;
        assert!(matches!(
            decode_profile(3, MpegVersion::Mpeg2).unwrap_err(),
            CodecParseError::ReservedValue {
                field: "adts_profile",
                value: 3,
            }
        ));
    }
}
