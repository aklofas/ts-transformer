//! MPEG audio header decoding and frame iterator implementation.

use super::model::{ChannelMode, Frame, Frames, Layer, Version};
use super::tables::{BITRATE_TABLE, SAMPLE_RATE_TABLE, bitrate_column};
use crate::codec::CodecParseError;

/// Decode bitrate (kbps) from `(version, layer, bitrate_index)` per
/// ISO 11172-3 §2.4.2.3 Table 8 + ISO 13818-3 Table 5.
///
/// Errors:
/// - [`CodecParseError::UnsupportedFreeFormat`] for `bitrate_index == 0`
///   (free-format mode — frame length must be discovered by scanning for
///   the next syncword; we do not implement that today).
/// - [`CodecParseError::Forbidden`] for `bitrate_index == 15`.
pub(crate) fn decode_bitrate(
    version: Version,
    layer: Layer,
    bitrate_index: u8,
) -> Result<u32, CodecParseError> {
    if bitrate_index == 0 {
        return Err(CodecParseError::UnsupportedFreeFormat {
            layer: layer_to_u8(layer),
        });
    }
    if bitrate_index == 15 {
        return Err(CodecParseError::Forbidden {
            field: "bitrate_index",
        });
    }
    let col = bitrate_column(version, layer);
    Ok(BITRATE_TABLE[col][bitrate_index as usize])
}

/// Map a [`Layer`] to its numeric 1/2/3 form for diagnostic surfacing.
fn layer_to_u8(layer: Layer) -> u8 {
    match layer {
        Layer::I => 1,
        Layer::II => 2,
        Layer::III => 3,
    }
}

/// Decode sample rate (Hz) from `(version, sample_rate_index)`.
///
/// Errors: `ReservedValue { field: "sample_rate_index", value: 3 }`.
pub(crate) fn decode_sample_rate(
    version: Version,
    sample_rate_index: u8,
) -> Result<u32, CodecParseError> {
    if sample_rate_index == 3 {
        return Err(CodecParseError::ReservedValue {
            field: "sample_rate_index",
            value: 3,
        });
    }
    let row = match version {
        Version::Mpeg1 => 0,
        Version::Mpeg2 => 1,
        Version::Mpeg2_5 => 2,
    };
    Ok(SAMPLE_RATE_TABLE[row][sample_rate_index as usize])
}

/// Decode channel mode from the 2-bit header field (bits 25-26).
fn decode_channel_mode(bits: u8) -> ChannelMode {
    match bits & 0b11 {
        0b00 => ChannelMode::Stereo,
        0b01 => ChannelMode::JointStereo,
        0b10 => ChannelMode::DualChannel,
        0b11 => ChannelMode::Mono,
        _ => unreachable!(),
    }
}

/// Return the number of channels for a given channel mode.
fn channels_for_mode(mode: ChannelMode) -> u8 {
    match mode {
        ChannelMode::Mono => 1,
        ChannelMode::Stereo | ChannelMode::JointStereo | ChannelMode::DualChannel => 2,
    }
}

/// Return the number of samples per frame for a given (version, layer) pair.
/// Per ISO 11172-3 (MPEG-1) + ISO 13818-3 (MPEG-2/2.5).
fn samples_per_frame(version: Version, layer: Layer) -> u16 {
    match (version, layer) {
        (_, Layer::I) => 384,
        (_, Layer::II) => 1152,
        (Version::Mpeg1, Layer::III) => 1152,
        (Version::Mpeg2, Layer::III) | (Version::Mpeg2_5, Layer::III) => 576,
    }
}

/// Compute frame length in bytes per spec.
///
/// Formulas:
/// - Layer I: `(12 * bitrate * 1000 / sample_rate + padding) * 4`
/// - Layer II: `144 * bitrate * 1000 / sample_rate + padding`
/// - Layer III, MPEG-1: `144 * bitrate * 1000 / sample_rate + padding`
/// - Layer III, MPEG-2/2.5: `72 * bitrate * 1000 / sample_rate + padding`
///
/// `bitrate` is in kbps; `sample_rate` is in Hz.
fn frame_length(
    layer: Layer,
    version: Version,
    bitrate_kbps: u32,
    sample_rate_hz: u32,
    padding: bool,
) -> u32 {
    let pad = if padding { 1 } else { 0 };
    let bitrate_bps = bitrate_kbps * 1000;
    match layer {
        Layer::I => (12 * bitrate_bps / sample_rate_hz + pad) * 4,
        Layer::II => 144 * bitrate_bps / sample_rate_hz + pad,
        Layer::III => match version {
            Version::Mpeg1 => 144 * bitrate_bps / sample_rate_hz + pad,
            Version::Mpeg2 | Version::Mpeg2_5 => 72 * bitrate_bps / sample_rate_hz + pad,
        },
    }
}

