//! Annex B ↔ length-prefixed NAL conversion.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! MPEG-TS elementary streams (and this crate's own [`crate::mpegts::demux`]
//! output) carry H.264/H.265/H.266 NAL units Annex-B-framed: each NAL is
//! delimited by a `00 00 01` or `00 00 00 01` start code. Some consumers —
//! notably Apple's VideoToolbox, and the ISO/IEC 14496-15 AVCC/HVCC sample
//! formats it expects — instead frame each NAL with a fixed-width
//! big-endian length prefix and no start code at all. This module converts
//! between the two framings without touching NAL contents: header bytes
//! and emulation-prevention bytes are passed through verbatim.
//!
//! Both directions are pure byte-plumbing — no bitstream parsing, no
//! codec-specific knowledge. `length_size` must be 1, 2, or 4 bytes,
//! matching the widths ISO/IEC 14496-15's `NALUnitLength` field allows.

use crate::codec::CodecParseError;
use crate::codec::annexb::start_codes;
use crate::mpegts::demux::VideoCodec;
use alloc::vec::Vec;

/// Validate a `length_size` argument, returning the maximum NAL byte
/// length it can encode.
fn max_encodable_len(length_size: u8) -> Result<u32, CodecParseError> {
    match length_size {
        1 => Ok(u8::MAX as u32),
        2 => Ok(u16::MAX as u32),
        4 => Ok(u32::MAX),
        other => Err(CodecParseError::InvalidLengthSize { got: other }),
    }
}

/// Convert an Annex-B-framed buffer (start-code-delimited NALs) into
/// length-prefixed framing: each NAL becomes a `length_size`-byte
/// big-endian length followed by the NAL bytes (header byte included).
///
/// Accepts both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start
/// codes; either may appear anywhere in `annexb`, including mixed within
/// the same buffer. Emulation-prevention bytes inside each NAL are left
/// untouched — they're part of the NAL payload on the wire, not part of
/// the framing this function rewrites.
///
/// `length_size` must be 1, 2, or 4 — [`CodecParseError::InvalidLengthSize`]
/// otherwise. If any single NAL's byte length exceeds what `length_size`
/// bytes can encode, returns [`CodecParseError::NalLengthOverflow`].
///
/// Bytes in `annexb` before the first start code (if any) are not part of
/// any NAL and are dropped, matching how [`crate::mpegts::demux`] itself
/// scans Annex B. An `annexb` with no start code at all yields an empty
/// `Vec`.
///
/// # C ABI
///
/// `tst_annexb_to_length_prefixed` — see `bindings/c/include/tstrans.h`.
pub fn annexb_to_length_prefixed(
    annexb: &[u8],
    length_size: u8,
) -> Result<Vec<u8>, CodecParseError> {
    let max_len = max_encodable_len(length_size)?;
    let starts = start_codes(annexb);
    let mut out = Vec::new();
    for win in starts.windows(2) {
        let nal = &annexb[win[0].data_start..win[1].prefix_start];
        write_length_prefixed_nal(&mut out, nal, length_size, max_len)?;
    }
    if let Some(&last) = starts.last() {
        let nal = &annexb[last.data_start..annexb.len()];
        write_length_prefixed_nal(&mut out, nal, length_size, max_len)?;
    }
    Ok(out)
}

fn write_length_prefixed_nal(
    out: &mut Vec<u8>,
    nal: &[u8],
    length_size: u8,
    max_len: u32,
) -> Result<(), CodecParseError> {
    let len = nal.len();
    if len as u64 > max_len as u64 {
        return Err(CodecParseError::NalLengthOverflow {
            nal_len: len.min(u32::MAX as usize) as u32,
            length_size,
        });
    }
    let len = len as u32;
    match length_size {
        1 => out.push(len as u8),
        2 => out.extend_from_slice(&(len as u16).to_be_bytes()),
        4 => out.extend_from_slice(&len.to_be_bytes()),
        _ => unreachable!("length_size validated by max_encodable_len before this is called"),
    }
    out.extend_from_slice(nal);
    Ok(())
}

