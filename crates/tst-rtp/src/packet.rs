//! `RtpHeader` — the 12-byte fixed RTP header per RFC 3550 §5.1.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The **encoder** emits only the fixed 12-byte header (no CSRC list, no
//! extension): `V=2`, `P=0`, `X=0`, `CC=0`. `M` defaults to 0 and `PT`
//! defaults to 33 (MP2T) per RFC 3551 §6 Table 5; both are settable
//! via the [`marker`](RtpHeader::marker) and
//! [`payload_type`](RtpHeader::payload_type) fields.
//!
//! The **decoder** ([`RtpHeader::decode`]) parses received packets fully —
//! it skips any CSRC list and extension header and trims RFC 3550 padding,
//! returning the true payload bounds — so packets from other RTP senders
//! (CSRC/extension/padding present) depacketize correctly.
//!
//! `decode` no longer enforces `PT=33`; instead it returns the `payload_type`
//! field so callers can enforce whatever payload type they require. The MP2T
//! recv path enforces `PT=33` at the transport boundary.

use thiserror::Error;

/// Fixed RTP header length per RFC 3550 §5.1.
pub const RTP_HEADER_LEN: usize = 12;

/// RTP payload type for MPEG-2 Transport Stream per RFC 3551 §6 Table 5.
pub(crate) const RTP_PT_MP2T: u8 = 33;

/// RTP version per RFC 3550 §5.1 (always 2).
pub(crate) const RTP_VERSION: u8 = 2;

/// One RTP fixed-header record.
///
/// Field accessors return scalars; the wire encoding lives in
/// `encode_into`. The struct is `#[non_exhaustive]` pre-1.0 so
/// adding fields is not a breaking change.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RtpHeader {
    /// Sequence number — wraps modulo 2^16.
    pub seq: u16,
    /// 32-bit timestamp at the rate of the payload's clock; for MP2T
    /// (PT=33) this is 90 kHz per RFC 3551 §6 Table 5.
    pub timestamp: u32,
    /// Synchronization source — random per-stream identifier.
    pub ssrc: u32,
    /// Marker bit (M). For MP2T sends this is always 0; on decode it
    /// carries the peer's value (RFC 6184 uses it as an access-unit
    /// end hint).
    pub marker: bool,
    /// Payload type (PT), 7 bits. 33 = MP2T; H.264 uses a dynamic PT
    /// assigned in SDP (commonly 96).
    pub payload_type: u8,
}

impl RtpHeader {
    /// Construct a header from `seq` / `timestamp` / `ssrc`. Encoding
    /// fields are pinned: V=2, P=0, X=0, CC=0. `marker` defaults to
    /// `false` and `payload_type` defaults to `RTP_PT_MP2T` (33).
    pub fn new(seq: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            seq,
            timestamp,
            ssrc,
            marker: false,
            payload_type: RTP_PT_MP2T,
        }
    }

    /// Encode this header into the first 12 bytes of `buf`.
    ///
    /// # Panics
    ///
    /// Panics if `buf.len() < 12`.
    pub(crate) fn encode_into(&self, buf: &mut [u8]) {
        assert!(
            buf.len() >= RTP_HEADER_LEN,
            "RtpHeader::encode_into: buf too small ({} < {RTP_HEADER_LEN})",
            buf.len(),
        );
        // Octet 0: V(2) | P(1) | X(1) | CC(4) = 0b10_0_0_0000 = 0x80
        buf[0] = (RTP_VERSION << 6) & 0xC0;
        // Octet 1: M(1) | PT(7)
        buf[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7F);
        // Octets 2..4: sequence (big-endian)
        buf[2..4].copy_from_slice(&self.seq.to_be_bytes());
        // Octets 4..8: timestamp (big-endian)
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        // Octets 8..12: SSRC (big-endian)
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// Why an RTP packet failed to parse.
///
/// At the transport boundary these are silently dropped + counter-ticked
/// (the recv-side transport's malformed-packet counter) rather than
/// surfaced as errors — RFC 3550 §5.1 expects receivers to ignore
/// unparseable packets and continue.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RtpParseError {
    /// Packet too short to contain a fixed header (or fixed-header +
    /// CSRC list given the CC field's count).
    #[error("RTP packet too short: {got} < {need}")]
    Truncated { got: usize, need: usize },
    /// `V` (version) field was not 2.
    #[error("unsupported RTP version: {0}")]
    UnsupportedVersion(u8),
}

