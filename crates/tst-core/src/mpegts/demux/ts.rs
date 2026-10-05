//! TS-packet unpacker — parse a single 188-byte transport-stream packet.
//!
//! Inverse of `mpegts::mux::ts::TsPacketWriter`. ISO/IEC 13818-1 §2.4.3.2.

use crate::mpegts::common::{TS_PACKET_SIZE, TS_SYNC_BYTE};

/// Why a TS packet's adaptation-field control/length combination violated
/// ITU-T H.222.0 §2.4.3.2 / §2.4.3.5. Surfaced on
/// `TsPacket::adaptation_malformed` and, by the demuxer, as
/// [`NonConformantIssue::AdaptationFieldMalformed`](crate::mpegts::demux::NonConformantIssue::AdaptationFieldMalformed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdaptationFieldKind {
    /// `adaptation_field_control == 0b00` — reserved; H.222.0 §2.4.3.2
    /// requires decoders to discard the packet.
    ReservedControl,
    /// `adaptation_field_length` is wrong for the control value: control
    /// `0b10` (adaptation only) requires exactly `183`; control `0b11`
    /// (adaptation + payload) requires `0..=182`. Also covers a length
    /// that runs past the 188-byte packet.
    BadLengthForControl,
    /// `pcr_flag = 1` but the adaptation field held fewer than the six
    /// bytes required for a PCR (H.222.0 §2.4.3.5).
    ShortPcr,
}

/// Why a PCR field decoded from the adaptation field violated ITU-T
/// H.222.0 §2.4.3.5. Surfaced on `TsPacket::pcr_malformed` when the
/// reserved bits or extension range fail the on-wire conformance checks.
///
/// Lenient mode (`StrictMode::Off`): the malformed PCR is dropped (set
/// `pcr_27mhz = None`) and a `NonConformantIssue::PcrMalformed` event is
/// queued so downstream observers can correlate timing anomalies with the
/// underlying syntax violation. Strict mode rejects the issue per
/// `StrictMode::rejects` (treated as a timing-class anomaly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PcrMalformedKind {
    /// Six reserved bits in byte 4 of the PCR field (mask `0x7E`) were not
    /// all set to 1. Per H.222.0 §2.4.3.5 Table 2-7, these bits are
    /// `reserved` and shall be 1 per §2.2 ("Reserved").
    InvalidReservedBits,
    /// `program_clock_reference_extension` decoded to a value outside the
    /// allowed range [0, 299]. Per H.222.0 §2.4.3.5 the extension counts
    /// 27 MHz ticks within a single 90 kHz tick and therefore has a
    /// maximum value of 299 (300 ticks per 90 kHz period).
    ExtensionOutOfRange,
}

/// Parsed fields from one TS packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsPacket<'a> {
    pub pid: u16,
    /// `transport_error_indicator` per ISO/IEC 13818-1 §2.4.3.2 (bit 0x80 of
    /// byte 1). Set when an upstream link-layer (e.g. ATSC FEC, satellite
    /// demod, CMTS) flagged the packet as known-corrupt. The demuxer drops
    /// these packets per ffmpeg `mpegts.c:3091-3097`.
    pub transport_error_indicator: bool,
    /// `transport_scrambling_control` per ISO/IEC 13818-1 §2.4.3.2 (bits
    /// 7-6 of byte 3). `0b00` = not scrambled; `0b01`/`0b10`/`0b11` =
    /// user-defined scrambling modes. The library does not descramble:
    /// the demuxer drops any packet with a non-zero value (does NOT route
    /// the payload to PSI/PES) and surfaces
    /// `NonConformantIssue::UnsupportedScrambling`.
    pub transport_scrambling_control: u8,
    pub payload_unit_start: bool,
    pub continuity_counter: u8,
    pub has_adaptation_field: bool,
    /// 27 MHz PCR value if the adaptation field carried a syntactically
    /// conformant PCR. `None` when no PCR was present OR the PCR field
    /// failed the H.222.0 §2.4.3.5 reserved-bits / extension-range checks
    /// (in which case [`Self::pcr_malformed`] is populated).
    pub pcr_27mhz: Option<u64>,
    /// Set when `pcr_flag = 1` and the PCR field was syntactically
    /// non-conformant per H.222.0 §2.4.3.5. The demuxer
    /// surfaces this as `NonConformantIssue::PcrMalformed`.
    pub pcr_malformed: Option<PcrMalformedKind>,
    /// Set when the adaptation field's control/length combination violated
    /// H.222.0 §2.4.3.2/§2.4.3.5 (see [`AdaptationFieldKind`]). The demuxer
    /// surfaces this as `NonConformantIssue::AdaptationFieldMalformed`.
    /// `None` when the adaptation field (if any) is well-formed.
    pub adaptation_malformed: Option<AdaptationFieldKind>,
    /// Adaptation field's `discontinuity_indicator` flag.
    pub discontinuity_indicator: bool,
    /// Adaptation field's `random_access_indicator` flag, per ISO/IEC
    /// 13818-1 §2.4.3.4 flags byte bit 6 (0x40). Set by encoders and
    /// muxers on TS packets that begin an access unit decodable without
    /// information from previous AUs (IDR, CRA, etc.). The signal is
    /// independent of NAL-level type and reflects the stream-level
    /// random-access contract. False when no adaptation field is present.
    pub random_access_indicator: bool,
    /// Slice into the input bytes pointing at the payload (post-adaptation,
    /// post-pointer-field if PSI). Empty if `has_payload=false`.
    pub payload: &'a [u8],
    pub has_payload: bool,
    /// The full 188-byte packet this struct was parsed from. Retained so
    /// duplicate detection (H.222.0 §2.4.3.3) can byte-compare a same-CC
    /// packet against its predecessor without re-deriving the raw bytes.
    pub raw: &'a [u8; 188],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TsParseError {
    NoSyncByte,
    Truncated,
    /// Adaptation field claimed length that doesn't fit in the packet.
    BadAdaptationLength,
}

