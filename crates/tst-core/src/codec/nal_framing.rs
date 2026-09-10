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
use crate::codec::annexb::nals;
use crate::mpegts::demux::VideoCodec;
use alloc::vec;
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
/// Callers that already own an output buffer — the C ABI among them —
/// should use [`annexb_to_length_prefixed_len`] to size it and
/// [`annexb_to_length_prefixed_into`] to fill it; this function is those
/// two over a freshly allocated `Vec`.
///
/// # C ABI
///
/// `tst_annexb_to_length_prefixed` — see `bindings/c/include/tstrans.h`.
pub fn annexb_to_length_prefixed(
    annexb: &[u8],
    length_size: u8,
) -> Result<Vec<u8>, CodecParseError> {
    let needed = annexb_to_length_prefixed_len(annexb, length_size)?;
    let mut out = vec![0u8; needed];
    // Sizing already accepted this input, so the write cannot fail: the
    // buffer is exactly `needed` bytes and every NAL re-validates to the
    // same lengths. `?` rather than an `unwrap` so a future divergence
    // between the two walks surfaces as an error, not a panic.
    write_nals(annexb, length_size, &mut out)?;
    Ok(out)
}

/// Byte length [`annexb_to_length_prefixed`] would produce for the same
/// arguments, without building the output.
///
/// Rejects exactly what [`annexb_to_length_prefixed`] rejects and with
/// the same error, so it can be used as a pure sizing pass ahead of an
/// [`annexb_to_length_prefixed_into`] call — that pairing is what the C
/// ABI's two-call idiom is built on.
///
/// Allocation-free.
pub fn annexb_to_length_prefixed_len(
    annexb: &[u8],
    length_size: u8,
) -> Result<usize, CodecParseError> {
    let max_len = max_encodable_len(length_size)?;
    let mut total: usize = 0;
    for nal in nals(annexb) {
        let one = length_prefixed_nal_len(nal, length_size, max_len)?;
        // Cannot overflow: a length-prefixed rendering is at most 4/3 the
        // size of its Annex-B input (a 4-byte prefix replacing a 3-byte
        // start code is the worst case, and every NAL consumes its own
        // bytes on both sides), so the total is bounded by
        // 4/3 * isize::MAX — comfortably inside `usize`. Left as a plain
        // add rather than a `checked_add`: there is no error in
        // `CodecParseError` that honestly describes a total-length
        // overflow, and inventing one would put a misleading message
        // ("NAL length N exceeds ...") on a state the slice-length
        // invariant already rules out. Debug builds — the `nal_framing`
        // fuzz target among them — still trap on overflow, and
        // `write_nals` re-checks every NAL against the real buffer, so
        // even a wrapped total would surface as `BufferTooSmall` rather
        // than a bad write.
        total += one;
    }
    Ok(total)
}

/// Write what [`annexb_to_length_prefixed`] would return into `out`,
/// returning the number of bytes written (always the same number
/// [`annexb_to_length_prefixed_len`] reports for the same arguments).
///
/// Rejects exactly what [`annexb_to_length_prefixed`] rejects and with
/// the same error. An `out` shorter than the conversion needs is
/// [`CodecParseError::BufferTooSmall`] carrying the required size; `out`
/// is left completely unmodified on every error path, and bytes beyond
/// the returned count are never touched on success.
///
/// Allocation-free.
pub fn annexb_to_length_prefixed_into(
    annexb: &[u8],
    length_size: u8,
    out: &mut [u8],
) -> Result<usize, CodecParseError> {
    let needed = annexb_to_length_prefixed_len(annexb, length_size)?;
    if out.len() < needed {
        return Err(CodecParseError::BufferTooSmall {
            needed,
            have: out.len(),
        });
    }
    write_nals(annexb, length_size, &mut out[..needed])
}