/// Result of [`RtpHeader::decode`] — header struct + the byte range of
/// the application payload within the input buffer.
///
/// The actual payload bytes are `&buf[payload_offset..payload_end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Parsed {
    /// Decoded fixed-header fields.
    pub header: RtpHeader,
    /// Byte offset in the input where the application payload begins.
    /// Equals `12 + 4*CC` for packets with no extension (X=0).
    pub payload_offset: usize,
    /// Byte offset (exclusive) where the application payload ends.
    /// For packets with no padding (P=0) this equals the packet length.
    /// Padding bytes (RFC 3550 §5.1) are excluded.
    pub payload_end: usize,
}

impl RtpHeader {
    /// Decode an RTP header from `buf`, validate `V=2`, and return the
    /// parsed header + the byte range of the application payload.
    ///
    /// The payload is `&buf[parsed.payload_offset..parsed.payload_end]`.
    ///
    /// Handles the full RFC 3550 §5.1 / §5.3.1 framing:
    /// - **CSRC list** (`CC > 0`): skipped (not retained).
    /// - **Header extension** (`X=1`, RFC 3550 §5.3.1): the 4-byte extension
    ///   header (profile + length-in-32bit-words) and its data words are
    ///   skipped; `payload_offset` is advanced past them.
    /// - **Padding** (`P=1`, RFC 3550 §5.1): the last byte of the packet is
    ///   the padding count; those bytes are excluded from `payload_end`.
    ///
    /// Malformed extension or padding (truncated / zero count) returns
    /// `Err(RtpParseError::Truncated)` so the caller can tick the
    /// malformed-packet counter and drop the packet.
    pub fn decode(buf: &[u8]) -> Result<Parsed, RtpParseError> {
        if buf.len() < RTP_HEADER_LEN {
            return Err(RtpParseError::Truncated {
                got: buf.len(),
                need: RTP_HEADER_LEN,
            });
        }
        let v = (buf[0] >> 6) & 0x03;
        if v != RTP_VERSION {
            return Err(RtpParseError::UnsupportedVersion(v));
        }
        let x_bit = (buf[0] >> 4) & 0x01; // header extension present
        let p_bit = (buf[0] >> 5) & 0x01; // padding present
        let cc = (buf[0] & 0x0F) as usize;
        let marker = (buf[1] & 0x80) != 0;
        let pt = buf[1] & 0x7F;

        // Byte offset right after the fixed header + CSRC list.
        let after_csrc = RTP_HEADER_LEN + cc * 4;
        if buf.len() < after_csrc {
            return Err(RtpParseError::Truncated {
                got: buf.len(),
                need: after_csrc,
            });
        }

        // RFC 3550 §5.3.1: if X=1, a 4-byte extension header follows the
        // CSRC list.  Bytes [after_csrc..after_csrc+2] are the profile ID
        // (unused), bytes [after_csrc+2..after_csrc+4] are the extension
        // length in 32-bit words (not counting the 4-byte prefix).
        let payload_offset = if x_bit == 1 {
            // Need at least 4 bytes for the extension header itself.
            let ext_header_end = after_csrc + 4;
            if buf.len() < ext_header_end {
                return Err(RtpParseError::Truncated {
                    got: buf.len(),
                    need: ext_header_end,
                });
            }
            let ext_len_words =
                u16::from_be_bytes([buf[after_csrc + 2], buf[after_csrc + 3]]) as usize;
            // ext_total = 4-byte prefix + ext_len_words * 4 bytes of data.
            let ext_total = 4 + ext_len_words * 4;
            let after_ext = after_csrc + ext_total;
            if buf.len() < after_ext {
                return Err(RtpParseError::Truncated {
                    got: buf.len(),
                    need: after_ext,
                });
            }
            after_ext
        } else {
            after_csrc
        };

        // RFC 3550 §5.1: if P=1, the last byte of the packet is the count
        // of padding bytes appended (including itself).  A count of 0 is
        // invalid, and a count larger than the remaining payload is invalid.
        let payload_end = if p_bit == 1 {
            // Need at least one byte for the padding count.
            if buf.len() < payload_offset + 1 {
                return Err(RtpParseError::Truncated {
                    got: buf.len(),
                    need: payload_offset + 1,
                });
            }
            let pad = buf[buf.len() - 1] as usize;
            if pad == 0 || pad > buf.len() - payload_offset {
                return Err(RtpParseError::Truncated {
                    got: buf.len() - payload_offset,
                    need: pad + 1,
                });
            }
            buf.len() - pad
        } else {
            buf.len()
        };

        let seq = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        Ok(Parsed {
            header: RtpHeader {
                seq,
                timestamp,
                ssrc,
                marker,
                payload_type: pt,
            },
            payload_offset,
            payload_end,
        })
    }
}