/// Decoded view of the 4-byte header (no body slice yet).
#[derive(Debug)]
pub(super) struct Header {
    pub(super) version: Version,
    pub(super) layer: Layer,
    pub(super) bitrate_kbps: u32,
    pub(super) sample_rate_hz: u32,
    pub(super) channel_mode: ChannelMode,
    pub(super) channels: u8,
    pub(super) frame_length_bytes: u32,
    pub(super) samples_per_frame: u16,
    pub(super) has_crc: bool,
    pub(super) raw: [u8; 4],
}

/// Decode the 4-byte MPEG audio header.
///
/// Field layout (MSB first):
///   syncword:           bits  0..11 (12 bits, must be 0xFFE or 0xFFF)
///   version_id:         bits 11..13 (2 bits)
///       0b00 = MPEG-2.5; 0b01 = reserved; 0b10 = MPEG-2; 0b11 = MPEG-1
///   layer:              bits 13..15 (2 bits)
///       0b00 = reserved; 0b01 = Layer III; 0b10 = Layer II; 0b11 = Layer I
///   protection_bit:     bit  15 (1 bit)
///       0 = CRC follows; 1 = no CRC
///   bitrate_index:      bits 16..20 (4 bits)
///   sample_rate_index:  bits 20..22 (2 bits)
///   padding_bit:        bit  22 (1 bit)
///   private_bit:        bit  23 (1 bit)
///   channel_mode:       bits 24..26 (2 bits)
pub(super) fn parse_header(bytes: &[u8]) -> Result<Header, CodecParseError> {
    if bytes.len() < 4 {
        return Err(CodecParseError::Truncated {
            needed: 4,
            had: bytes.len() as u32,
        });
    }
    let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
    let h: u32 = ((raw[0] as u32) << 24)
        | ((raw[1] as u32) << 16)
        | ((raw[2] as u32) << 8)
        | (raw[3] as u32);

    // Sync word: top 11 bits must be 0x7FF (frame sync). We validate the
    // 12-bit form here; the 12th bit (next: version_id MSB) is allowed to
    // be 0 (MPEG-2.5) or 1.
    let sync = (h >> 21) & 0x7FF;
    if sync != 0x7FF {
        return Err(CodecParseError::BadSyncWord {
            expected: 0x7FF,
            found: sync as u16,
        });
    }

    let version_id = ((h >> 19) & 0b11) as u8;
    let version = match version_id {
        0b00 => Version::Mpeg2_5,
        0b01 => {
            return Err(CodecParseError::ReservedValue {
                field: "version_id",
                value: 0b01,
            });
        }
        0b10 => Version::Mpeg2,
        0b11 => Version::Mpeg1,
        _ => unreachable!(),
    };

    let layer_id = ((h >> 17) & 0b11) as u8;
    let layer = match layer_id {
        0b00 => {
            return Err(CodecParseError::ReservedValue {
                field: "layer",
                value: 0,
            });
        }
        0b01 => Layer::III,
        0b10 => Layer::II,
        0b11 => Layer::I,
        _ => unreachable!(),
    };

    let protection_bit = ((h >> 16) & 1) as u8;
    let has_crc = protection_bit == 0;

    let bitrate_index = ((h >> 12) & 0b1111) as u8;
    let bitrate_kbps = decode_bitrate(version, layer, bitrate_index)?;

    let sample_rate_index = ((h >> 10) & 0b11) as u8;
    let sample_rate_hz = decode_sample_rate(version, sample_rate_index)?;

    let padding = ((h >> 9) & 1) != 0;

    let channel_mode_bits = ((h >> 6) & 0b11) as u8;
    let channel_mode = decode_channel_mode(channel_mode_bits);
    let channels = channels_for_mode(channel_mode);

    let frame_length_bytes = frame_length(layer, version, bitrate_kbps, sample_rate_hz, padding);
    let samples_per_frame = samples_per_frame(version, layer);

    Ok(Header {
        version,
        layer,
        bitrate_kbps,
        sample_rate_hz,
        channel_mode,
        channels,
        frame_length_bytes,
        samples_per_frame,
        has_crc,
        raw,
    })
}

