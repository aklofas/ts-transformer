//! Minimal LATM/LOAS sync validator.
//!
//! Spec: ISO/IEC 14496-3 §1.7 (LATM / LOAS framing) — referenced by
//! ITU-T H.222.0 Table 2-34 stream_type `0x11` (`AudioAacLatm`).
//!
//! ## Scope
//!
//! AAC-LATM PES payloads are sequences of LOAS-framed `audioMuxElement`
//! records. Each record begins with a 24-bit LOAS sync header:
//!
//! ```text
//!   syncword:           11 bits = 0x2B7
//!   audioMuxLengthBytes: 13 bits
//! ```
//!
//! On the wire the first two bytes match the pattern
//! `[0x56, 0xE0 .. 0xFF]` (sync = `0x2B7` shifted left 5 bits):
//!
//! ```text
//!   bytes[0] = 0x56                 (0x2B7 >> 3)
//!   bytes[1] = 0b111x_xxxx          (top 3 bits = remaining sync;
//!                                    low 5 bits = top of length)
//! ```
//!
//! This module only validates the LOAS sync word and length-fits check.
//! Full `audioMuxElement` decode (per ISO/IEC 14496-3 §1.7.3) is deferred
//! — consumers needing AudioSpecificConfig + raw_data_block walks must
//! integrate a downstream decoder (e.g. ffmpeg).
//!
//! ## Rationale
//!
//! Without sync validation, a demuxer that advertises `stream_type=0x11`
//! (AAC-LATM) lets malformed LATM streams (truncated PES, wrong syncword,
//! audio shipped without LATM wrapping on a 0x11 PID) silently produce
//! `Sample` events with garbage payload. Downstream decoders then report
//! cryptic parse errors that don't correlate back to the
//! conformance bug. This validator surfaces the framing violation as
//! [`crate::mpegts::demux::NonConformantIssue::LatmFraming`].
//!
//! ## Lenient vs. strict LATM validation
//!
//! Lenient mode (`StrictMode::Off`): the demuxer surfaces the issue as
//! a `NonConformant` event alongside the `Sample` event (permissive —
//! consumers may still want the bytes for forensic analysis).
//! Strict mode (`StrictMode::Full`): the `Sample` event is suppressed
//! and the issue propagates as `DemuxError::StrictRejection`.

/// First wire byte of the 11-bit LOAS syncword (`0x2B7 >> 3`).
const LOAS_SYNC_BYTE0: u8 = 0x56;
/// Top 3 bits of byte 1 must equal `0b111` (low 3 bits of the 11-bit
/// `0x2B7` sync, shifted into the top of byte 1).
const LOAS_SYNC_BYTE1_MASK: u8 = 0b1110_0000;
const LOAS_SYNC_BYTE1_VALUE: u8 = 0b1110_0000;

/// Specific LATM/LOAS framing violation detected in a PES payload.
///
/// `#[non_exhaustive]` — future violations (e.g. an
/// `AudioMuxElementMalformed` variant once full decode lands) can be
/// added without breaking matchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LatmFramingKind {
    /// The PES payload does not begin with a valid LOAS syncword.
    /// Either an off-by-one offset (some encoders prepend a `0x00`
    /// byte before the sync), the wrong codec entirely shipped on a
    /// `stream_type=0x11` PID, or a corrupt PES boundary.
    MissingSyncword,
    /// The LOAS header parsed but its declared `audioMuxLengthBytes`
    /// runs past the end of the PES payload.
    ///
    /// [`validate_latm_sync`] still returns it. The demuxer does not report
    /// it: ISO/IEC 14496-3 §1.7.2 lets an AudioSyncStream record continue
    /// into the next PES, so a record running past its PES is not a fault.
    AudioMuxLengthOverrun,
    /// PES payload is shorter than the 3-byte LOAS header — cannot
    /// validate the sync word or read the length field.
    ///
    /// [`validate_latm_sync`] still returns it. The demuxer does not report
    /// it: a LOAS header may be split across PES (ISO/IEC 14496-3 §1.7.2).
    Truncated,
}