/// One RTP payload a packetizer produced, with the marker bit it needs
/// (set on the last payload of an access unit). The sender writes the RTP
/// header — sequence number, timestamp, SSRC, payload type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chunk {
    /// The RTP payload: a whole NAL unit or one fragmentation unit.
    pub payload: Vec<u8>,
    /// The RTP marker bit: `true` on the access unit's last payload.
    pub marker: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Step-2 failing tests: X=1 extension skipping and P=1 padding stripping ---

    #[test]
    fn decode_skips_rfc3550_header_extension() {
        // RFC 3550 §5.3.1: when X=1, a 4-byte extension header follows the CSRC list.
        // The first two bytes are a profile-specific ID, the next two are the
        // extension length in 32-bit words (not counting the 4-byte prefix).
        //
        // Packet layout (24 bytes total):
        //   Byte 0:  0x90 = V=2 | P=0 | X=1 | CC=0
        //   Byte 1:  33   = M=0 | PT=33
        //   Bytes 2..4:  seq=1
        //   Bytes 4..8:  timestamp=0
        //   Bytes 8..12: ssrc=0
        //   Bytes 12..14: ext profile = 0xBEDE (RFC 5285 one-byte-header form)
        //   Bytes 14..16: ext length = 1 word (= 4 bytes of extension data follow)
        //   Bytes 16..20: ext data 0xDEADBEEF
        //   Bytes 20..24: actual MP2T payload 0xAAAAAAAA
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0x90, 33, 0, 1,                         // V=2,X=1,CC=0 | PT=33 | seq=1
            0, 0, 0, 0,                             // timestamp
            0, 0, 0, 0,                             // ssrc
            0xBE, 0xDE, 0x00, 0x01,                 // ext header: profile 0xBEDE, len=1 word
            0xDE, 0xAD, 0xBE, 0xEF,                 // ext data (1 word = 4 bytes)
            0xAA, 0xAA, 0xAA, 0xAA,                 // MP2T payload
        ];
        let parsed = RtpHeader::decode(pkt).expect("valid X=1 packet should parse");
        assert_eq!(
            &pkt[parsed.payload_offset..parsed.payload_end],
            &[0xAA, 0xAA, 0xAA, 0xAA],
            "payload must be the 4 bytes after the extension, not the extension itself"
        );
    }

    #[test]
    fn decode_rejects_truncated_extension() {
        // X=1 but the packet is only 12 bytes — the 4-byte extension header is missing.
        let pkt: &[u8] = &[
            0x90, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
            // no extension header bytes
        ];
        let err = RtpHeader::decode(pkt).unwrap_err();
        assert!(
            matches!(err, RtpParseError::Truncated { .. }),
            "truncated extension must produce Truncated error, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_truncated_extension_data() {
        // X=1, extension length=2 words (8 bytes of data) but only 4 bytes present.
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0x90, 33, 0, 1,
            0, 0, 0, 0,
            0, 0, 0, 0,
            0xBE, 0xDE, 0x00, 0x02,   // ext header: len=2 words (8 bytes expected)
            0xDE, 0xAD, 0xBE, 0xEF,   // only 4 bytes of ext data present (packet truncated)
        ];
        let err = RtpHeader::decode(pkt).unwrap_err();
        assert!(
            matches!(err, RtpParseError::Truncated { .. }),
            "truncated extension data must produce Truncated error, got {err:?}"
        );
    }

    #[test]
    fn decode_strips_rfc3550_padding() {
        // RFC 3550 §5.1: when P=1, the last byte of the packet is the count of
        // padding bytes appended (including itself).  Those bytes must NOT be
        // delivered as payload.
        //
        // Packet layout (16 bytes):
        //   Bytes 0..12:  fixed RTP header, P=1
        //   Bytes 12..14: payload 0xAA 0xAA
        //   Bytes 14..15: padding 0x00 0x02  (pad byte 1, pad count = 2)
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0xA0, 33, 0, 1,             // V=2, P=1, X=0, CC=0 | PT=33
            0, 0, 0, 0,                 // timestamp
            0, 0, 0, 0,                 // ssrc
            0xAA, 0xAA,                 // real payload
            0x00, 0x02,                 // 2 padding bytes (last byte = count)
        ];
        let parsed = RtpHeader::decode(pkt).expect("valid P=1 packet should parse");
        assert_eq!(
            &pkt[parsed.payload_offset..parsed.payload_end],
            &[0xAA, 0xAA],
            "padding bytes must not be part of the returned payload"
        );
    }

    #[test]
    fn decode_rejects_zero_padding_count() {
        // P=1, last byte = 0 — RFC 3550 forbids a padding count of 0.
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0xA0, 33, 0, 1,
            0, 0, 0, 0,
            0, 0, 0, 0,
            0xAA, 0xAA,
            0x00,       // last byte = 0: malformed
        ];
        let err = RtpHeader::decode(pkt).unwrap_err();
        assert!(
            matches!(err, RtpParseError::Truncated { .. }),
            "padding count 0 must produce Truncated error, got {err:?}"
        );
    }

    #[test]
    fn decode_rejects_padding_exceeds_payload() {
        // P=1, last byte = 10, but the packet only has 13 bytes total.
        // pad count > packet_len - payload_offset → malformed.
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0xA0, 33, 0, 1,
            0, 0, 0, 0,
            0, 0, 0, 0,
            0x0A,       // last byte = 10 (but only 1 byte available for payload+padding)
        ];
        let err = RtpHeader::decode(pkt).unwrap_err();
        assert!(
            matches!(err, RtpParseError::Truncated { .. }),
            "over-large padding count must produce Truncated error, got {err:?}"
        );
    }

    #[test]
    fn decode_x_and_p_combined() {
        // X=1 and P=1 together: extension is skipped, padding is stripped.
        // Header (12) + ext-header (4) + ext-data 1 word (4) + payload (4) + pad (2) = 26 bytes.
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0xB0, 33, 0, 1,             // V=2, P=1, X=1, CC=0
            0, 0, 0, 0,
            0, 0, 0, 0,
            0xBE, 0xDE, 0x00, 0x01,     // ext: profile, 1-word length
            0xDE, 0xAD, 0xBE, 0xEF,     // ext data (4 bytes)
            0xAA, 0xAA, 0xAA, 0xAA,     // real payload
            0x00, 0x02,                 // 2 padding bytes
        ];
        let parsed = RtpHeader::decode(pkt).expect("valid X=1+P=1 packet should parse");
        assert_eq!(
            &pkt[parsed.payload_offset..parsed.payload_end],
            &[0xAA, 0xAA, 0xAA, 0xAA],
        );
    }

    // --- End Step-2 tests ---

    #[test]
    fn encode_writes_rfc_3550_byte_layout() {
        // RFC 3550 §5.1 octet layout:
        //  0                   1                   2                   3
        //  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |V=2|P|X|  CC   |M|     PT      |       sequence number         |
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |                           timestamp                           |
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |           synchronization source (SSRC) identifier            |
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        let h = RtpHeader::new(0x1234, 0xDEADBEEF, 0xCAFE_BABE);
        let mut buf = [0u8; 16];
        h.encode_into(&mut buf);
        assert_eq!(
            &buf[..12],
            &[
                0x80, // V=2, P=0, X=0, CC=0
                33,   // M=0, PT=33
                0x12, 0x34, // seq
                0xDE, 0xAD, 0xBE, 0xEF, // timestamp
                0xCA, 0xFE, 0xBA, 0xBE, // SSRC
            ],
        );
    }

    #[test]
    fn encode_zero_filled_fields() {
        let h = RtpHeader::new(0, 0, 0);
        let mut buf = [0xFFu8; 12];
        h.encode_into(&mut buf);
        // Octet 0 = 0x80, octet 1 = 33, rest = 0.
        assert_eq!(buf[0], 0x80);
        assert_eq!(buf[1], 33);
        assert!(buf[2..].iter().all(|b| *b == 0));
    }

    #[test]
    #[should_panic(expected = "buf too small")]
    fn encode_panics_on_short_buf() {
        let h = RtpHeader::new(0, 0, 0);
        let mut buf = [0u8; 11];
        h.encode_into(&mut buf);
    }

    #[test]
    fn decode_matches_encoded_bytes() {
        let h = RtpHeader::new(0x1234, 0xDEADBEEF, 0xCAFE_BABE);
        let mut buf = [0u8; 12];
        h.encode_into(&mut buf);
        let parsed = RtpHeader::decode(&buf).unwrap();
        // Decode returns header + payload offset.
        assert_eq!(parsed.header, h);
        assert_eq!(parsed.payload_offset, 12);
    }

    #[test]
    fn decode_rejects_wrong_version() {
        // V=3 (top two bits = 0b11_0_0_0000 = 0xC0) — invalid.
        let mut buf = [0u8; 12];
        buf[0] = 0xC0;
        buf[1] = 33;
        let err = RtpHeader::decode(&buf).unwrap_err();
        assert!(matches!(err, RtpParseError::UnsupportedVersion(3)));
    }

    #[test]
    fn decode_dynamic_payload_type_accepted() {
        // Dynamic PT (e.g. 96 for H.264) must now decode successfully;
        // PT enforcement is the caller's responsibility.
        let mut buf = [0u8; 12];
        buf[0] = 0x80;
        buf[1] = 96; // dynamic PT range, not MP2T
        let parsed = RtpHeader::decode(&buf).unwrap();
        assert_eq!(parsed.header.payload_type, 96);
        assert!(!parsed.header.marker);
    }

    #[test]
    fn decode_accepts_marker_bit() {
        // M=1 is informational on receive; decode exposes it via marker field.
        let mut buf = [0u8; 12];
        buf[0] = 0x80;
        buf[1] = 0x80 | 33; // M=1, PT=33
        let parsed = RtpHeader::decode(&buf).unwrap();
        assert_eq!(parsed.header.seq, 0);
        assert!(parsed.header.marker);
        assert_eq!(parsed.header.payload_type, 33);
    }

    #[test]
    fn decode_skips_csrc_list() {
        // CC=2 → 8 extra CSRC bytes between header and payload.
        let mut buf = vec![0u8; 12 + 8];
        buf[0] = 0x82; // V=2, CC=2
        buf[1] = 33;
        let parsed = RtpHeader::decode(&buf).unwrap();
        assert_eq!(parsed.payload_offset, 20);
    }

    #[test]
    fn decode_rejects_truncated() {
        let buf = [0u8; 11];
        let err = RtpHeader::decode(&buf).unwrap_err();
        assert!(matches!(err, RtpParseError::Truncated { .. }));
    }

    #[test]
    fn decode_rejects_csrc_overflow() {
        // CC=5 needs 20 bytes of CSRC, but buf is only 12.
        let mut buf = [0u8; 12];
        buf[0] = 0x85; // V=2, CC=5
        buf[1] = 33;
        let err = RtpHeader::decode(&buf).unwrap_err();
        assert!(matches!(err, RtpParseError::Truncated { .. }));
    }

    #[test]
    fn decode_exposes_marker_and_dynamic_pt() {
        // V=2, M=1, PT=96 (dynamic H.264), seq=7, ts=0x11223344, ssrc=0x55667788, 1 payload byte
        #[rustfmt::skip]
        let pkt: &[u8] = &[
            0x80, 0x80 | 96, 0, 7,
            0x11, 0x22, 0x33, 0x44,
            0x55, 0x66, 0x77, 0x88,
            0xAB,
        ];
        let parsed = RtpHeader::decode(pkt).expect("dynamic PT must decode");
        assert!(parsed.header.marker);
        assert_eq!(parsed.header.payload_type, 96);
        assert_eq!(parsed.header.seq, 7);
        assert_eq!(&pkt[parsed.payload_offset..parsed.payload_end], &[0xAB]);
    }

    #[test]
    fn decode_pt33_no_marker_unchanged() {
        let pkt: &[u8] = &[0x80, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x47];
        let parsed = RtpHeader::decode(pkt).unwrap();
        assert!(!parsed.header.marker);
        assert_eq!(parsed.header.payload_type, 33);
    }

    #[test]
    fn encode_into_honors_marker_and_pt_fields() {
        // Default construction stays byte-identical to the old pinned M=0/PT=33.
        let h = RtpHeader::new(1, 2, 3);
        let mut buf = [0u8; 12];
        h.encode_into(&mut buf);
        assert_eq!(buf[1], 33); // M=0 | PT=33

        // Setting the marker field must flip the M bit in octet 1.
        let mut h = RtpHeader::new(1, 2, 3);
        h.marker = true;
        let mut buf = [0u8; 12];
        h.encode_into(&mut buf);
        assert_eq!(buf[1], 0x80 | 33); // M=1 | PT=33
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn encode_decode_roundtrip(
            seq in any::<u16>(),
            ts in any::<u32>(),
            ssrc in any::<u32>(),
            marker in any::<bool>(),
            payload_type in 0u8..=127,
        ) {
            // Struct literal — same crate, so #[non_exhaustive] doesn't
            // bind; covers marker=true and dynamic PTs, which new() never
            // produces.
            let h = RtpHeader {
                seq,
                timestamp: ts,
                ssrc,
                marker,
                payload_type,
            };
            let mut buf = [0u8; 12];
            h.encode_into(&mut buf);
            let parsed = RtpHeader::decode(&buf).unwrap();
            prop_assert_eq!(parsed.header, h);
            prop_assert_eq!(parsed.header.marker, marker);
            prop_assert_eq!(parsed.header.payload_type, payload_type);
            prop_assert_eq!(parsed.payload_offset, 12);
        }
    }
}
