//! Cross-codec utility helpers used by the muxer/demuxer to derive
//! per-stream codec-specific stats (`StreamCodecCounters`).
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Today this module exposes a single helper: [`count_nal_units`], used
//! on the sender side to count NAL units (H.264/H.265/H.266) or OBUs
//! (AV1) inside a single access-unit buffer. The receiver side already
//! has `Vec<NalUnit>` / `Vec<Obu>` directly from
//! `mpegts::demux::payload::split_nals` and does not need this helper.

use crate::codec::VideoCodec;

/// Count NAL units (H.264/H.265/H.266) or OBUs (AV1) in a single
/// access-unit buffer.
///
/// **H.264/H.265/H.266:** Annex-B framing; counts start codes
/// (`0x000001` or `0x00000001`). A buffer with no start code returns 1
/// (one NAL with the start code stripped — defensive; muxer rejects
/// such buffers upstream via `validate_annex_b`, but the count is
/// still well-defined as "one NAL"). Empty buffers return 0.
///
/// **AV1:** OBU-LEB128 framing; walks the OBU stream header-by-header.
/// Each OBU header is 1 byte plus an optional extension byte; the
/// payload size is in a LEB128-encoded field when `obu_has_size_field`
/// bit is set (the convention for muxed AV1 in MPEG-TS per
/// [`AV1 Codec ISO Media File Format`] section 2.1 — `obu_has_size_field`
/// MUST be 1). Malformed OBU streams return the count of OBUs walked
/// before the parse failed.
///
/// [`AV1 Codec ISO Media File Format`]: https://aomediacodec.github.io/av1-isobmff/
pub fn count_nal_units(buf: &[u8], codec: VideoCodec) -> u64 {
    match codec {
        VideoCodec::H264 | VideoCodec::H265 | VideoCodec::H266 => count_annex_b_nals(buf),
        VideoCodec::Av1 => count_av1_obus(buf),
    }
}

fn count_annex_b_nals(buf: &[u8]) -> u64 {
    if buf.is_empty() {
        return 0;
    }
    let mut count: u64 = 0;
    let mut i: usize = 0;
    while i + 3 <= buf.len() {
        // 4-byte start code: 00 00 00 01
        if i + 4 <= buf.len()
            && buf[i] == 0
            && buf[i + 1] == 0
            && buf[i + 2] == 0
            && buf[i + 3] == 1
        {
            count += 1;
            i += 4;
            continue;
        }
        // 3-byte start code: 00 00 01
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            count += 1;
            i += 3;
            continue;
        }
        i += 1;
    }
    // Defensive: caller buffer with no start code at all is one NAL by
    // convention. validate_annex_b rejects this in push_video, but
    // count_nal_units must be well-defined on any byte slice.
    if count == 0 { 1 } else { count }
}