/// Convert a length-prefixed buffer (each NAL preceded by a `length_size`-
/// byte big-endian length, no start codes) into Annex-B framing: each NAL
/// is emitted as a 4-byte `00 00 00 01` start code followed by the NAL
/// bytes, back to back.
///
/// `length_size` must be 1, 2, or 4 — [`CodecParseError::InvalidLengthSize`]
/// otherwise. If `data` ends mid-length-prefix or mid-NAL (fewer bytes
/// remain than the just-read length declares), returns
/// [`CodecParseError::Truncated`].
///
/// Inverse of [`annexb_to_length_prefixed`] up to start-code width
/// normalization: a 3-byte Annex-B start code round-trips through this
/// pair as a 4-byte one, since length-prefixed framing carries no
/// start-code-width information to preserve.
pub fn length_prefixed_to_annexb(data: &[u8], length_size: u8) -> Result<Vec<u8>, CodecParseError> {
    max_encodable_len(length_size)?;
    let length_size = length_size as usize;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let remaining = data.len() - pos;
        if remaining < length_size {
            return Err(CodecParseError::Truncated {
                needed: length_size as u32,
                had: remaining as u32,
            });
        }
        let nal_len = read_length_prefix(&data[pos..pos + length_size]);
        pos += length_size;

        let remaining = data.len() - pos;
        let nal_len = nal_len as usize;
        if remaining < nal_len {
            return Err(CodecParseError::Truncated {
                needed: nal_len as u32,
                had: remaining as u32,
            });
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + nal_len]);
        pos += nal_len;
    }
    Ok(out)
}

/// Read a big-endian length prefix of 1, 2, or 4 bytes. `prefix.len()`
/// must equal one of those widths (the only callers slice exactly
/// `length_size` bytes, and `length_size` is validated by
/// [`max_encodable_len`] before this is ever reached).
fn read_length_prefix(prefix: &[u8]) -> u32 {
    match prefix.len() {
        1 => prefix[0] as u32,
        2 => u16::from_be_bytes([prefix[0], prefix[1]]) as u32,
        4 => u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]),
        other => unreachable!("length_size validated to 1/2/4 before this is called, got {other}"),
    }
}

/// Complete parameter-set NALs extracted from an Annex-B access unit, as
/// `CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets` on Apple's
/// VideoToolbox wants them: each inner `Vec<u8>` is one complete NAL
/// (header byte(s) included), with no start code and no length prefix.
///
/// `vps` is always empty for H.264 — it has no VPS NAL type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

/// Scan an Annex-B access unit and collect its parameter-set NALs
/// (VPS/SPS/PPS), classified by NAL type, as complete NALs (header
/// byte(s) included, no start code, no length prefix) — ready for
/// `CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets`.
///
/// Non-fallible: a malformed or empty `annexb` (no start codes, or no
/// NAL classified as a parameter set) simply yields empty `Vec`s, not an
/// error.
///
/// NAL-type classification:
/// - **H.264**: `nal_type = byte0 & 0x1F`; SPS = 7, PPS = 8. `vps` is
///   always empty (H.264 has no VPS NAL type).
/// - **H.265**: `nal_type = (byte0 >> 1) & 0x3F`; VPS = 32, SPS = 33,
///   PPS = 34.
/// - **H.266**: not implemented in this arc — always returns an empty
///   [`ParameterSets`]. (H.266's own scheme would be VPS = 14, SPS = 15,
///   PPS = 16 under the same `(byte0 >> 1) & 0x3F` shift, but this PoC
///   targets H.264/HEVC only; wire H.266 up when it gets a VideoToolbox
///   consumer.)
/// - **AV1**: OBU-framed, not NAL-framed — always returns an empty
///   [`ParameterSets`].
///
/// # C ABI
///
/// `tst_param_sets_extract` — see `bindings/c/include/tstrans.h`.
pub fn extract_parameter_sets(annexb: &[u8], codec: VideoCodec) -> ParameterSets {
    let mut sets = ParameterSets::default();
    match codec {
        VideoCodec::H266 | VideoCodec::Av1 => return sets,
        VideoCodec::H264 | VideoCodec::H265 => {}
    }

    let starts = start_codes(annexb);
    for win in starts.windows(2) {
        classify_parameter_set(
            &annexb[win[0].data_start..win[1].prefix_start],
            codec,
            &mut sets,
        );
    }
    if let Some(&last) = starts.last() {
        classify_parameter_set(&annexb[last.data_start..annexb.len()], codec, &mut sets);
    }
    sets
}

