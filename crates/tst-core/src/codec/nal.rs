//! NAL unit header fields for H.264, H.265 and H.266: the one table the
//! muxer's Annex-B checks, the MISP SEI splice, the parameter-set filter,
//! the slice-header parsers and the RTP packetizers share. `no_std`.
//!
//! Every function takes a NAL unit that starts with its header and has no
//! Annex-B start code (what [`crate::codec::annexb::nals`] yields).

use crate::codec::VideoCodec;

/// Length of the NAL unit header in bytes: 1 for H.264 (§7.3.1), 2 for
/// H.265 (§7.3.1.2) and H.266 (§7.3.1.2). `None` for AV1, which has no
/// NAL units.
pub const fn header_len(codec: VideoCodec) -> Option<usize> {
    match codec {
        VideoCodec::H264 => Some(1),
        VideoCodec::H265 | VideoCodec::H266 => Some(2),
        VideoCodec::Av1 => None,
    }
}

/// `nal_unit_type` of `nal`: H.264 `byte0 & 0x1F`, H.265
/// `(byte0 >> 1) & 0x3F`, H.266 `(byte1 >> 3) & 0x1F`.
///
/// Only the byte that holds the type must be present (a one-byte H.265
/// NAL still has a type); check `nal.len()` against [`header_len`] when
/// the whole header matters. `None` for AV1 or when that byte is missing.
pub fn nal_unit_type(codec: VideoCodec, nal: &[u8]) -> Option<u8> {
    match codec {
        VideoCodec::H264 => nal.first().map(|b| b & 0x1F),
        VideoCodec::H265 => nal.first().map(|b| (b >> 1) & 0x3F),
        VideoCodec::H266 => nal.get(1).map(|b| (b >> 3) & 0x1F),
        VideoCodec::Av1 => None,
    }
}

/// `true` for a VCL (coded slice data) NAL type: H.264 1–5 (§7.4.1),
/// H.265 0–31 (§7.4.2.2), H.266 0–11 (Table 5). `false` for AV1.
pub const fn is_vcl(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => matches!(nal_type, 1..=5),
        VideoCodec::H265 => nal_type <= 31,
        VideoCodec::H266 => nal_type <= 11,
        VideoCodec::Av1 => false,
    }
}

/// `true` for the NAL type of an IDR picture: H.264 5, H.265 19
/// (IDR_W_RADL) and 20 (IDR_N_LP), H.266 7 (IDR_W_RADL) and 8 (IDR_N_LP).
/// `false` for AV1.
pub const fn is_idr(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 5,
        VideoCodec::H265 => matches!(nal_type, 19 | 20),
        VideoCodec::H266 => matches!(nal_type, 7 | 8),
        VideoCodec::Av1 => false,
    }
}