/// Construct a strict (fail-fast) frame iterator over an MPEG audio
/// elementary stream (PES payload bytes).
///
/// The first parse error terminates the iterator. Use
/// [`frames_with_resync`] when populating stats from possibly-corrupted
/// streams, where dropping every frame after the first malformed one is
/// worse than yielding an error and continuing.
pub fn frames(bytes: &[u8]) -> Frames<'_> {
    Frames {
        buf: bytes,
        cursor: 0,
        done: false,
        resync: false,
    }
}

/// Construct a best-effort frame iterator that scans forward for the
/// next plausible 11-bit MPEG audio syncword after each parse error.
///
/// On a parse error at position `C`, the iterator yields the error
/// once, then advances the cursor to the next plausible syncword in
/// `buf[C+1..]` (or to the end-of-buffer if none is found, after which
/// the iterator terminates).
///
/// This does NOT implement free-format frame-length discovery — when
/// the spec-defined sync-scan strategy resolves to a corrupted region,
/// the iterator may yield further errors at the same density as the
/// corruption. Strict callers (fuzzers, conformance tests) should use
/// [`frames`]; stats/telemetry callers should prefer
/// `frames_with_resync` to avoid stream-wide stat undercount on first
/// bit-flip.
pub fn frames_with_resync(bytes: &[u8]) -> Frames<'_> {
    Frames {
        buf: bytes,
        cursor: 0,
        done: false,
        resync: true,
    }
}

/// Scan `buf[start..]` for the next plausible 11-bit MPEG audio
/// syncword (top 11 bits = `0x7FF`, i.e. `buf[i] == 0xFF` and
/// `(buf[i+1] & 0xE0) == 0xE0`). Returns the absolute position of the
/// first candidate, or `None` if no candidate exists before
/// `buf.len() - 1`.
///
/// Note: this matches the byte-level sync only — the version_id /
/// layer / bitrate fields are validated downstream by `parse_header`.
/// Resync may therefore land on a false positive that re-fails parse;
/// the next `next()` call will simply re-resync from `cursor + 1`.
fn find_next_sync(buf: &[u8], start: usize) -> Option<usize> {
    crate::codec::framing::scan_for_sync(buf, start, |b0, b1| b0 == 0xFF && (b1 & 0xE0) == 0xE0)
}

