//! KLV carried in a video `user_data_unregistered` SEI message, so each KLV
//! record travels inside the access unit it belongs to.
//!
//! One SEI NAL unit carries one record. Its RBSP is a single SEI message:
//!
//! | Field | Bytes | Value |
//! |---|---|---|
//! | `payload_type` | 1 | `0x05` (`user_data_unregistered`) |
//! | `payload_size` | 1 + n | `0xFF` per full 255, then the remainder (H.264 §7.3.2.3.1) |
//! | UUID | 16 | [`TST_KLV_SEI_UUID`] |
//! | version | 1 | [`SEI_VERSION`] |
//! | KLV PTS | 8 | 90 kHz PTS of the KLV record, big-endian |
//! | KLV | rest | the KLV local set bytes as pushed |
//! | `rbsp_trailing_bits` | 1 | `0x80` |
//!
//! The RBSP is emulation-prevention escaped and follows the codec's SEI NAL
//! header: H.264 `0x06` (type 6, `nal_ref_idc` 0) or H.265 `[0x4E, 0x01]`
//! (PREFIX_SEI type 39, layer 0, `nuh_temporal_id_plus1` 1).

use tst_core::codec::annexb::nals;
use tst_core::codec::misp_time::{MispTimeError, build_user_data_unregistered_sei_nal};
use tst_core::codec::nal::{header_len, nal_unit_type};

use crate::VideoCodec;

/// The `uuid_iso_iec_11578` that marks a tst KLV SEI: the ASCII bytes
/// `tstKLV`, then `00 01`, then eight fixed random bytes.
pub const TST_KLV_SEI_UUID: [u8; 16] = [
    0x74, 0x73, 0x74, 0x4b, 0x4c, 0x56, 0x00, 0x01, 0x8c, 0xa0, 0x4e, 0x0d, 0x9b, 0x2f, 0x11, 0xe9,
];

/// The version byte that follows the UUID. A SEI with any other version is
/// not parsed.
pub const SEI_VERSION: u8 = 1;

/// UUID (16) + version (1) + PTS (8): the bytes before the KLV record.
const PREFIX_LEN: usize = 16 + 1 + 8;

/// SEI `payload_type` of `user_data_unregistered`.
const USER_DATA_UNREGISTERED: usize = 5;

/// H.265 PREFIX_SEI `nal_unit_type`.
const H265_PREFIX_SEI: u8 = 39;

/// H.264 SEI `nal_unit_type`.
const H264_SEI: u8 = 6;

/// Build the SEI NAL unit that carries one KLV record (layout in the
/// [module docs](self)).
///
/// Returns the bare NAL: header plus escaped RBSP, with no Annex-B start
/// code.
///
/// # Errors
///
/// [`MispTimeError::UnsupportedCodec`] for H.266 and AV1. A validated
/// [`MountConfig`](crate::MountConfig) only holds H.264 or H.265.
pub fn build_klv_sei_nal(
    codec: VideoCodec,
    klv_pts_90k: u64,
    klv: &[u8],
) -> Result<Vec<u8>, MispTimeError> {
    // The UUID is the builder's first field; this payload is what follows it.
    let mut payload = Vec::with_capacity(PREFIX_LEN - 16 + klv.len());
    payload.push(SEI_VERSION);
    payload.extend_from_slice(&klv_pts_90k.to_be_bytes());
    payload.extend_from_slice(klv);
    build_user_data_unregistered_sei_nal(codec, &TST_KLV_SEI_UUID, &payload)
}

/// One KLV record taken from a tst KLV SEI.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct KlvSei {
    /// 90 kHz PTS of the KLV record (the 8-byte big-endian field after the
    /// version byte).
    pub klv_pts_90k: u64,
    /// The KLV bytes: everything after the PTS up to the end of the SEI
    /// payload.
    pub klv: Vec<u8>,
}

/// Every tst KLV SEI in an Annex-B access unit, in order.
///
/// Only SEI NAL units are considered: H.264 type 6 and H.265 PREFIX_SEI
/// type 39. Foreign SEIs (another UUID, another payload type, another
/// version) and malformed candidates are skipped, never errors. Only the
/// first SEI message of a NAL unit is read; the builder writes one message
/// per NAL. H.266 and AV1 yield nothing.
pub fn extract_klv_seis(au: &[u8], codec: VideoCodec) -> Vec<KlvSei> {
    let Some(hdr) = header_len(codec) else {
        return Vec::new();
    };
    let sei_type = match codec {
        VideoCodec::H264 => H264_SEI,
        VideoCodec::H265 => H265_PREFIX_SEI,
        VideoCodec::H266 | VideoCodec::Av1 => return Vec::new(),
    };
    nals(au)
        .filter(|nal| nal.len() > hdr && nal_unit_type(codec, nal) == Some(sei_type))
        .filter_map(|nal| parse_klv_sei_rbsp(&nal[hdr..]))
        .collect()
}

/// Parse one SEI NAL body (the NAL header stripped, emulation prevention
/// still in place) and return its KLV record if it is a tst KLV SEI.
///
/// Reads the first SEI message: `payload_type` must be 5, the payload must
/// hold at least the 25-byte prefix and start with [`TST_KLV_SEI_UUID`] and
/// [`SEI_VERSION`]. `None` for anything else, including a body cut short;
/// never panics.
pub fn parse_klv_sei_rbsp(rbsp_escaped: &[u8]) -> Option<KlvSei> {
    let r = unescape(rbsp_escaped);
    let mut i = 0;
    if read_ff_coded(&r, &mut i)? != USER_DATA_UNREGISTERED {
        return None;
    }
    let size = read_ff_coded(&r, &mut i)?;
    let body = r.get(i..i.checked_add(size)?)?;
    if body.len() < PREFIX_LEN || body[..16] != TST_KLV_SEI_UUID || body[16] != SEI_VERSION {
        return None;
    }
    let pts = u64::from_be_bytes(body[17..PREFIX_LEN].try_into().ok()?);
    Some(KlvSei {
        klv_pts_90k: pts,
        klv: body[PREFIX_LEN..].to_vec(),
    })
}

/// Read an SEI `payload_type` or `payload_size`: a `0xFF` byte per full 255,
/// then the final byte. `None` when the buffer ends first.
fn read_ff_coded(r: &[u8], i: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    loop {
        let b = *r.get(*i)?;
        *i += 1;
        value = value.checked_add(usize::from(b))?;
        if b != 0xFF {
            return Some(value);
        }
    }
}

/// Undo emulation prevention: drop each `0x03` that follows two zero bytes.
fn unescape(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut zeros = 0u32;
    for &b in src {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 { zeros += 1 } else { zeros = 0 }
    }
    out
}
