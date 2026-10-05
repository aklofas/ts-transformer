//! MISB ST 0604 MISP timestamps for compressed Motion Imagery.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Builds and extracts the ST 0604.6 Precision / Nano Precision Time
//! Stamp carried in an H.264 / H.265 `user_data_unregistered` SEI
//! message (§7, §11.1, §12.1/§12.2). The 28-byte payload is a 16-byte
//! identifier, a 1-byte MISB ST 0603 Time Status, and an 11-byte
//! "Modified" timestamp (8-byte big-endian value with a `0xFF` guard
//! byte after each 2-byte group, §7.4 Table 2).
//!
//! Out of scope (see `docs/project/deferred-features.md`):
//! H.262/MPEG-2 `user_data` carriage (§10), the Commercial Time Stamp
//! (`pic_timing` / `time_code` SEI, §11.2/§12.3), and AV1 / H.266
//! (ST 0604 defines no carriage for them).

use crate::mpegts::mux::VideoCodec;
use alloc::vec::Vec;

/// ST 0604.6 §7.1 Table 1 — H.262/H.264 Precision Time Stamp Identifier.
pub const MISP_MICROSEC_ID_H264: [u8; 16] = *b"MISPmicrosectime";
/// ST 0604.6 §7.2 — H.265 Precision (microsecond) Time Stamp Identifier.
pub const MISP_MICROSEC_ID_H265: [u8; 16] = [
    0xa8, 0x68, 0x7d, 0xd4, 0xd7, 0x59, 0x37, 0x58, 0xa5, 0xce, 0xf0, 0x33, 0x8b, 0x65, 0x45, 0xf1,
];
/// ST 0604.6 §8.1 — H.265 Nano Precision Time Stamp Identifier.
pub const MISP_NANOSEC_ID_H265: [u8; 16] = [
    0xcf, 0x84, 0x82, 0x78, 0xee, 0x23, 0x30, 0x6c, 0x92, 0x65, 0xe8, 0xfe, 0xf2, 0x2f, 0xb8, 0xb8,
];

/// Which MISP time base a [`MispTimestamp`] carries.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MispTimeKind {
    /// Microseconds since the MISP epoch (ST 0603 Precision Time Stamp).
    Micro,
    /// Nanoseconds since the MISP epoch (ST 0603 Nano Precision Time
    /// Stamp). H.265-only per ST 0604.6 §12.2.
    Nano,
}

/// One MISP timestamp destined for (or extracted from) a video SEI.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MispTimestamp {
    pub kind: MispTimeKind,
    /// MISB ST 0603 Time Status byte (see [`crate::klv::st0605`] for the
    /// same byte in Class 0 packs).
    pub time_status: u8,
    /// Micro: microseconds since the MISP epoch. Nano: nanoseconds.
    pub value: u64,
}

impl MispTimestamp {
    /// Microsecond-precision timestamp (valid for H.264 and H.265).
    pub fn micros(value_us: u64, time_status: u8) -> Self {
        Self {
            kind: MispTimeKind::Micro,
            time_status,
            value: value_us,
        }
    }

    /// Nanosecond-precision timestamp (H.265-only per ST 0604.6 §12.2).
    pub fn nanos(value_ns: u64, time_status: u8) -> Self {
        Self {
            kind: MispTimeKind::Nano,
            time_status,
            value: value_ns,
        }
    }
}

/// Why a MISP SEI could not be built or spliced.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MispTimeError {
    /// The Nano Precision Time Stamp is defined for H.265 only
    /// (ST 0604.6 §12.2); H.264 carries the microsecond form.
    #[error("nano-precision MISP timestamp is H.265-only (ST 0604.6 §12.2), not {codec:?}")]
    NanoUnsupportedForCodec { codec: VideoCodec },
    /// ST 0604 defines SEI timestamp carriage for H.264 and H.265 only.
    #[error("ST 0604 defines no MISP SEI carriage for {codec:?}")]
    UnsupportedCodec { codec: VideoCodec },
    /// The access unit contains no VCL NAL to anchor the SEI in front of.
    #[error("access unit contains no VCL NAL unit to place the MISP SEI before")]
    NoVclNal,
}

/// Why a present MISP SEI payload failed to parse (absence is `Ok(None)`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MispTimeExtractError {
    /// A matched MISP SEI message was shorter than the mandatory 28 bytes.
    #[error("MISP SEI payload truncated (need 28 bytes)")]
    TruncatedSei,
    /// One of the ST 0604.6 §7.4 `0xFF` guard bytes was absent.
    #[error("MISP SEI modified-timestamp guard byte is not 0xFF")]
    BadGuardByte,
}