/// Classify one already-extracted NAL by its header byte and, if it's a
/// parameter set, push a copy into the matching field of `sets`. Any
/// other NAL type (slices, SEI, AUD, …) is silently ignored — this is a
/// filter, not a validator.
fn classify_parameter_set(nal: &[u8], codec: VideoCodec, sets: &mut ParameterSets) {
    let Some(&byte0) = nal.first() else {
        return;
    };
    match codec {
        VideoCodec::H264 => match byte0 & 0x1F {
            7 => sets.sps.push(nal.to_vec()),
            8 => sets.pps.push(nal.to_vec()),
            _ => {}
        },
        VideoCodec::H265 => match (byte0 >> 1) & 0x3F {
            32 => sets.vps.push(nal.to_vec()),
            33 => sets.sps.push(nal.to_vec()),
            34 => sets.pps.push(nal.to_vec()),
            _ => {}
        },
        VideoCodec::H266 | VideoCodec::Av1 => {
            unreachable!("filtered out by extract_parameter_sets before this is called")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built two-NAL Annex B buffer: NAL 1 uses a 4-byte start code,
    /// NAL 2 uses a 3-byte start code (mixed on purpose, since both are
    /// legal anywhere in an Annex-B stream).
    fn two_nal_annexb() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x01, // 4-byte start code
            0x67, 0xAA, 0xBB, 0xCC, // NAL 1: header 0x67 + 3 payload bytes (len 4)
            0x00, 0x00, 0x01, // 3-byte start code
            0x65, 0xDD, 0xEE, // NAL 2: header 0x65 + 2 payload bytes (len 3)
        ]
    }

    #[test]
    fn annexb_to_length_prefixed_emits_be_lengths_and_header_bytes() {
        let out = annexb_to_length_prefixed(&two_nal_annexb(), 4).unwrap();
        assert_eq!(
            out,
            vec![
                0x00, 0x00, 0x00, 0x04, // NAL 1 length, big-endian
                0x67, 0xAA, 0xBB, 0xCC, // NAL 1 bytes, header included
                0x00, 0x00, 0x00, 0x03, // NAL 2 length, big-endian
                0x65, 0xDD, 0xEE, // NAL 2 bytes, header included
            ]
        );
    }

    #[test]
    fn round_trip_normalizes_start_code_width_to_four_bytes() {
        let annexb = two_nal_annexb();
        let length_prefixed = annexb_to_length_prefixed(&annexb, 4).unwrap();
        let round_tripped = length_prefixed_to_annexb(&length_prefixed, 4).unwrap();
        // NAL 2's 3-byte start code in the input normalizes to 4 bytes —
        // length-prefixed framing carries no start-code-width information.
        let normalized = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, 0xCC, // NAL 1
            0x00, 0x00, 0x00, 0x01, 0x65, 0xDD, 0xEE, // NAL 2, now 4-byte start code
        ];
        assert_eq!(round_tripped, normalized);
    }

    #[test]
    fn annexb_to_length_prefixed_rejects_invalid_length_size() {
        let err = annexb_to_length_prefixed(&two_nal_annexb(), 3).unwrap_err();
        assert_eq!(err, CodecParseError::InvalidLengthSize { got: 3 });
    }

    #[test]
    fn length_prefixed_to_annexb_rejects_invalid_length_size() {
        let err = length_prefixed_to_annexb(&[0x00, 0x01, 0xAA], 3).unwrap_err();
        assert_eq!(err, CodecParseError::InvalidLengthSize { got: 3 });
    }

    #[test]
    fn annexb_to_length_prefixed_one_byte_length_size_round_trips() {
        let out = annexb_to_length_prefixed(&two_nal_annexb(), 1).unwrap();
        assert_eq!(
            out,
            vec![
                0x04, 0x67, 0xAA, 0xBB, 0xCC, // NAL 1: 1-byte length + bytes
                0x03, 0x65, 0xDD, 0xEE, // NAL 2: 1-byte length + bytes
            ]
        );
        let annexb = length_prefixed_to_annexb(&out, 1).unwrap();
        assert_eq!(
            annexb,
            vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, 0xCC, 0x00, 0x00, 0x00, 0x01, 0x65, 0xDD,
                0xEE,
            ]
        );
    }

    #[test]
    fn nal_exceeding_one_byte_length_size_overflows() {
        // 256 payload bytes -> NAL length 257, too large for a 1-byte prefix.
        let mut annexb = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        annexb.extend(core::iter::repeat(0xAA).take(256));
        let err = annexb_to_length_prefixed(&annexb, 1).unwrap_err();
        assert_eq!(
            err,
            CodecParseError::NalLengthOverflow {
                nal_len: 257,
                length_size: 1,
            }
        );
    }

    #[test]
    fn length_prefixed_to_annexb_truncated_prefix_errors() {
        // Only 1 byte present where a 2-byte prefix is required.
        let err = length_prefixed_to_annexb(&[0xAA], 2).unwrap_err();
        assert_eq!(err, CodecParseError::Truncated { needed: 2, had: 1 });
    }

    #[test]
    fn length_prefixed_to_annexb_truncated_nal_body_errors() {
        // 4-byte length prefix declares 10 bytes of NAL, only 2 follow.
        let data = vec![0x00, 0x00, 0x00, 0x0A, 0x67, 0xAA];
        let err = length_prefixed_to_annexb(&data, 4).unwrap_err();
        assert_eq!(err, CodecParseError::Truncated { needed: 10, had: 2 });
    }

    #[test]
    fn empty_input_round_trips_to_empty_output() {
        assert_eq!(annexb_to_length_prefixed(&[], 4).unwrap(), Vec::<u8>::new());
        assert_eq!(length_prefixed_to_annexb(&[], 4).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn bytes_before_first_start_code_are_dropped() {
        let mut annexb = vec![0xDE, 0xAD, 0xBE, 0xEF]; // no start code here
        annexb.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x01]);
        let out = annexb_to_length_prefixed(&annexb, 4).unwrap();
        assert_eq!(out, vec![0x00, 0x00, 0x00, 0x02, 0x67, 0x01]);
    }

    /// Hand-built H.264 AU: SPS(0x67) + PPS(0x68) + an IDR slice(0x65)
    /// that must NOT be classified as a parameter set.
    fn h264_sps_pps_idr_annexb() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x01, // start code
            0x67, 0x42, 0x00, 0x1E, // SPS: nal_type = 0x67 & 0x1F = 7
            0x00, 0x00, 0x00, 0x01, // start code
            0x68, 0xCE, 0x3C, 0x80, // PPS: nal_type = 0x68 & 0x1F = 8
            0x00, 0x00, 0x00, 0x01, // start code
            0x65, 0x88, 0x84, 0x00, // IDR slice: nal_type = 0x65 & 0x1F = 5 (not a param set)
        ]
    }

    #[test]
    fn extract_parameter_sets_h264_collects_full_sps_and_pps_nals() {
        let sets = extract_parameter_sets(&h264_sps_pps_idr_annexb(), VideoCodec::H264);
        assert_eq!(sets.sps, vec![vec![0x67, 0x42, 0x00, 0x1E]]);
        assert_eq!(sets.pps, vec![vec![0x68, 0xCE, 0x3C, 0x80]]);
        assert!(sets.vps.is_empty());
    }

    /// Hand-built H.265 AU: VPS(0x40) + SPS(0x42) + PPS(0x44). NAL type
    /// is `(byte0 >> 1) & 0x3F`: 0x40 -> 32 (VPS), 0x42 -> 33 (SPS),
    /// 0x44 -> 34 (PPS).
    fn h265_vps_sps_pps_annexb() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x01, // start code
            0x40, 0x01, 0x0C, 0x01, // VPS
            0x00, 0x00, 0x00, 0x01, // start code
            0x42, 0x01, 0x01, 0x02, // SPS
            0x00, 0x00, 0x00, 0x01, // start code
            0x44, 0x01, 0xC0, 0xF3, // PPS
        ]
    }

    #[test]
    fn extract_parameter_sets_h265_collects_full_vps_sps_pps_nals() {
        let sets = extract_parameter_sets(&h265_vps_sps_pps_annexb(), VideoCodec::H265);
        assert_eq!(sets.vps, vec![vec![0x40, 0x01, 0x0C, 0x01]]);
        assert_eq!(sets.sps, vec![vec![0x42, 0x01, 0x01, 0x02]]);
        assert_eq!(sets.pps, vec![vec![0x44, 0x01, 0xC0, 0xF3]]);
    }

    #[test]
    fn extract_parameter_sets_h266_returns_empty_for_now() {
        let sets = extract_parameter_sets(&h264_sps_pps_idr_annexb(), VideoCodec::H266);
        assert_eq!(sets, ParameterSets::default());
    }

    #[test]
    fn extract_parameter_sets_av1_returns_empty() {
        let sets = extract_parameter_sets(&h264_sps_pps_idr_annexb(), VideoCodec::Av1);
        assert_eq!(sets, ParameterSets::default());
    }

    #[test]
    fn extract_parameter_sets_empty_input_returns_empty() {
        assert_eq!(
            extract_parameter_sets(&[], VideoCodec::H264),
            ParameterSets::default()
        );
    }
}