pub fn parse_ts_packet(buf: &[u8]) -> Result<TsPacket<'_>, TsParseError> {
    let raw: &[u8; TS_PACKET_SIZE] = match buf.try_into() {
        Ok(raw) => raw,
        Err(_) => return Err(TsParseError::Truncated),
    };
    if buf[0] != TS_SYNC_BYTE {
        return Err(TsParseError::NoSyncByte);
    }
    let transport_error_indicator = (buf[1] & 0x80) != 0;
    let payload_unit_start = (buf[1] & 0x40) != 0;
    let pid = u16::from_be_bytes([buf[1] & 0x1F, buf[2]]);
    let transport_scrambling_control = (buf[3] >> 6) & 0x03;
    let adaptation_control = (buf[3] >> 4) & 0x03;
    let has_adaptation_field = adaptation_control & 0b10 != 0;
    let has_payload = adaptation_control & 0b01 != 0;
    let continuity_counter = buf[3] & 0x0F;
    let mut payload_off = 4;
    let mut pcr_27mhz = None;
    let mut pcr_malformed = None;
    let mut adaptation_malformed = None;
    let mut discontinuity_indicator = false;
    let mut random_access_indicator = false;
    // H.222.0 §2.4.3.2: adaptation_field_control=00 is reserved; decoders
    // shall discard the packet. Flag it before entering the af block (which
    // has_adaptation_field=false bypasses).
    if adaptation_control == 0b00 {
        adaptation_malformed = Some(AdaptationFieldKind::ReservedControl);
    }
    if has_adaptation_field {
        let af_len = buf[4] as usize;
        if 5 + af_len > TS_PACKET_SIZE {
            return Err(TsParseError::BadAdaptationLength);
        }
        // H.222.0 §2.4.3.2: control 10 (adaptation only) ⇒ length must be
        // exactly 183; control 11 (adaptation + payload) ⇒ length 0..=182.
        if (adaptation_control == 0b10 && af_len != 183)
            || (adaptation_control == 0b11 && af_len > 182)
        {
            adaptation_malformed = Some(AdaptationFieldKind::BadLengthForControl);
        }
        if af_len >= 1 {
            let flags = buf[5];
            discontinuity_indicator = (flags & 0x80) != 0;
            random_access_indicator = (flags & 0x40) != 0;
            let pcr_flag = (flags & 0x10) != 0;
            if pcr_flag && af_len < 7 && adaptation_malformed.is_none() {
                adaptation_malformed = Some(AdaptationFieldKind::ShortPcr);
            }
            if pcr_flag && af_len >= 7 {
                let b = &buf[6..12];
                let base = (((b[0] as u64) << 25)
                    | ((b[1] as u64) << 17)
                    | ((b[2] as u64) << 9)
                    | ((b[3] as u64) << 1)
                    | (((b[4] as u64) >> 7) & 0x01))
                    & ((1u64 << 33) - 1);
                let ext = (((b[4] as u64) & 0x01) << 8) | (b[5] as u64);
                // H.222.0 §2.4.3.5: the six middle bits of byte 4 (mask
                // 0x7E) are `reserved` and shall be 1 per §2.2. ITU-T
                // §2.4.3.5 also caps `program_clock_reference_extension`
                // at 299 (300 ticks per 90 kHz period). Malformed PCRs
                // surface as `NonConformantIssue::PcrMalformed` rather
                // than feeding bogus timing into the anomaly check.
                let reserved_bits = b[4] & 0x7E;
                if reserved_bits != 0x7E {
                    pcr_malformed = Some(PcrMalformedKind::InvalidReservedBits);
                } else if ext > 299 {
                    pcr_malformed = Some(PcrMalformedKind::ExtensionOutOfRange);
                } else {
                    pcr_27mhz = Some(base * 300 + ext);
                }
            }
        }
        payload_off = 5 + af_len;
    }
    let payload = if has_payload && payload_off < TS_PACKET_SIZE {
        &buf[payload_off..TS_PACKET_SIZE]
    } else {
        &[]
    };
    Ok(TsPacket {
        pid,
        transport_error_indicator,
        transport_scrambling_control,
        payload_unit_start,
        continuity_counter,
        has_adaptation_field,
        pcr_27mhz,
        pcr_malformed,
        adaptation_malformed,
        discontinuity_indicator,
        random_access_indicator,
        payload,
        has_payload,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_simple_packet(pid: u16, pusi: bool, cc: u8) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = if pusi { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F);
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x10 | (cc & 0x0F); // adaptation_control=01 (payload only)
        buf
    }

    #[test]
    fn parses_simple_packet() {
        let buf = build_simple_packet(0x100, true, 0xA);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.pid, 0x100);
        assert!(!pkt.transport_error_indicator);
        assert!(pkt.payload_unit_start);
        assert_eq!(pkt.continuity_counter, 0xA);
        assert!(pkt.has_payload);
        assert_eq!(pkt.payload.len(), 184);
    }

    #[test]
    fn rejects_no_sync_byte() {
        let mut buf = build_simple_packet(0x100, false, 0);
        buf[0] = 0x00;
        assert!(matches!(
            parse_ts_packet(&buf),
            Err(TsParseError::NoSyncByte)
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        let buf = [0u8; 100];
        assert!(matches!(
            parse_ts_packet(&buf),
            Err(TsParseError::Truncated)
        ));
    }

    #[test]
    fn adaptation_field_random_access_indicator_extracted() {
        // Build a TS packet with adaptation field flags byte 0x40 (RAI set).
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40 | ((0x100u16 >> 8) as u8 & 0x1F); // pusi=1, pid=0x100
        buf[2] = 0x00;
        buf[3] = 0x30; // adaptation_control=11 (af + payload)
        buf[4] = 1; // adaptation_field_length=1 (flags byte only)
        buf[5] = 0x40; // flags: RAI=1
        let pkt = parse_ts_packet(&buf).unwrap();
        assert!(pkt.random_access_indicator);
        assert!(!pkt.discontinuity_indicator);
    }

    #[test]
    fn adaptation_field_random_access_indicator_clear_when_unset() {
        // Build a TS packet with adaptation field flags byte 0x00.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40;
        buf[2] = 0x00;
        buf[3] = 0x30;
        buf[4] = 1;
        buf[5] = 0x00; // flags: all clear
        let pkt = parse_ts_packet(&buf).unwrap();
        assert!(!pkt.random_access_indicator);
    }

    #[test]
    fn no_adaptation_field_means_rai_false() {
        // Default-built packet: adaptation_control=01, no af present.
        let buf = build_simple_packet(0x100, true, 0);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert!(!pkt.random_access_indicator);
    }

    /// Build a TS packet carrying a PCR via the adaptation field. `base`
    /// is the 33-bit base value and `ext` is the 9-bit extension; `bad_reserved`
    /// flips one of the six middle bits in byte 4 (mask 0x7E) to 0 so the
    /// reserved-bits check fails.
    fn build_pcr_packet(base: u64, ext: u16, bad_reserved: bool) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x00; // no PUSI, pid=0x0100 hi nibble
        // PID 0x0100
        buf[1] |= ((0x100u16 >> 8) as u8) & 0x1F;
        buf[2] = 0x00;
        buf[3] = 0x30; // adaptation_control=11 (af + payload), CC=0
        buf[4] = 7; // adaptation_field_length: flags(1) + PCR(6)
        buf[5] = 0x10; // flags: PCR_flag=1
        // Encode PCR: base[32..0] across bytes 6-10 bit 7, ext[8..0] across byte 10 bit 0 and byte 11.
        let b6 = ((base >> 25) & 0xFF) as u8;
        let b7 = ((base >> 17) & 0xFF) as u8;
        let b8 = ((base >> 9) & 0xFF) as u8;
        let b9 = ((base >> 1) & 0xFF) as u8;
        // byte 10: bit 7 = base bit 0; bits 6-1 = reserved (must be 1's); bit 0 = ext bit 8.
        let base_lsb = ((base & 0x01) as u8) << 7;
        let reserved = if bad_reserved { 0x7C } else { 0x7E }; // flip bit 1 of mask 0x7E to 0
        let ext_hi = ((ext >> 8) & 0x01) as u8;
        let b10 = base_lsb | reserved | ext_hi;
        let b11 = (ext & 0xFF) as u8;
        buf[6] = b6;
        buf[7] = b7;
        buf[8] = b8;
        buf[9] = b9;
        buf[10] = b10;
        buf[11] = b11;
        buf
    }

    #[test]
    fn pcr_decoded_when_well_formed() {
        let base: u64 = 0x1_2345_6789;
        let ext: u16 = 100;
        let buf = build_pcr_packet(base, ext, false);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.pcr_27mhz, Some(base * 300 + ext as u64));
        assert_eq!(pkt.pcr_malformed, None);
    }

    #[test]
    fn pcr_malformed_when_reserved_bits_zero() {
        let buf = build_pcr_packet(0x100, 0, true);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.pcr_27mhz, None);
        assert_eq!(
            pkt.pcr_malformed,
            Some(PcrMalformedKind::InvalidReservedBits)
        );
    }

    #[test]
    fn pcr_malformed_when_extension_above_299() {
        // ext = 300 (out of range — max valid is 299).
        let buf = build_pcr_packet(0x100, 300, false);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.pcr_27mhz, None);
        assert_eq!(
            pkt.pcr_malformed,
            Some(PcrMalformedKind::ExtensionOutOfRange)
        );
    }

    #[test]
    fn pcr_ext_at_boundary_299_is_valid() {
        let buf = build_pcr_packet(0x100, 299, false);
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.pcr_27mhz, Some(0x100 * 300 + 299));
        assert_eq!(pkt.pcr_malformed, None);
    }

    #[test]
    fn adaptation_control_00_flagged_reserved() {
        let mut buf = build_simple_packet(0x100, false, 0);
        buf[3] &= 0x0F; // adaptation_control=00 (bits 5-4 = 00, already zero)
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(
            pkt.adaptation_malformed,
            Some(AdaptationFieldKind::ReservedControl)
        );
        assert!(!pkt.has_payload && !pkt.has_adaptation_field);
    }

    #[test]
    fn adaptation_only_control_10_requires_length_183() {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x01;
        buf[2] = 0x00;
        buf[3] = 0x20; // adaptation_control=10 (adaptation only)
        buf[4] = 100; // wrong: must be exactly 183 for control 10
        buf[5] = 0x00; // flags
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(
            pkt.adaptation_malformed,
            Some(AdaptationFieldKind::BadLengthForControl)
        );
    }

    #[test]
    fn pcr_flag_with_short_field_is_short_pcr() {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x01;
        buf[2] = 0x00;
        buf[3] = 0x30; // adaptation_control=11
        buf[4] = 1; // af_len=1 (flags only) — too short for the 6 PCR bytes
        buf[5] = 0x10; // flags: PCR_flag=1
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(
            pkt.adaptation_malformed,
            Some(AdaptationFieldKind::ShortPcr)
        );
    }

    #[test]
    fn parses_transport_scrambling_control() {
        // adaptation_control=01 (payload only); set TSC bits (byte 3 bits 7-6).
        let mut buf = build_simple_packet(0x100, true, 0x0);
        buf[3] = 0b1000_0000 | 0x10; // TSC=10 (user scrambled) + adaptation_control=01
        let pkt = parse_ts_packet(&buf).unwrap();
        assert_eq!(pkt.transport_scrambling_control, 0b10);

        let clear = build_simple_packet(0x100, true, 0x0);
        assert_eq!(
            parse_ts_packet(&clear)
                .unwrap()
                .transport_scrambling_control,
            0
        );
    }
}