/// The 16-byte `uuid_iso_iec_11578` identifier for a codec + kind combo.
pub fn identifier_for(
    codec: VideoCodec,
    kind: MispTimeKind,
) -> Result<&'static [u8; 16], MispTimeError> {
    match (codec, kind) {
        (VideoCodec::H264, MispTimeKind::Micro) => Ok(&MISP_MICROSEC_ID_H264),
        (VideoCodec::H264, MispTimeKind::Nano) => {
            Err(MispTimeError::NanoUnsupportedForCodec { codec })
        }
        (VideoCodec::H265, MispTimeKind::Micro) => Ok(&MISP_MICROSEC_ID_H265),
        (VideoCodec::H265, MispTimeKind::Nano) => Ok(&MISP_NANOSEC_ID_H265),
        (VideoCodec::H266 | VideoCodec::Av1, _) => Err(MispTimeError::UnsupportedCodec { codec }),
    }
}

/// Assemble the 28-byte ST 0604.6 SEI payload (Table 2): identifier,
/// Time Status, then the 8-byte big-endian value as four 2-byte groups
/// with a `0xFF` guard byte after each of the first three groups.
pub(crate) fn sei_payload(
    codec: VideoCodec,
    ts: &MispTimestamp,
) -> Result<[u8; 28], MispTimeError> {
    let id = identifier_for(codec, ts.kind)?;
    let v = ts.value.to_be_bytes();
    let mut out = [0u8; 28];
    out[..16].copy_from_slice(id);
    out[16] = ts.time_status;
    out[17] = v[0];
    out[18] = v[1];
    out[19] = 0xFF;
    out[20] = v[2];
    out[21] = v[3];
    out[22] = 0xFF;
    out[23] = v[4];
    out[24] = v[5];
    out[25] = 0xFF;
    out[26] = v[6];
    out[27] = v[7];
    Ok(out)
}

/// Append `rbsp` to `out` with `emulation_prevention_three_byte`
/// insertion (H.264 §7.4.1 / H.265 §7.4.2): any byte value 0x00–0x03
/// that would follow two consecutive zero bytes gets a 0x03 inserted
/// before it. `out`'s existing tail does not count toward the zero run
/// (callers append the NAL header first — headers are non-zero here).
pub(crate) fn append_rbsp_escaped(out: &mut Vec<u8>, rbsp: &[u8]) {
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        if b == 0 { zeros += 1 } else { zeros = 0 }
    }
}

/// Build the complete MISP-timestamp SEI NAL for one access unit.
///
/// Returns the bare NAL bytes (header + escaped RBSP, NO Annex-B start
/// code — the splice site supplies a 3-byte `00 00 01` prefix). H.264
/// gets `nal_unit_type=6` (`nal_ref_idc=0`); H.265 gets PREFIX_SEI
/// (type 39, `nuh_layer_id=0`, `nuh_temporal_id_plus1=1`). The SEI
/// message is `user_data_unregistered` (payloadType 5) with the fixed
/// 28-byte ST 0604 payload.
pub fn build_sei_nal(codec: VideoCodec, ts: &MispTimestamp) -> Result<Vec<u8>, MispTimeError> {
    let payload = sei_payload(codec, ts)?;
    // RBSP = payload_type(5) + payload_size(28) + payload + trailing 0x80.
    let mut rbsp = [0u8; 31];
    rbsp[0] = 0x05;
    rbsp[1] = 28;
    rbsp[2..30].copy_from_slice(&payload);
    rbsp[30] = 0x80;
    let mut out = Vec::with_capacity(2 + rbsp.len() + 4);
    match codec {
        VideoCodec::H264 => out.push(0x06),
        VideoCodec::H265 => out.extend_from_slice(&[0x4E, 0x01]),
        // sei_payload() above already rejected H.266 / AV1.
        VideoCodec::H266 | VideoCodec::Av1 => unreachable!("rejected by sei_payload"),
    }
    append_rbsp_escaped(&mut out, &rbsp);
    Ok(out)
}