/// Validate the LATM/LOAS sync header at the start of an AAC-LATM PES
/// payload.
///
/// Returns `Ok(audio_mux_length_bytes)` on a valid sync — the value is
/// the declared payload length of the first `audioMuxElement` (excluding
/// the 3-byte LOAS header itself), useful for callers that want to walk
/// subsequent records.
///
/// On failure returns the specific [`LatmFramingKind`] so the caller can
/// route to the appropriate `NonConformantIssue` variant.
///
/// # Spec reference
///
/// - ISO/IEC 14496-3 §1.7.2 — LOAS syncword + length.
/// - H.222.0 Table 2-34 — stream_type 0x11 binding to AAC-LATM.
pub fn validate_latm_sync(pes_payload: &[u8]) -> Result<u16, LatmFramingKind> {
    // The 3-byte LOAS header carries the sync word and 13-bit length.
    if pes_payload.len() < 3 {
        return Err(LatmFramingKind::Truncated);
    }
    let b0 = pes_payload[0];
    let b1 = pes_payload[1];
    let b2 = pes_payload[2];
    if b0 != LOAS_SYNC_BYTE0 || (b1 & LOAS_SYNC_BYTE1_MASK) != LOAS_SYNC_BYTE1_VALUE {
        return Err(LatmFramingKind::MissingSyncword);
    }
    // audioMuxLengthBytes: low 5 bits of byte 1 + all 8 bits of byte 2
    // = 13-bit big-endian value.
    let audio_mux_length_bytes: u16 = ((u16::from(b1 & 0x1F)) << 8) | u16::from(b2);
    let total_record_len = 3usize + usize::from(audio_mux_length_bytes);
    if total_record_len > pes_payload.len() {
        return Err(LatmFramingKind::AudioMuxLengthOverrun);
    }
    Ok(audio_mux_length_bytes)
}

/// The longest LOAS record on the wire: the 3-byte header plus a 13-bit
/// `audioMuxLengthBytes` of at most `0x1FFF`.
pub(crate) const MAX_LOAS_RECORD_BYTES: usize = 3 + 0x1FFF;

/// Offset of the first byte-aligned LOAS syncword candidate in `payload`,
/// searched only within its first [`MAX_LOAS_RECORD_BYTES`] bytes: the tail
/// of a record carried in from an earlier PES is never longer than that.
pub(crate) fn find_loas_sync(payload: &[u8]) -> Option<usize> {
    payload
        .windows(2)
        .take(MAX_LOAS_RECORD_BYTES)
        .position(starts_with_loas_sync)
}

/// `true` when `bytes` begins with the 11-bit LOAS syncword `0x2B7`.
pub(crate) fn starts_with_loas_sync(bytes: &[u8]) -> bool {
    matches!(bytes, [b0, b1, ..]
        if *b0 == LOAS_SYNC_BYTE0 && (*b1 & LOAS_SYNC_BYTE1_MASK) == LOAS_SYNC_BYTE1_VALUE)
}