/// Byte length of one NAL's length-prefixed rendering, or
/// [`CodecParseError::NalLengthOverflow`] if `length_size` bytes cannot
/// encode it. Cannot overflow: `nal.len()` is at most `isize::MAX`.
fn length_prefixed_nal_len(
    nal: &[u8],
    length_size: u8,
    max_len: u32,
) -> Result<usize, CodecParseError> {
    if nal.len() as u64 > max_len as u64 {
        return Err(CodecParseError::NalLengthOverflow {
            nal_len: saturating_u32(nal.len()),
            length_size,
        });
    }
    Ok(length_size as usize + nal.len())
}

/// `len` as a `u32`, saturating — for the `nal_len` diagnostic field on
/// [`CodecParseError::NalLengthOverflow`], which is `u32` and is only
/// ever populated on the path where the length is already too large.
fn saturating_u32(len: usize) -> u32 {
    len.min(u32::MAX as usize) as u32
}

/// Write the length-prefixed rendering of `annexb` into `out`, returning
/// the bytes written.
///
/// Callers size `out` with [`annexb_to_length_prefixed_len`] first, so
/// the capacity check here is a belt-and-braces guard against the two
/// walks disagreeing — never the primary error report (that is
/// [`annexb_to_length_prefixed_into`]'s up-front check, which leaves
/// `out` untouched). Because it stops at the first NAL that does not
/// fit, the `needed` it reports on that unreachable path is a lower
/// bound rather than the full requirement.
fn write_nals(annexb: &[u8], length_size: u8, out: &mut [u8]) -> Result<usize, CodecParseError> {
    let max_len = max_encodable_len(length_size)?;
    let width = length_size as usize;
    let mut written = 0usize;
    for nal in nals(annexb) {
        let one = length_prefixed_nal_len(nal, length_size, max_len)?;
        // Subtract rather than add: `written <= out.len()` is the loop
        // invariant, so this cannot overflow the way `written + one`
        // could for a pathologically large NAL.
        if one > out.len() - written {
            return Err(CodecParseError::BufferTooSmall {
                needed: written.saturating_add(one),
                have: out.len(),
            });
        }
        let end = written + one;
        let len = nal.len() as u32;
        match length_size {
            1 => out[written] = len as u8,
            2 => out[written..written + 2].copy_from_slice(&(len as u16).to_be_bytes()),
            4 => out[written..written + 4].copy_from_slice(&len.to_be_bytes()),
            _ => unreachable!("length_size validated by max_encodable_len before this is called"),
        }
        out[written + width..end].copy_from_slice(nal);
        written = end;
    }
    Ok(written)
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

    for nal in nals(annexb) {
        classify_parameter_set(nal, codec, &mut sets);
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

    /// Every Annex-B shape the three sizing/writing entry points have to
    /// agree on, paired with the `length_size` each is exercised at. The
    /// 70,000-byte NAL at `length_size = 2` is the overflow row: it is the
    /// only member whose `Vec` conversion fails, so it pins that `_len`
    /// and `_into` refuse exactly what the `Vec` path refuses.
    fn agreement_corpus() -> Vec<(&'static str, Vec<u8>, u8)> {
        let mut big_nal = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        big_nal.extend(core::iter::repeat(0xAA).take(70_000));

        let mut leading_garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        leading_garbage.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x01]);

        vec![
            ("empty", Vec::new(), 4),
            (
                "one NAL, 3-byte start code",
                vec![0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB],
                4,
            ),
            (
                "one NAL, 4-byte start code",
                vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB],
                4,
            ),
            ("three NALs, mixed widths", three_nal_annexb(), 4),
            ("three NALs, 1-byte length prefix", three_nal_annexb(), 1),
            ("three NALs, 2-byte length prefix", three_nal_annexb(), 2),
            ("70,000-byte NAL at length_size 2", big_nal, 2),
            ("bytes before the first start code", leading_garbage, 4),
            (
                "trailing zero run that is not a start code",
                vec![0x00, 0x00, 0x01, 0x41, 0x00, 0x00],
                4,
            ),
            (
                // Back-to-back start codes: the first delimits a
                // zero-length NAL, which must still be sized and written
                // as a bare length prefix with no body.
                "adjacent start codes (empty NAL)",
                vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x41],
                4,
            ),
            ("no start code at all", vec![0x01, 0x02, 0x03, 0x04], 4),
            ("invalid length_size", three_nal_annexb(), 3),
        ]
    }

    /// Three NALs: 4-byte, 3-byte and 4-byte start codes, bodies of 4, 3
    /// and 2 bytes.
    fn three_nal_annexb() -> Vec<u8> {
        vec![
            0x00, 0x00, 0x00, 0x01, // 4-byte start code
            0x67, 0xAA, 0xBB, 0xCC, // NAL 1 (len 4)
            0x00, 0x00, 0x01, // 3-byte start code
            0x65, 0xDD, 0xEE, // NAL 2 (len 3)
            0x00, 0x00, 0x00, 0x01, // 4-byte start code
            0x68, 0xFF, // NAL 3 (len 2)
        ]
    }

    /// `_len` and `_into` must be indistinguishable from the `Vec` path:
    /// same acceptance, same byte count, same bytes. Written as one sweep
    /// over the corpus so a new shape only has to be added in one place.
    #[test]
    fn len_and_into_agree_with_the_vec_path() {
        for (name, annexb, length_size) in agreement_corpus() {
            let vec_result = annexb_to_length_prefixed(&annexb, length_size);
            let len_result = annexb_to_length_prefixed_len(&annexb, length_size);

            match (&vec_result, &len_result) {
                (Ok(v), Ok(n)) => assert_eq!(v.len(), *n, "{name}: _len disagrees with vec.len()"),
                (Err(ve), Err(le)) => {
                    assert_eq!(ve, le, "{name}: _len raised a different error");
                    // The error path must also be reproduced by _into,
                    // with a buffer large enough that capacity cannot be
                    // the cause.
                    let mut out = vec![0u8; 1024];
                    assert_eq!(
                        annexb_to_length_prefixed_into(&annexb, length_size, &mut out).as_ref(),
                        Err(ve),
                        "{name}: _into raised a different error"
                    );
                    continue;
                }
                _ => panic!("{name}: _len and the vec path disagree on success/failure"),
            }

            let expected = vec_result.unwrap();
            let needed = len_result.unwrap();

            // Exactly-sized buffer: writes the whole rendering, reports it.
            let mut exact = vec![0u8; needed];
            assert_eq!(
                annexb_to_length_prefixed_into(&annexb, length_size, &mut exact),
                Ok(needed),
                "{name}: _into into an exactly-sized buffer"
            );
            assert_eq!(exact, expected, "{name}: _into wrote different bytes");

            // Oversized buffer: writes the same prefix, leaves the tail alone.
            let mut roomy = vec![0xCDu8; needed + 8];
            assert_eq!(
                annexb_to_length_prefixed_into(&annexb, length_size, &mut roomy),
                Ok(needed),
                "{name}: _into into an oversized buffer"
            );
            assert_eq!(&roomy[..needed], &expected[..], "{name}: oversized prefix");
            assert_eq!(&roomy[needed..], &[0xCDu8; 8], "{name}: tail was touched");

            // One byte short: refuses, naming the true requirement, and
            // leaves the caller's buffer completely unchanged.
            if needed > 0 {
                let mut short = vec![0xCDu8; needed - 1];
                assert_eq!(
                    annexb_to_length_prefixed_into(&annexb, length_size, &mut short),
                    Err(CodecParseError::BufferTooSmall {
                        needed,
                        have: needed - 1,
                    }),
                    "{name}: _into with one byte too few"
                );
                assert_eq!(
                    short,
                    vec![0xCDu8; needed - 1],
                    "{name}: a refused _into wrote into the buffer"
                );
            }
        }
    }

    #[test]
    fn nal_exceeding_two_byte_length_size_overflows() {
        let mut annexb = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        annexb.extend(core::iter::repeat(0xAA).take(70_000));
        assert_eq!(
            annexb_to_length_prefixed_len(&annexb, 2).unwrap_err(),
            CodecParseError::NalLengthOverflow {
                nal_len: 70_001,
                length_size: 2,
            }
        );
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