fn count_av1_obus(buf: &[u8]) -> u64 {
    let mut count: u64 = 0;
    let mut i: usize = 0;
    while i < buf.len() {
        // OBU header byte:
        //   obu_forbidden_bit (1)
        //   obu_type          (4)
        //   obu_extension_flag(1)
        //   obu_has_size_field(1)
        //   obu_reserved_1bit (1)
        let header = buf[i];
        let extension_flag = (header & 0b0000_0100) != 0;
        let has_size_field = (header & 0b0000_0010) != 0;
        i += 1;
        if extension_flag {
            if i >= buf.len() {
                return count;
            }
            i += 1; // skip extension byte
        }
        if has_size_field {
            // LEB128 size per AV1 spec §4.10.5, shared with the demux/mux
            // OBU walkers (also rejects sizes > u32::MAX per spec, one byte
            // earlier than a truncated-body bounds check would).
            let (size, consumed) = match crate::codec::av1::decode::leb128::read_leb128(buf, i) {
                Ok(t) => t,
                Err(_) => return count,
            };
            i += consumed;
            // Overflow safety: `size` is decoded from untrusted input as a
            // u64 and cast to usize; `i + size` must not wrap. Treat
            // overflow as "truncated" (same sentinel as the bounds-check
            // below) — count the header and stop walking.
            let Some(end) = i.checked_add(size as usize) else {
                return count + 1;
            };
            if end > buf.len() {
                // Truncated — count the header but stop walking.
                return count + 1;
            }
            i = end;
            count += 1;
        } else {
            // OBUs without size field consume the rest of the buffer per
            // AV1 §5.3.1. Count it and stop.
            return count + 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_empty_buf_returns_zero() {
        assert_eq!(count_nal_units(&[], VideoCodec::H264), 0);
    }

    #[test]
    fn annex_b_single_nal_3byte_start_code() {
        assert_eq!(
            count_nal_units(&[0x00, 0x00, 0x01, 0x09], VideoCodec::H264),
            1
        );
    }

    #[test]
    fn annex_b_single_nal_4byte_start_code() {
        assert_eq!(
            count_nal_units(&[0x00, 0x00, 0x00, 0x01, 0x09], VideoCodec::H264),
            1
        );
    }

    #[test]
    fn annex_b_two_nals_mixed_start_codes() {
        let buf = [
            0x00, 0x00, 0x01, 0x09, 0xF0, 0x00, 0x00, 0x00, 0x01, 0x05, 0xAA, 0xBB,
        ];
        assert_eq!(count_nal_units(&buf, VideoCodec::H264), 2);
    }

    #[test]
    fn annex_b_h265_three_nals() {
        let buf = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x44, 0x01,
        ];
        assert_eq!(count_nal_units(&buf, VideoCodec::H265), 3);
    }

    #[test]
    fn annex_b_h266_two_nals() {
        let buf = [
            0x00, 0x00, 0x00, 0x01, 0x00, 0x21, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x23, 0x00,
        ];
        assert_eq!(count_nal_units(&buf, VideoCodec::H266), 2);
    }

    #[test]
    fn av1_single_obu_with_size_field() {
        // OBU header: obu_type=6 (FRAME), has_size_field=1.
        //   0b0_0110_0_1_0 = 0x32
        // Size LEB128 = 2 (one-byte: 0x02)
        // Payload: 0xAA 0xBB
        let buf = [0x32, 0x02, 0xAA, 0xBB];
        assert_eq!(count_nal_units(&buf, VideoCodec::Av1), 1);
    }

    #[test]
    fn av1_two_obus_back_to_back() {
        // OBU 1: TEMPORAL_DELIMITER (type=2, has_size=1, size=0)
        //   header 0b0_0010_0_1_0 = 0x12
        // OBU 2: FRAME (type=6, has_size=1, size=2, payload 0xAA 0xBB)
        let buf = [0x12, 0x00, 0x32, 0x02, 0xAA, 0xBB];
        assert_eq!(count_nal_units(&buf, VideoCodec::Av1), 2);
    }

    #[test]
    fn av1_obu_without_size_field_terminates_walk() {
        // OBU type=6 (FRAME), has_size=0 → consumes rest of buffer = 1 OBU.
        //   header 0b0_0110_0_0_0 = 0x30
        let buf = [0x30, 0xAA, 0xBB, 0xCC, 0xDD];
        assert_eq!(count_nal_units(&buf, VideoCodec::Av1), 1);
    }

    #[test]
    fn av1_obu_huge_size_field_does_not_panic() {
        // Robustness: an OBU whose LEB128 size field decodes to the
        // largest representable 8-byte LEB128 value must NOT panic via
        // `i + (size as usize)` overflow — regardless of how the shared
        // `read_leb128` (AV1 spec §4.10.5) rejects it. Encoded max size is
        // (1 << 56) - 1, far past the spec's u32::MAX conformance ceiling,
        // so `read_leb128` now rejects it as invalid LEB128 (a decode
        // failure, same bucket as a truncated size field) rather than
        // accepting the huge value and failing the later bounds check —
        // the header is not counted either way, but the rejection point
        // moved one step earlier when `count_av1_obus` started sharing
        // `read_leb128` with the mux/demux OBU walkers.
        //
        // OBU header: type=6 (FRAME), has_size=1 → 0x32.
        // Then 8 LEB128 bytes: 7×0xFF (continuation) + 1×0x7F (terminator).
        let buf = [0x32, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        assert_eq!(count_nal_units(&buf, VideoCodec::Av1), 0);
    }

    #[test]
    fn annex_b_no_start_code_returns_one_defensively() {
        // Defensive contract: any non-empty buffer with no start code
        // counts as one NAL. Muxer validates start codes upstream; this
        // ensures count_nal_units is well-defined regardless.
        assert_eq!(count_nal_units(&[0xAB, 0xCD], VideoCodec::H264), 1);
    }
}