/// Walk the LOAS records (ISO/IEC 14496-3 §1.7.2 AudioSyncStream) of one PES
/// payload, starting at the record boundary `start`.
///
/// - `Ok(Some(n))`: the last record continues into the next PES for `n`
///   more bytes.
/// - `Ok(None)`: the walk ended on a record boundary, or on a 3-byte header
///   split across PES; the next PES's position in the stream is unknown.
/// - `Err(MissingSyncword)`: a record boundary lacks the syncword.
///
/// Every record advances at least 3 bytes, so the walk is bounded by the
/// payload length; `n` is at most one record (`3 + 0x1FFF` bytes).
pub(crate) fn walk_loas(payload: &[u8], start: usize) -> Result<Option<usize>, LatmFramingKind> {
    let mut pos = start;
    while let Some(rest) = payload.get(pos..).filter(|r| !r.is_empty()) {
        if rest.len() < 3 {
            return Ok(None);
        }
        if !starts_with_loas_sync(rest) {
            return Err(LatmFramingKind::MissingSyncword);
        }
        let audio_mux_length_bytes = (usize::from(rest[1] & 0x1F) << 8) | usize::from(rest[2]);
        pos += 3 + audio_mux_length_bytes;
    }
    Ok(pos.checked_sub(payload.len()).filter(|&n| n > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_prelude::*;

    /// Helper: build a valid LOAS-framed `audioMuxElement` of `len` body
    /// bytes (zero-filled). Returns the wire bytes (3-byte header + len
    /// zero bytes).
    fn build_loas_record(len: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + usize::from(len));
        out.push(LOAS_SYNC_BYTE0);
        // top 3 bits = sync, low 5 bits = high 5 bits of length
        out.push(LOAS_SYNC_BYTE1_VALUE | ((len >> 8) as u8 & 0x1F));
        out.push((len & 0xFF) as u8);
        out.resize(3 + usize::from(len), 0);
        out
    }

    #[test]
    fn walk_loas_reports_the_bytes_a_split_record_still_owes() {
        let mut buf = build_loas_record(10);
        buf.extend_from_slice(&build_loas_record(50)[..20]);
        assert_eq!(walk_loas(&buf, 0), Ok(Some(33)));
    }

    #[test]
    fn walk_loas_flags_a_missing_sync_at_a_record_boundary() {
        let mut buf = build_loas_record(10);
        buf.extend_from_slice(&[0x00, 0x11, 0x22, 0x33]);
        assert_eq!(walk_loas(&buf, 0), Err(LatmFramingKind::MissingSyncword));
    }

    #[test]
    fn walk_loas_ending_on_a_boundary_or_a_split_header_carries_nothing() {
        let mut buf = build_loas_record(10);
        assert_eq!(walk_loas(&buf, 0), Ok(None));
        buf.extend_from_slice(&[0x56, 0xE0]);
        assert_eq!(walk_loas(&buf, 0), Ok(None));
    }

    #[test]
    fn find_loas_sync_looks_only_within_one_record() {
        let mut buf = vec![0x00; MAX_LOAS_RECORD_BYTES - 1];
        buf.extend_from_slice(&build_loas_record(0));
        assert_eq!(find_loas_sync(&buf), Some(MAX_LOAS_RECORD_BYTES - 1));
        buf.insert(0, 0x00);
        assert_eq!(find_loas_sync(&buf), None);
    }

    #[test]
    fn walk_loas_starts_after_the_carried_bytes() {
        let mut buf = vec![0xAA; 5];
        buf.extend_from_slice(&build_loas_record(8));
        assert_eq!(walk_loas(&buf, 5), Ok(None));
        assert_eq!(walk_loas(&buf, 0), Err(LatmFramingKind::MissingSyncword));
    }

    #[test]
    fn valid_loas_sync_returns_length() {
        let buf = build_loas_record(100);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 100);
    }

    #[test]
    fn empty_payload_yields_truncated() {
        assert_eq!(validate_latm_sync(&[]), Err(LatmFramingKind::Truncated));
    }

    #[test]
    fn two_byte_payload_yields_truncated() {
        // Need 3 bytes minimum to read sync + length.
        assert_eq!(
            validate_latm_sync(&[0x56, 0xE0]),
            Err(LatmFramingKind::Truncated)
        );
    }

    /// Primary lenient-mode test: PES that does not begin with the
    /// LOAS syncword on a `stream_type=0x11` PID is non-conformant.
    #[test]
    fn missing_syncword_returns_missing_syncword() {
        // Plausible ADTS sync (0xFFF) — common confusion: ADTS-framed
        // AAC mistakenly shipped on a LATM-advertising PID.
        let bytes = [0xFF, 0xF1, 0x4C, 0x80, 0x00, 0x00];
        assert_eq!(
            validate_latm_sync(&bytes),
            Err(LatmFramingKind::MissingSyncword)
        );
    }

    #[test]
    fn wrong_byte1_top_bits_yields_missing_syncword() {
        // Byte 0 matches but byte 1 has top 3 bits != 0b111.
        let bytes = [0x56, 0x40, 0x00, 0x00, 0x00];
        assert_eq!(
            validate_latm_sync(&bytes),
            Err(LatmFramingKind::MissingSyncword)
        );
    }

    #[test]
    fn audio_mux_length_overrun_returns_overrun() {
        // Declare 200 bytes but only ship 50 in the buffer.
        let mut buf = build_loas_record(50);
        // Patch the length to 200 (overflow vs 50-byte body).
        buf[1] = LOAS_SYNC_BYTE1_VALUE | ((200u16 >> 8) as u8);
        buf[2] = (200u16 & 0xFF) as u8;
        assert_eq!(
            validate_latm_sync(&buf),
            Err(LatmFramingKind::AudioMuxLengthOverrun)
        );
    }

    #[test]
    fn zero_length_record_is_valid() {
        // Edge case: audioMuxLengthBytes == 0. Spec doesn't forbid this
        // (a zero-byte audioMuxElement is unusual but parseable as a
        // structural matter); the validator only checks sync + length-fits.
        let buf = build_loas_record(0);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 0);
    }

    #[test]
    fn max_length_record_is_valid() {
        // 13-bit length field: 0x1FFF = 8191.
        let buf = build_loas_record(8191);
        assert_eq!(validate_latm_sync(&buf).unwrap(), 8191);
    }
}