pub(crate) fn insert_sei_before_first_vcl(
    au: &[u8],
    sei_nal: &[u8],
    codec: VideoCodec,
) -> Result<Vec<u8>, MispTimeError> {
    let at = first_vcl_prefix_offset(au, codec).ok_or(MispTimeError::NoVclNal)?;
    let mut out = Vec::with_capacity(au.len() + 3 + sei_nal.len());
    out.extend_from_slice(&au[..at]);
    out.extend_from_slice(&[0, 0, 1]);
    out.extend_from_slice(sei_nal);
    out.extend_from_slice(&au[at..]);
    Ok(out)
}

/// Extract the first MISP timestamp SEI from an Annex-B access unit.
///
/// `Ok(None)` = no MISP SEI present. `Err` = a MISP identifier matched
/// but its payload is malformed (the distinction conformance checkers
/// need). Liberal on input: all three ST 0604 identifiers are matched
/// on both codecs; SEI NALs anywhere in the AU are scanned (prefix AND
/// suffix positions); non-MISP SEI content is skipped, even if broken.
///
/// # C ABI
///
/// `tst_misp_time_extract` — see `bindings/c/include/tstrans.h`.
pub fn extract(
    au: &[u8],
    codec: VideoCodec,
) -> Result<Option<MispTimestamp>, MispTimeExtractError> {
    let mut i = 0usize;
    while i + 3 <= au.len() {
        if !(au[i] == 0 && au[i + 1] == 0) {
            i += 1;
            continue;
        }
        let data_at = if au[i + 2] == 1 {
            i + 3
        } else if i + 4 <= au.len() && au[i + 2] == 0 && au[i + 3] == 1 {
            i + 4
        } else {
            i += 1;
            continue;
        };
        // Find this NAL's end (next start code or EOF).
        let mut end = data_at;
        while end + 3 <= au.len() {
            if au[end] == 0
                && au[end + 1] == 0
                && (au[end + 2] == 1
                    || (end + 4 <= au.len() && au[end + 2] == 0 && au[end + 3] == 1))
            {
                break;
            }
            end += 1;
        }
        if end + 3 > au.len() {
            end = au.len();
        }
        let nal = &au[data_at..end];
        if let Some(found) = scan_sei_nal(nal, codec)? {
            return Ok(Some(found));
        }
        i = end;
    }
    Ok(None)
}

/// If `nal` is an SEI NAL for `codec`, walk its messages for a MISP
/// user_data_unregistered payload. Non-SEI NALs and non-MISP messages
/// yield `Ok(None)`.
fn scan_sei_nal(
    nal: &[u8],
    codec: VideoCodec,
) -> Result<Option<MispTimestamp>, MispTimeExtractError> {
    let (is_sei, header_len) = match codec {
        VideoCodec::H264 => (!nal.is_empty() && nal[0] & 0x1F == 6, 1),
        VideoCodec::H265 => (nal.len() >= 2 && matches!((nal[0] >> 1) & 0x3F, 39 | 40), 2),
        VideoCodec::H266 | VideoCodec::Av1 => (false, 0),
    };
    if !is_sei {
        return Ok(None);
    }
    // Strip emulation-prevention bytes from the RBSP.
    let mut rbsp = Vec::with_capacity(nal.len() - header_len);
    let mut zeros = 0u32;
    for &b in &nal[header_len..] {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue; // the escape byte itself is dropped
        }
        rbsp.push(b);
        if b == 0 { zeros += 1 } else { zeros = 0 }
    }
    // Walk SEI messages: ff-accumulated payload_type, then payload_size.
    let mut p = 0usize;
    loop {
        let mut payload_type = 0usize;
        loop {
            let Some(&b) = rbsp.get(p) else {
                return Ok(None);
            };
            p += 1;
            payload_type = payload_type.saturating_add(b as usize);
            if b != 0xFF {
                break;
            }
        }
        let mut payload_size = 0usize;
        loop {
            let Some(&b) = rbsp.get(p) else {
                return Ok(None);
            };
            p += 1;
            payload_size = payload_size.saturating_add(b as usize);
            if b != 0xFF {
                break;
            }
        }
        // Compute the end index with overflow protection: an enormous
        // payload_size from a saturating-accumulated 0xFF chain must not
        // wrap p + payload_size before .get() can return None.
        let payload_end = p.checked_add(payload_size).unwrap_or(usize::MAX);
        let Some(payload) = rbsp.get(p..payload_end) else {
            // The declared payload_size runs past the RBSP end. Check
            // whether this truncated message is a MISP one: if
            // payload_type == 5 AND at least 16 bytes remain AND those
            // bytes match a known MISP identifier, the caller needs to
            // know the SEI was present but malformed. Fewer than 16
            // available bytes means the identifier is unconfirmable,
            // so we fall through to Ok(None) (no confirmed MISP match).
            if payload_type == 5 {
                let id_end = p.checked_add(16).unwrap_or(usize::MAX);
                if let Some(head) = rbsp.get(p..id_end) {
                    if head == MISP_MICROSEC_ID_H264
                        || head == MISP_MICROSEC_ID_H265
                        || head == MISP_NANOSEC_ID_H265
                    {
                        return Err(MispTimeExtractError::TruncatedSei);
                    }
                }
            }
            return Ok(None);
        };
        if payload_type == 5 && payload_size >= 16 {
            let kind = if payload[..16] == MISP_MICROSEC_ID_H264
                || payload[..16] == MISP_MICROSEC_ID_H265
            {
                Some(MispTimeKind::Micro)
            } else if payload[..16] == MISP_NANOSEC_ID_H265 {
                Some(MispTimeKind::Nano)
            } else {
                None
            };
            if let Some(kind) = kind {
                if payload_size < 28 {
                    return Err(MispTimeExtractError::TruncatedSei);
                }
                if payload[19] != 0xFF || payload[22] != 0xFF || payload[25] != 0xFF {
                    return Err(MispTimeExtractError::BadGuardByte);
                }
                let v = [
                    payload[17],
                    payload[18],
                    payload[20],
                    payload[21],
                    payload[23],
                    payload[24],
                    payload[26],
                    payload[27],
                ];
                return Ok(Some(MispTimestamp {
                    kind,
                    time_status: payload[16],
                    value: u64::from_be_bytes(v),
                }));
            }
        }
        p = payload_end; // payload_end is p + payload_size; overflow already handled above
        // rbsp_trailing_bits: a 0x80 (or padding) terminates the walk.
        if rbsp.get(p).is_none_or(|&b| b == 0x80) {
            return Ok(None);
        }
    }
}