/// `Iterator::next` implementation for [`Frames`], called from the model module.
pub(super) fn frames_next<'a>(it: &mut Frames<'a>) -> Option<Result<Frame<'a>, CodecParseError>> {
    if it.done {
        return None;
    }
    if it.cursor >= it.buf.len() {
        it.done = true;
        return None;
    }
    let remaining = &it.buf[it.cursor..];
    let header = match parse_header(remaining) {
        Ok(h) => h,
        Err(e) => {
            if it.resync {
                // Advance cursor to next plausible syncword (or
                // to end-of-buffer to terminate on subsequent call).
                match find_next_sync(it.buf, it.cursor + 1) {
                    Some(next) => it.cursor = next,
                    None => it.done = true,
                }
            } else {
                it.done = true;
            }
            return Some(Err(e));
        }
    };
    let len = header.frame_length_bytes as usize;
    if remaining.len() < len {
        if it.resync {
            match find_next_sync(it.buf, it.cursor + 1) {
                Some(next) => it.cursor = next,
                None => it.done = true,
            }
        } else {
            it.done = true;
        }
        return Some(Err(CodecParseError::Truncated {
            needed: header.frame_length_bytes,
            had: remaining.len() as u32,
        }));
    }
    let body = &remaining[..len];
    let frame = Frame {
        layer: header.layer,
        version: header.version,
        bitrate_kbps: header.bitrate_kbps,
        sample_rate_hz: header.sample_rate_hz,
        channel_mode: header.channel_mode,
        channels: header.channels,
        frame_length_bytes: header.frame_length_bytes,
        samples_per_frame: header.samples_per_frame,
        has_crc: header.has_crc,
        raw_header: header.raw,
        body,
    };
    it.cursor += len;
    Some(Ok(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_v1l1_index1_is_32() {
        assert_eq!(decode_bitrate(Version::Mpeg1, Layer::I, 1).unwrap(), 32);
    }
    #[test]
    fn bitrate_v1l3_index9_is_128() {
        assert_eq!(decode_bitrate(Version::Mpeg1, Layer::III, 9).unwrap(), 128);
    }
    #[test]
    fn bitrate_v2l3_index12_is_128() {
        // V2/V2.5 Layer II/III shares a column.
        assert_eq!(decode_bitrate(Version::Mpeg2, Layer::III, 12).unwrap(), 128);
    }
    #[test]
    fn bitrate_index0_is_free_format_rejected() {
        // bitrate_index=0 surfaces as the distinct
        // `UnsupportedFreeFormat` variant (not `ReservedValue`) so callers
        // can tell "spec defines this but we don't decode it" apart from
        // "spec leaves this for future use".
        let err = decode_bitrate(Version::Mpeg1, Layer::I, 0).unwrap_err();
        assert!(matches!(
            err,
            CodecParseError::UnsupportedFreeFormat { layer: 1 }
        ));
        let err2 = decode_bitrate(Version::Mpeg2, Layer::III, 0).unwrap_err();
        assert!(matches!(
            err2,
            CodecParseError::UnsupportedFreeFormat { layer: 3 }
        ));
    }
    #[test]
    fn bitrate_index15_is_forbidden() {
        let err = decode_bitrate(Version::Mpeg1, Layer::I, 15).unwrap_err();
        assert!(matches!(err, CodecParseError::Forbidden { field } if field == "bitrate_index"));
    }
    #[test]
    fn sample_rate_v1_index0_is_44100() {
        assert_eq!(decode_sample_rate(Version::Mpeg1, 0).unwrap(), 44100);
    }
    #[test]
    fn sample_rate_v2_index0_is_22050() {
        assert_eq!(decode_sample_rate(Version::Mpeg2, 0).unwrap(), 22050);
    }
    #[test]
    fn sample_rate_v25_index0_is_11025() {
        assert_eq!(decode_sample_rate(Version::Mpeg2_5, 0).unwrap(), 11025);
    }
    #[test]
    fn sample_rate_index3_is_reserved() {
        let err = decode_sample_rate(Version::Mpeg1, 3).unwrap_err();
        assert!(
            matches!(err, CodecParseError::ReservedValue { field, value: 3 } if field == "sample_rate_index")
        );
    }
    #[test]
    fn channel_mode_decoded() {
        assert_eq!(decode_channel_mode(0b00), ChannelMode::Stereo);
        assert_eq!(decode_channel_mode(0b01), ChannelMode::JointStereo);
        assert_eq!(decode_channel_mode(0b10), ChannelMode::DualChannel);
        assert_eq!(decode_channel_mode(0b11), ChannelMode::Mono);
    }
    #[test]
    fn channels_count_per_mode() {
        assert_eq!(channels_for_mode(ChannelMode::Stereo), 2);
        assert_eq!(channels_for_mode(ChannelMode::JointStereo), 2);
        assert_eq!(channels_for_mode(ChannelMode::DualChannel), 2);
        assert_eq!(channels_for_mode(ChannelMode::Mono), 1);
    }
    #[test]
    fn samples_per_frame_layer1() {
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::I), 384);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::I), 384);
    }
    #[test]
    fn samples_per_frame_layer2() {
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::II), 1152);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::II), 1152);
    }
    #[test]
    fn samples_per_frame_layer3_v1_is_1152_v2_is_576() {
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::III), 1152);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::III), 576);
        assert_eq!(samples_per_frame(Version::Mpeg2_5, Layer::III), 576);
    }

    #[test]
    fn frame_length_v1l1_128kbps_44100_no_padding_is_136() {
        // Layer I frame length: (12 * bitrate / sample_rate + padding) * 4
        // (12 * 128000 / 44100 + 0) * 4 = 34*4 = 136 bytes.
        assert_eq!(
            frame_length(Layer::I, Version::Mpeg1, 128, 44100, false),
            136
        );
    }
    #[test]
    fn frame_length_v1l1_padding_adds_4_bytes() {
        assert_eq!(
            frame_length(Layer::I, Version::Mpeg1, 128, 44100, true),
            140
        );
    }
    #[test]
    fn frame_length_v1l3_128kbps_44100_no_padding_is_417() {
        // Layer III frame length: 144 * bitrate / sample_rate + padding
        // 144 * 128000 / 44100 = 417 (truncated)
        assert_eq!(
            frame_length(Layer::III, Version::Mpeg1, 128, 44100, false),
            417
        );
    }
    #[test]
    fn frame_length_v1l3_padding_adds_1_byte() {
        assert_eq!(
            frame_length(Layer::III, Version::Mpeg1, 128, 44100, true),
            418
        );
    }
    #[test]
    fn frame_length_v2l3_64kbps_22050_is_208() {
        // V2 Layer III: 72 * 64000 / 22050 = 208 (truncated)
        assert_eq!(
            frame_length(Layer::III, Version::Mpeg2, 64, 22050, false),
            208
        );
    }

    /// V1 Layer III, 128 kbps, 44.1 kHz, joint stereo, no padding, no CRC.
    /// Header bits:
    ///   syncword:        0xFFF
    ///   version_id:      0b11 (MPEG-1)
    ///   layer_desc:      0b01 (Layer III; bits inverted: 01 = III)
    ///   protection_bit:  1 (no CRC)
    ///   bitrate_index:   0b1001 (9 = 128 kbps)
    ///   sample_rate_idx: 0b00 (44100 Hz)
    ///   padding:         0
    ///   private:         0
    ///   channel_mode:    0b01 (joint stereo) — byte 3 bits 7-6
    ///   mode_ext:        0b00
    ///   copyright:       0
    ///   original:        0
    ///   emphasis:        0b00
    /// Byte layout: FF  FB  90  40
    ///              [0] [1] [2] [3]
    const V1L3_128K_44100_JS: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];

    #[test]
    fn parse_header_v1l3_128k_44100_js() {
        let h = parse_header(&V1L3_128K_44100_JS).unwrap();
        assert_eq!(h.version, Version::Mpeg1);
        assert_eq!(h.layer, Layer::III);
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate_hz, 44100);
        assert_eq!(h.channel_mode, ChannelMode::JointStereo);
        assert_eq!(h.channels, 2);
        assert!(!h.has_crc);
        assert_eq!(h.frame_length_bytes, 417);
        assert_eq!(h.samples_per_frame, 1152);
    }

    #[test]
    fn parse_header_too_short_yields_truncated() {
        let err = parse_header(&[0xFF, 0xFB]).unwrap_err();
        assert!(matches!(
            err,
            CodecParseError::Truncated { needed: 4, had: 2 }
        ));
    }

    #[test]
    fn parse_header_bad_sync_yields_bad_sync_word() {
        let err = parse_header(&[0xAB, 0xCD, 0x00, 0x00]).unwrap_err();
        assert!(matches!(err, CodecParseError::BadSyncWord { .. }));
    }

    #[test]
    fn frames_empty_yields_none() {
        let mut it = frames(&[]);
        assert!(it.next().is_none());
    }

    #[test]
    fn frames_short_yields_truncated_then_none() {
        let mut it = frames(&[0xFF]);
        match it.next() {
            Some(Err(CodecParseError::Truncated { .. })) => {}
            other => panic!("expected Err(Truncated), got {:?}", other),
        }
        assert!(it.next().is_none(), "iterator should end after first error");
    }

    #[test]
    fn frames_two_back_to_back() {
        // Build two V1L3 128k 44.1k frames (417 bytes each, no padding).
        let header: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];
        let mut buf = Vec::with_capacity(2 * 417);
        for _ in 0..2 {
            buf.extend_from_slice(&header);
            buf.extend(core::iter::repeat(0u8).take(417 - 4));
        }
        let mut it = frames(&buf);
        let f1 = it.next().unwrap().unwrap();
        assert_eq!(f1.frame_length_bytes, 417);
        assert_eq!(f1.bytes().len(), 417);
        let f2 = it.next().unwrap().unwrap();
        assert_eq!(f2.frame_length_bytes, 417);
        assert!(it.next().is_none());
    }

    #[test]
    fn frames_truncated_body_yields_truncated() {
        // 4-byte header says 417 but we only give 100 bytes total.
        let header: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];
        let mut buf = Vec::from(header);
        buf.extend(core::iter::repeat(0u8).take(96));
        let mut it = frames(&buf);
        match it.next() {
            Some(Err(CodecParseError::Truncated { .. })) => {}
            other => panic!("expected Err(Truncated), got {:?}", other),
        }
        assert!(it.next().is_none());
    }

    /// The strict iterator terminates after the first parse error,
    /// dropping every subsequent valid frame.
    #[test]
    fn strict_iterator_drops_frames_after_first_corruption() {
        // Layout: [corrupted 4 bytes that fail parse_header] [valid frame].
        // Use a buffer where the first bytes are not a valid sync but
        // contain 0xFF later, so a strict iter fails immediately.
        let mut buf = vec![0x00, 0x00, 0x00, 0x00];
        let header: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];
        buf.extend_from_slice(&header);
        buf.extend(core::iter::repeat(0u8).take(417 - 4));

        let mut it = frames(&buf);
        match it.next() {
            Some(Err(CodecParseError::BadSyncWord { .. })) => {}
            other => panic!("expected BadSyncWord, got {:?}", other),
        }
        assert!(
            it.next().is_none(),
            "strict iterator must terminate after first error"
        );
    }

    /// The resync iterator yields the error then resumes from the next
    /// plausible syncword, recovering the valid frame at position N+M.
    #[test]
    fn resync_iterator_recovers_valid_frame_after_corruption() {
        // Same layout as the strict-iterator failing-test above, but
        // confirm the resync variant yields Err + then the valid frame.
        let mut buf = vec![0x00, 0x00, 0x00, 0x00];
        let header: [u8; 4] = [0xFF, 0xFB, 0x90, 0x40];
        buf.extend_from_slice(&header);
        buf.extend(core::iter::repeat(0u8).take(417 - 4));

        let mut it = frames_with_resync(&buf);

        // First call: parse_header on [0x00, 0x00, 0x00, 0x00] fails
        // BadSyncWord; resync scans forward and finds the valid 0xFF
        // 0xFB 0x90 0x40 syncword at byte 4.
        match it.next() {
            Some(Err(CodecParseError::BadSyncWord { .. })) => {}
            other => panic!("expected BadSyncWord, got {:?}", other),
        }

        // Second call: parses cleanly from the recovered cursor.
        let f = it.next().unwrap().unwrap();
        assert_eq!(f.frame_length_bytes, 417);
        assert_eq!(f.bitrate_kbps, 128);

        assert!(it.next().is_none());
    }

    /// A resync iterator over a buffer with no plausible syncword
    /// anywhere terminates after yielding the initial error (no infinite
    /// loop, no spurious extra yields).
    #[test]
    fn resync_iterator_no_syncword_terminates() {
        // 32 bytes of zero — no 0xFF prefix anywhere.
        let buf = vec![0x00u8; 32];
        let mut it = frames_with_resync(&buf);

        // The first byte is 0x00 so parse_header fails BadSyncWord.
        match it.next() {
            Some(Err(CodecParseError::BadSyncWord { .. })) => {}
            other => panic!("expected BadSyncWord, got {:?}", other),
        }

        // No further syncword exists; iterator must terminate.
        assert!(
            it.next().is_none(),
            "resync iterator must terminate when no sync found"
        );
    }

    /// Malformed-MIDDLE-frame regression test.
    ///
    /// Layout: [valid frame] [8 bytes of garbage that fail parse_header]
    /// [valid frame]. Strict `frames()` parses the first valid frame, then
    /// fails on the garbage and terminates — yielding only 1 valid frame
    /// (this is the stats-undercount bug). `frames_with_resync()` recovers
    /// past the garbage to the second syncword and yields 2 valid frames.
    ///
    /// This exercises the iterator the mpegts demux + mux audio stats sites
    /// use (`frames_with_resync`, not `frames`).
    #[test]
    fn frames_with_resync_recovers_past_middle_garbage_two_valid_frames() {
        let header: [u8; 4] = V1L3_128K_44100_JS;
        let mut buf = Vec::with_capacity(417 + 8 + 417);
        // Frame 1: full 417-byte V1L3 128k frame.
        buf.extend_from_slice(&header);
        buf.extend(core::iter::repeat(0u8).take(417 - 4));
        // 8 bytes of garbage between frames — no 0xFF, so parse_header
        // fails BadSyncWord at cursor=417 and resync must scan past these.
        buf.extend_from_slice(&[0x00u8; 8]);
        // Frame 2: another full 417-byte V1L3 128k frame.
        buf.extend_from_slice(&header);
        buf.extend(core::iter::repeat(0u8).take(417 - 4));

        // Strict iterator: gets the first valid frame, then terminates on
        // the garbage — undercount.
        let strict_count = frames(&buf).filter_map(Result::ok).count();
        assert_eq!(
            strict_count, 1,
            "strict iterator must terminate on middle garbage (undercount)"
        );

        // Resync iterator: skips past the garbage and finds the second
        // valid syncword, recovering both frames.
        let resync_count = frames_with_resync(&buf).filter_map(Result::ok).count();
        assert_eq!(
            resync_count, 2,
            "resync iterator must recover the second valid frame after middle garbage"
        );
    }

    /// Free-format MPEG audio (bitrate_index == 0) surfaces as the
    /// distinct `UnsupportedFreeFormat` error from the frame iterator
    /// (not `ReservedValue`).
    #[test]
    fn frames_free_format_yields_unsupported_free_format() {
        // V1L3 header but bitrate_index = 0 (free format):
        //   FF FB 0X XX  where bitrate_index nibble = 0
        // Original V1L3 128k header was FF FB 90 40; flip bitrate nibble to 0.
        let header: [u8; 4] = [0xFF, 0xFB, 0x00, 0x40];
        let mut it = frames(&header);
        match it.next() {
            Some(Err(CodecParseError::UnsupportedFreeFormat { layer: 3 })) => {}
            other => panic!("expected UnsupportedFreeFormat(layer=3), got {:?}", other),
        }
    }

    /// `find_next_sync` returns None when no candidate exists.
    #[test]
    fn find_next_sync_no_candidate() {
        assert_eq!(find_next_sync(&[], 0), None);
        assert_eq!(find_next_sync(&[0xFF], 0), None); // single 0xFF — no room for second byte
        assert_eq!(find_next_sync(&[0x00, 0x00, 0x00], 0), None);
        // 0xFF present but second byte's top 3 bits != 0b111
        assert_eq!(find_next_sync(&[0xFF, 0x00], 0), None);
    }

    /// `find_next_sync` locates the first plausible syncword.
    #[test]
    fn find_next_sync_finds_candidate() {
        // 0xFF at index 2, 0xE0 at index 3 — valid 11-bit sync.
        let buf = [0x00, 0x11, 0xFF, 0xE0, 0xAA];
        assert_eq!(find_next_sync(&buf, 0), Some(2));
        // Start search past the candidate — must return None.
        assert_eq!(find_next_sync(&buf, 3), None);
    }

    #[test]
    fn mpegaudio_frame_owned_roundtrip() {
        use crate::codec::mpegaudio::{ChannelMode, Frame, Layer, Version};
        let payload = vec![0xFF, 0xFB, 0x90, 0x40, 0x01, 0x02, 0x03];
        let borrowed = Frame {
            layer: Layer::III,
            version: Version::Mpeg1,
            bitrate_kbps: 128,
            sample_rate_hz: 44100,
            channel_mode: ChannelMode::JointStereo,
            channels: 2,
            frame_length_bytes: 417,
            samples_per_frame: 1152,
            has_crc: false,
            raw_header: [0xFF, 0xFB, 0x90, 0x40],
            body: &payload,
        };
        let owned = borrowed.to_owned();
        assert_eq!(borrowed, owned.as_ref());
        // Verify the body was actually copied into owned storage.
        assert_eq!(owned.body, payload);
    }
}