/// Byte offset of the START-CODE PREFIX of the first VCL NAL in an
/// Annex-B AU, or `None` when no VCL NAL is present. VCL = H.264
/// `nal_unit_type` 1..=5 (§7.4.1); H.265 `nal_unit_type` 0..=31
/// (§7.4.2.2, all VCL types are < 32). Only H.264/H.265 reach this —
/// build_sei_nal() has already rejected other codecs.
fn first_vcl_prefix_offset(au: &[u8], codec: VideoCodec) -> Option<usize> {
    let mut i = 0usize;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 {
            let (prefix_len, data_at) = if au[i + 2] == 1 {
                (3, i + 3)
            } else if i + 4 <= au.len() && au[i + 2] == 0 && au[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };
            // H.265 NAL header is 2 bytes (forbidden_zero_bit | nal_unit_type | nuh_layer_id
            // | nuh_temporal_id_plus1); require both bytes to be present before classifying
            // H.265 VCL so a 1-byte truncated tail is never accepted as VCL.
            let has_header = match codec {
                VideoCodec::H265 => data_at + 1 < au.len(),
                _ => data_at < au.len(),
            };
            if has_header {
                let header = au[data_at];
                let is_vcl = match codec {
                    VideoCodec::H264 => (1..=5).contains(&(header & 0x1F)),
                    VideoCodec::H265 => ((header >> 1) & 0x3F) <= 31,
                    VideoCodec::H266 | VideoCodec::Av1 => false,
                };
                if is_vcl {
                    return Some(i);
                }
            }
            i += prefix_len;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpegts::mux::VideoCodec;

    // Minimal Annex-B AU builders for splice tests.
    fn h264_au(nal_headers: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for &h in nal_headers {
            v.extend_from_slice(&[0, 0, 0, 1, h, 0xAA, 0xBB]);
        }
        v
    }

    #[test]
    fn identifier_constants_match_st0604() {
        // §7.1 Table 1: ASCII "MISPmicrosectime".
        assert_eq!(&MISP_MICROSEC_ID_H264, b"MISPmicrosectime");
        // §7.2: a8687dd4-d759-3758-a5ce-f0338b6545f1
        assert_eq!(
            MISP_MICROSEC_ID_H265,
            [
                0xa8, 0x68, 0x7d, 0xd4, 0xd7, 0x59, 0x37, 0x58, 0xa5, 0xce, 0xf0, 0x33, 0x8b, 0x65,
                0x45, 0xf1
            ]
        );
        // §8.1: cf848278-ee23-306c-9265-e8fef22fb8b8
        assert_eq!(
            MISP_NANOSEC_ID_H265,
            [
                0xcf, 0x84, 0x82, 0x78, 0xee, 0x23, 0x30, 0x6c, 0x92, 0x65, 0xe8, 0xfe, 0xf2, 0x2f,
                0xb8, 0xb8
            ]
        );
    }

    #[test]
    fn payload_layout_matches_table_2() {
        // ST 0604.6 §7.4 Table 2: id(16) + status(1) + 2,2 FF 2,2 FF 2,2 FF 2,2.
        let ts = MispTimestamp::micros(0x0102_0304_0506_0708, 0x9F);
        let p = sei_payload(VideoCodec::H264, &ts).unwrap();
        assert_eq!(&p[..16], b"MISPmicrosectime");
        assert_eq!(p[16], 0x9F);
        assert_eq!(
            &p[17..28],
            &[
                0x01, 0x02, 0xFF, 0x03, 0x04, 0xFF, 0x05, 0x06, 0xFF, 0x07, 0x08
            ]
        );
    }

    #[test]
    fn kind_codec_matrix() {
        let nano = MispTimestamp::nanos(1, 0x1F);
        assert!(matches!(
            sei_payload(VideoCodec::H264, &nano),
            Err(MispTimeError::NanoUnsupportedForCodec { .. })
        ));
        assert!(sei_payload(VideoCodec::H265, &nano).is_ok());
        let micro = MispTimestamp::micros(1, 0x1F);
        assert_eq!(
            &sei_payload(VideoCodec::H265, &micro).unwrap()[..16],
            &MISP_MICROSEC_ID_H265
        );
        for c in [VideoCodec::H266, VideoCodec::Av1] {
            assert!(matches!(
                sei_payload(c, &micro),
                Err(MispTimeError::UnsupportedCodec { .. })
            ));
        }
    }

    #[test]
    fn rbsp_escape_inserts_before_low_bytes() {
        for (raw, escaped) in [
            (&[0x00, 0x00, 0x00][..], &[0x00, 0x00, 0x03, 0x00][..]),
            (&[0x00, 0x00, 0x01][..], &[0x00, 0x00, 0x03, 0x01][..]),
            (&[0x00, 0x00, 0x02][..], &[0x00, 0x00, 0x03, 0x02][..]),
            (&[0x00, 0x00, 0x03][..], &[0x00, 0x00, 0x03, 0x03][..]),
            (&[0x00, 0x00, 0x04][..], &[0x00, 0x00, 0x04][..]),
            // Escape resets the zero-run: 00 00 00 00 -> 00 00 03 00 00.
            (
                &[0x00, 0x00, 0x00, 0x00][..],
                &[0x00, 0x00, 0x03, 0x00, 0x00][..],
            ),
            (
                &[0xAA, 0x00, 0x00, 0x01, 0xBB][..],
                &[0xAA, 0x00, 0x00, 0x03, 0x01, 0xBB][..],
            ),
        ] {
            let mut out = Vec::new();
            append_rbsp_escaped(&mut out, raw);
            assert_eq!(out, escaped, "raw={raw:02X?}");
        }
    }

    #[test]
    fn build_sei_nal_h264_golden() {
        let ts = MispTimestamp::micros(0x0102_0304_0506_0708, 0x9F);
        let nal = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        // No escapes needed for this payload: header, type, size, 28, trailer.
        let mut expect = vec![0x06, 0x05, 28];
        expect.extend_from_slice(&sei_payload(VideoCodec::H264, &ts).unwrap());
        expect.push(0x80);
        assert_eq!(nal, expect);
    }

    #[test]
    fn build_sei_nal_h265_header() {
        let ts = MispTimestamp::nanos(7, 0x1F);
        let nal = build_sei_nal(VideoCodec::H265, &ts).unwrap();
        // nal_unit_type 39 (PREFIX_SEI) << 1 = 0x4E; layer 0, tid+1 = 1.
        assert_eq!(&nal[..2], &[0x4E, 0x01]);
        assert_eq!(nal[2], 0x05);
        assert_eq!(nal[3], 28);
        assert_eq!(&nal[4..20], &MISP_NANOSEC_ID_H265);
        assert_eq!(*nal.last().unwrap(), 0x80);
    }

    #[test]
    fn build_sei_nal_escapes_zero_run() {
        // status 0x00 + value MSBs 0x00 0x00 forms 00 00 00 inside the
        // payload -> the escaper MUST fire (identifier ends with 'e').
        let ts = MispTimestamp::micros(0x0000_0000_0000_0000, 0x00);
        let nal = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        assert!(nal.len() > 3 + 28 + 1, "escapes must lengthen the NAL");
        // No unescaped start-code-like sequence may remain anywhere.
        assert!(
            !nal.windows(3)
                .any(|w| w == [0, 0, 0] || w == [0, 0, 1] || w == [0, 0, 2])
        );
    }

    #[test]
    fn splice_lands_after_parameter_sets_before_idr() {
        // AUD(9), SPS(7), PPS(8), IDR(5): SEI must go right before the IDR.
        let au = h264_au(&[0x09, 0x67, 0x68, 0x65]); // types 9,7,8,5
        let sei = build_sei_nal(VideoCodec::H264, &MispTimestamp::micros(1, 0x1F)).unwrap();
        let out = insert_sei_before_first_vcl(&au, &sei, VideoCodec::H264).unwrap();
        // Expected: AUD+SPS+PPS bytes, then 00 00 01 + sei, then IDR NAL.
        let idr_at = au.len() - 7; // last NAL starts 7 bytes from the end
        assert_eq!(&out[..idr_at], &au[..idr_at]);
        assert_eq!(&out[idr_at..idr_at + 3], &[0, 0, 1]);
        assert_eq!(&out[idr_at + 3..idr_at + 3 + sei.len()], &sei[..]);
        assert_eq!(&out[idr_at + 3 + sei.len()..], &au[idr_at..]);
    }

    #[test]
    fn splice_before_lone_vcl() {
        let au = h264_au(&[0x41]); // type 1 non-IDR slice, nal_ref_idc=2
        let sei = build_sei_nal(VideoCodec::H264, &MispTimestamp::micros(1, 0x1F)).unwrap();
        let out = insert_sei_before_first_vcl(&au, &sei, VideoCodec::H264).unwrap();
        assert_eq!(&out[..3], &[0, 0, 1]); // SEI first (3-byte prefix)
        assert_eq!(&out[3..3 + sei.len()], &sei[..]);
    }

    #[test]
    fn splice_h265_vcl_detection() {
        // H.265 IDR_W_RADL = type 19 -> header byte (19 << 1) = 0x26, 0x01.
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0xAA]); // VPS (32)
        au.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0xBB]); // IDR_W_RADL (19)
        let sei = build_sei_nal(VideoCodec::H265, &MispTimestamp::micros(1, 0x1F)).unwrap();
        let out = insert_sei_before_first_vcl(&au, &sei, VideoCodec::H265).unwrap();
        assert_eq!(&out[..7], &au[..7]);
        assert_eq!(&out[7..10], &[0, 0, 1]);
    }

    #[test]
    fn splice_no_vcl_errors() {
        let au = h264_au(&[0x09, 0x67]); // AUD + SPS only
        let sei = build_sei_nal(VideoCodec::H264, &MispTimestamp::micros(1, 0x1F)).unwrap();
        assert!(matches!(
            insert_sei_before_first_vcl(&au, &sei, VideoCodec::H264),
            Err(MispTimeError::NoVclNal)
        ));
    }

    #[test]
    fn extract_round_trips_all_kinds() {
        for (codec, ts) in [
            (
                VideoCodec::H264,
                MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F),
            ),
            (VideoCodec::H265, MispTimestamp::micros(42, 0x00)),
            (VideoCodec::H265, MispTimestamp::nanos(u64::MAX, 0x9F)),
        ] {
            let sei = build_sei_nal(codec, &ts).unwrap();
            let au_tail = match codec {
                VideoCodec::H264 => h264_au(&[0x65]),
                _ => {
                    let mut v = Vec::new();
                    v.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0xBB]);
                    v
                }
            };
            let au = insert_sei_before_first_vcl(&au_tail, &sei, codec).unwrap();
            assert_eq!(extract(&au, codec).unwrap(), Some(ts), "{codec:?}");
        }
    }

    #[test]
    fn extract_absent_is_none() {
        assert_eq!(extract(&h264_au(&[0x65]), VideoCodec::H264).unwrap(), None);
        assert_eq!(extract(&[], VideoCodec::H264).unwrap(), None);
    }

    #[test]
    fn extract_foreign_uuid_is_none() {
        // A user_data_unregistered SEI with a non-MISP UUID: skipped.
        let mut nal = vec![0x06, 0x05, 28];
        nal.extend_from_slice(&[0x11; 28]);
        nal.push(0x80);
        let mut au = vec![0, 0, 1];
        au.extend_from_slice(&nal);
        au.extend_from_slice(&h264_au(&[0x65]));
        assert_eq!(extract(&au, VideoCodec::H264).unwrap(), None);
    }

    #[test]
    fn extract_bad_guard_byte_errors() {
        let ts = MispTimestamp::micros(0x0102_0304_0506_0708, 0x9F);
        let mut sei = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        // Payload starts at offset 3 (header, type, size); guard #1 is
        // payload[19] -> NAL offset 3 + 19 (no escapes in this payload).
        sei[3 + 19] = 0x00;
        let au = insert_sei_before_first_vcl(&h264_au(&[0x65]), &sei, VideoCodec::H264).unwrap();
        assert!(matches!(
            extract(&au, VideoCodec::H264),
            Err(MispTimeExtractError::BadGuardByte)
        ));
    }

    #[test]
    fn extract_survives_escaped_payload() {
        // The all-zero timestamp forces emulation-prevention bytes on
        // build; extract must transparently strip them.
        let ts = MispTimestamp::micros(0, 0x00);
        let sei = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        let au = insert_sei_before_first_vcl(&h264_au(&[0x65]), &sei, VideoCodec::H264).unwrap();
        assert_eq!(extract(&au, VideoCodec::H264).unwrap(), Some(ts));
    }

    // TruncatedSei when a confirmed MISP identifier is present but
    // the declared payload_size runs past the RBSP end.
    #[test]
    fn extract_truncated_misp_sei_errors() {
        let ts = MispTimestamp::micros(0x0102_0304_0506_0708, 0x9F);
        let sei = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        // Drop the last 6 bytes of the SEI NAL (mid-payload truncation).
        // Wrap it as a lone NAL: [0,0,1] + truncated_nal.
        let mut au = vec![0u8, 0, 1];
        au.extend_from_slice(&sei[..sei.len() - 6]);
        assert_eq!(
            extract(&au, VideoCodec::H264),
            Err(MispTimeExtractError::TruncatedSei),
            "confirmed MISP identifier + truncated payload must be Err"
        );
    }

    // Companion: fewer than 16 bytes available = identifier
    // unconfirmable = Ok(None) (not Err).
    #[test]
    fn extract_truncated_before_full_identifier_is_none() {
        let ts = MispTimestamp::micros(0x0102_0304_0506_0708, 0x9F);
        let sei = build_sei_nal(VideoCodec::H264, &ts).unwrap();
        // Keep only the NAL header byte (0x06), payload_type (0x05),
        // payload_size (28) and 10 identifier bytes — fewer than 16.
        // NAL layout: [0x06, 0x05, 28, id[0..10]]; p lands at byte 3,
        // so keep sei[0..3+10] = 13 bytes total.
        let mut au = vec![0u8, 0, 1];
        au.extend_from_slice(&sei[..3 + 10]);
        assert_eq!(
            extract(&au, VideoCodec::H264),
            Ok(None),
            "fewer than 16 identifier bytes = unconfirmable = Ok(None)"
        );
    }

    // H.265 SUFFIX_SEI (NAL type 40) is matched by the parser.
    #[test]
    fn demux_codec_converts_for_extract() {
        let d = crate::mpegts::demux::VideoCodec::H265;
        assert_eq!(VideoCodec::from(d), VideoCodec::H265);
    }

    #[test]
    fn extract_h265_suffix_sei_round_trips() {
        let ts = MispTimestamp::micros(0xDEAD_BEEF_CAFE_0001, 0x3F);
        // Build a normal PREFIX_SEI (type 39), then rewrite its 2-byte header
        // to SUFFIX_SEI (type 40): (40 << 1) | 0 = 0x50, nuh_temporal_id_plus1 = 1.
        let mut sei = build_sei_nal(VideoCodec::H265, &ts).unwrap();
        assert_eq!(
            sei[0], 0x4E,
            "sanity: first byte should be PREFIX_SEI header"
        );
        sei[0] = 0x50; // (40 << 1) = 0x50
        sei[1] = 0x01; // nuh_layer_id=0, nuh_temporal_id_plus1=1
        // Build an AU: a VCL NAL first, then the suffix SEI.
        let mut au = vec![0u8, 0, 0, 1, 0x26, 0x01, 0xBB]; // IDR_W_RADL
        au.extend_from_slice(&[0, 0, 1]);
        au.extend_from_slice(&sei);
        assert_eq!(
            extract(&au, VideoCodec::H265).unwrap(),
            Some(ts),
            "SUFFIX_SEI (type 40) must be extracted"
        );
    }

    // Regression: a 0xFF-chain payload_size that overflows usize MUST NOT
    // panic in debug builds. The RBSP here is: payload_type=5 (user_data_unregistered),
    // then 64 bytes of 0xFF (accumulated size = 64*255 = 16320, far past end), then a
    // non-0xFF terminator 0x00 ending the size accumulation, followed by 16 bytes
    // matching MISP_MICROSEC_ID_H264 as the "payload start" — so the identifier-peek
    // path fires and must return Err(TruncatedSei) (confirmed MISP, payload overruns).
    // A variant with fewer than 16 bytes after the chain must return Ok(None) instead.
    #[test]
    fn scan_sei_saturating_payload_size_no_panic() {
        // Construct raw RBSP: type=5, then 64×0xFF + 0x00 (terminates size loop
        // with accumulated size = 64*255 + 0 = 16320), then 16 MISP id bytes.
        // The SEI NAL for H.264 has a 1-byte header (0x06).
        let mut nal: Vec<u8> = Vec::new();
        nal.push(0x06); // H.264 SEI NAL header (nal_unit_type=6)
        // RBSP body (no emulation prevention bytes needed here — 0xFF is never escaped):
        nal.push(0x05); // payload_type = 5 (user_data_unregistered)
        nal.extend(core::iter::repeat(0xFF).take(64)); // 64-byte 0xFF size chain
        nal.push(0x00); // terminator byte (size accumulation ends, adds 0)
        nal.extend_from_slice(&MISP_MICROSEC_ID_H264); // 16 identifier bytes at "payload start"
        // Wrap as an Annex-B NAL (3-byte start code).
        let mut au: Vec<u8> = vec![0, 0, 1];
        au.extend_from_slice(&nal);
        // MUST NOT panic; confirmed MISP id present but payload overruns → TruncatedSei.
        assert_eq!(
            extract(&au, VideoCodec::H264),
            Err(MispTimeExtractError::TruncatedSei),
            "saturating payload_size + confirmed MISP id must be Err(TruncatedSei)"
        );

        // Variant: fewer than 16 bytes after the size chain → identifier unconfirmable.
        let mut nal2: Vec<u8> = Vec::new();
        nal2.push(0x06);
        nal2.push(0x05);
        nal2.extend(core::iter::repeat(0xFF).take(64));
        nal2.push(0x00);
        nal2.extend_from_slice(&MISP_MICROSEC_ID_H264[..8]); // only 8 bytes — < 16
        let mut au2: Vec<u8> = vec![0, 0, 1];
        au2.extend_from_slice(&nal2);
        assert_eq!(
            extract(&au2, VideoCodec::H264),
            Ok(None),
            "saturating payload_size + <16 identifier bytes must be Ok(None)"
        );
    }

    // Regression: H.265 AU ending in a 1-byte truncated NAL stub whose
    // single header byte pattern-matches a VCL type must NOT be treated as VCL.
    // AU: VPS (non-VCL type 32), then a 1-byte stub 0x26 (IDR_W_RADL first byte).
    // insert_sei_before_first_vcl must return Err(NoVclNal) — no complete VCL present.
    #[test]
    fn h265_one_byte_vcl_stub_not_classified_as_vcl() {
        // VPS NAL: type 32 -> first header byte = (32 << 1) = 0x40, second = 0x01.
        // Stub: just the single byte 0x26 = (19 << 1) which looks like IDR_W_RADL
        // but is truncated (missing the second header byte).
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0xAA]); // VPS (type 32, non-VCL)
        au.extend_from_slice(&[0, 0, 1, 0x26]); // 3-byte start + 1-byte stub (type 19 first byte)
        let sei = build_sei_nal(VideoCodec::H265, &MispTimestamp::micros(1, 0x1F)).unwrap();
        assert_eq!(
            insert_sei_before_first_vcl(&au, &sei, VideoCodec::H265),
            Err(MispTimeError::NoVclNal),
            "1-byte H.265 NAL stub must not be accepted as VCL"
        );
    }
}
