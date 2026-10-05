//! RTCP RR / SR / SDES packet encoding, decoding per RFC 3550 §6.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! v1 supports the minimum compound packet needed for the receiver-side
//! reports (RR + SDES with CNAME, RFC 3550 §6.5.1) and sender-side
//! reports (SR + SDES with CNAME). NACK / REMB / PLI / RTPFB are out of
//! scope.

pub mod ingest;
pub mod reporter;
pub mod stats;

use bytes::{Buf, BufMut};

/// Errors returned by the fallible RTCP encoders (RFC 3550 §6).
///
/// The encoders validate that each wire field can faithfully represent the
/// value being encoded — they reject out-of-range input rather than silently
/// truncating (e.g. masking the 5-bit RC field) or panicking. A failure here
/// for a locally-built packet indicates an internal construction bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtcpError {
    /// More report blocks than the 5-bit RC/SC count field can express
    /// (max 31, RFC 3550 §6.4.1). Carries the offending block count.
    TooManyReportBlocks(usize),
    /// The packet's length in 32-bit words exceeds the 16-bit RTCP length
    /// field (max 65535 words, RFC 3550 §6.4.1). Carries the offending
    /// word count.
    LengthOverflow(usize),
    /// The CNAME (or another SDES item value) is longer than its 1-byte
    /// length field can express (max 255 bytes, RFC 3550 §6.5). Carries the
    /// offending byte length.
    CnameTooLong(usize),
}

impl core::fmt::Display for RtcpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyReportBlocks(n) => {
                write!(
                    f,
                    "RTCP report has {n} blocks; the 5-bit RC field allows at most 31"
                )
            }
            Self::LengthOverflow(n) => {
                write!(
                    f,
                    "RTCP packet is {n} 32-bit words; the 16-bit length field allows at most {}",
                    u16::MAX
                )
            }
            Self::CnameTooLong(n) => {
                write!(
                    f,
                    "RTCP SDES CNAME is {n} bytes; the 1-byte length field allows at most 255"
                )
            }
        }
    }
}

impl std::error::Error for RtcpError {}

/// Maximum report blocks the 5-bit RC/SC count field can express
/// (RFC 3550 §6.4.1).
const MAX_REPORT_BLOCKS: usize = 31;

/// RTCP packet types we encode and decode. Per RFC 3550 §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtcpPacketType {
    /// SR — Sender Report (PT=200)
    SenderReport,
    /// RR — Receiver Report (PT=201)
    ReceiverReport,
    /// SDES — Source Description (PT=202)
    SourceDescription,
}

impl RtcpPacketType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            200 => Some(Self::SenderReport),
            201 => Some(Self::ReceiverReport),
            202 => Some(Self::SourceDescription),
            _ => None,
        }
    }
    pub fn to_u8(self) -> u8 {
        match self {
            Self::SenderReport => 200,
            Self::ReceiverReport => 201,
            Self::SourceDescription => 202,
        }
    }
}

/// One report block per RFC 3550 §6.4.1 (in SR) or §6.4.2 (in RR).
///
/// 24 bytes wire-format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportBlock {
    pub ssrc: u32,
    /// Fraction lost since last RR/SR, Q8 fixed point (RFC 3550 §6.4.1).
    pub fraction_lost: u8,
    /// Cumulative packets lost — 24-bit signed (we store as i32 with
    /// top 8 bits zero for positive losses).
    pub cumulative_lost: i32,
    /// Extended highest sequence number received (RFC 3550 §A.1).
    pub extended_highest_seq: u32,
    /// Interarrival jitter (RFC 3550 §6.4.1).
    pub jitter: u32,
    /// Last SR timestamp (middle 32 bits of NTP from received SR).
    pub last_sr: u32,
    /// Delay since last SR, in units of 1/65536 seconds (RFC 3550 §6.4.1).
    pub delay_since_last_sr: u32,
}

impl ReportBlock {
    /// 24-byte wire length per RFC 3550 §6.4.1.
    pub const WIRE_LEN: usize = 24;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.put_u32(self.ssrc);
        let lost = (self.cumulative_lost & 0xFFFFFF) as u32;
        out.put_u32(((self.fraction_lost as u32) << 24) | lost);
        out.put_u32(self.extended_highest_seq);
        out.put_u32(self.jitter);
        out.put_u32(self.last_sr);
        out.put_u32(self.delay_since_last_sr);
    }

    pub fn decode(mut input: &[u8]) -> Result<(Self, usize), &'static str> {
        if input.len() < Self::WIRE_LEN {
            return Err("RTCP ReportBlock truncated");
        }
        let ssrc = input.get_u32();
        let flx = input.get_u32();
        let fraction_lost = (flx >> 24) as u8;
        let cumulative_raw = flx & 0xFFFFFF;
        let cumulative_lost = if cumulative_raw & 0x800000 != 0 {
            // Sign-extend the 24-bit value to i32
            (cumulative_raw | 0xFF000000) as i32
        } else {
            cumulative_raw as i32
        };
        let extended_highest_seq = input.get_u32();
        let jitter = input.get_u32();
        let last_sr = input.get_u32();
        let delay_since_last_sr = input.get_u32();
        Ok((
            Self {
                ssrc,
                fraction_lost,
                cumulative_lost,
                extended_highest_seq,
                jitter,
                last_sr,
                delay_since_last_sr,
            },
            Self::WIRE_LEN,
        ))
    }
}

/// SR — Sender Report. RFC 3550 §6.4.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderReport {
    pub ssrc: u32,
    /// NTP timestamp at moment of report — 64-bit fixed-point per RFC 3550 §4.
    pub ntp_timestamp: u64,
    /// RTP timestamp corresponding to the NTP timestamp above.
    pub rtp_timestamp: u32,
    pub sender_packet_count: u32,
    pub sender_octet_count: u32,
    pub report_blocks: Vec<ReportBlock>,
}

impl SenderReport {
    /// Validate the RTCP header length field (32-bit words minus 1, the value
    /// stored in the 16-bit `length` field) for a packet of `payload_len_words`
    /// payload words. Returns the `u16` length-field value or
    /// [`RtcpError::LengthOverflow`] if `length_field` would not fit in 16 bits.
    ///
    /// The wire `length` is the total packet length in 32-bit words minus 1;
    /// since the 4-byte header is one word, `length == payload_len_words`.
    fn encode_length_field(payload_len_words: usize) -> Result<u16, RtcpError> {
        u16::try_from(payload_len_words).map_err(|_| RtcpError::LengthOverflow(payload_len_words))
    }

    /// Encode as compound RTCP — version=2, padding=0, PT=200, length
    /// in 32-bit words minus 1.
    ///
    /// Returns [`RtcpError::TooManyReportBlocks`] if there are more than 31
    /// report blocks (the 5-bit RC field cannot express the count) or
    /// [`RtcpError::LengthOverflow`] if the packet exceeds the 16-bit length
    /// field — rather than silently masking/truncating either field.
    pub fn encode(&self) -> Result<Vec<u8>, RtcpError> {
        if self.report_blocks.len() > MAX_REPORT_BLOCKS {
            return Err(RtcpError::TooManyReportBlocks(self.report_blocks.len()));
        }
        let block_count = self.report_blocks.len() as u8;
        let payload_len_words = 6 + (self.report_blocks.len() * 6);
        // 1 word for the header itself = total length in words minus 1
        let length_field = Self::encode_length_field(payload_len_words)?;
        let mut out = Vec::with_capacity((payload_len_words + 1) * 4);
        out.push(0x80 | (block_count & 0x1F));
        out.push(RtcpPacketType::SenderReport.to_u8());
        out.put_u16(length_field);
        out.put_u32(self.ssrc);
        out.put_u64(self.ntp_timestamp);
        out.put_u32(self.rtp_timestamp);
        out.put_u32(self.sender_packet_count);
        out.put_u32(self.sender_octet_count);
        for b in &self.report_blocks {
            b.encode(&mut out);
        }
        Ok(out)
    }

    pub fn decode(input: &[u8]) -> Result<(Self, usize), &'static str> {
        if input.len() < 28 {
            return Err("RTCP SR truncated");
        }
        let v = (input[0] >> 6) & 0x3;
        if v != 2 {
            return Err("RTCP SR bad version");
        }
        let rc = (input[0] & 0x1F) as usize;
        let pt = input[1];
        if pt != RtcpPacketType::SenderReport.to_u8() {
            return Err("RTCP SR wrong PT");
        }
        let length_words = u16::from_be_bytes([input[2], input[3]]) as usize;
        let total_bytes = (length_words + 1) * 4;
        // Fixed SR payload: 4 (SSRC) + 8 (NTP) + 4 (RTP ts) + 4 (pkt count) + 4 (octet count) = 24
        // Plus 4-byte header = 28 minimum; plus rc * 24 for report blocks.
        let min_bytes = 28usize.saturating_add(rc.saturating_mul(ReportBlock::WIRE_LEN));
        if total_bytes < min_bytes {
            return Err("RTCP SR declared length too small for fixed fields");
        }
        if input.len() < total_bytes {
            return Err("RTCP SR truncated by length");
        }
        let mut cursor = &input[4..total_bytes];
        let ssrc = cursor.get_u32();
        let ntp_timestamp = cursor.get_u64();
        let rtp_timestamp = cursor.get_u32();
        let sender_packet_count = cursor.get_u32();
        let sender_octet_count = cursor.get_u32();
        let mut report_blocks = Vec::with_capacity(rc);
        for _ in 0..rc {
            let (rb, _) = ReportBlock::decode(cursor)?;
            report_blocks.push(rb);
            cursor = &cursor[ReportBlock::WIRE_LEN..];
        }
        Ok((
            Self {
                ssrc,
                ntp_timestamp,
                rtp_timestamp,
                sender_packet_count,
                sender_octet_count,
                report_blocks,
            },
            total_bytes,
        ))
    }
}

/// RR — Receiver Report. RFC 3550 §6.4.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverReport {
    pub ssrc: u32,
    pub report_blocks: Vec<ReportBlock>,
}

impl ReceiverReport {
    /// Encode as compound RTCP — version=2, padding=0, PT=201, length
    /// in 32-bit words minus 1.
    ///
    /// Returns [`RtcpError::TooManyReportBlocks`] if there are more than 31
    /// report blocks or [`RtcpError::LengthOverflow`] if the packet exceeds
    /// the 16-bit length field — rather than silently masking/truncating.
    pub fn encode(&self) -> Result<Vec<u8>, RtcpError> {
        if self.report_blocks.len() > MAX_REPORT_BLOCKS {
            return Err(RtcpError::TooManyReportBlocks(self.report_blocks.len()));
        }
        let block_count = self.report_blocks.len() as u8;
        let payload_len_words = 1 + (self.report_blocks.len() * 6);
        let length_field = SenderReport::encode_length_field(payload_len_words)?;
        let mut out = Vec::with_capacity((payload_len_words + 1) * 4);
        out.push(0x80 | (block_count & 0x1F));
        out.push(RtcpPacketType::ReceiverReport.to_u8());
        out.put_u16(length_field);
        out.put_u32(self.ssrc);
        for b in &self.report_blocks {
            b.encode(&mut out);
        }
        Ok(out)
    }

    pub fn decode(input: &[u8]) -> Result<(Self, usize), &'static str> {
        if input.len() < 8 {
            return Err("RTCP RR truncated");
        }
        let v = (input[0] >> 6) & 0x3;
        if v != 2 {
            return Err("RTCP RR bad version");
        }
        let rc = (input[0] & 0x1F) as usize;
        let pt = input[1];
        if pt != RtcpPacketType::ReceiverReport.to_u8() {
            return Err("RTCP RR wrong PT");
        }
        let length_words = u16::from_be_bytes([input[2], input[3]]) as usize;
        let total_bytes = (length_words + 1) * 4;
        // Fixed RR payload: 4 (SSRC) → header + SSRC = 8 bytes minimum; plus rc * 24.
        let min_bytes = 8usize.saturating_add(rc.saturating_mul(ReportBlock::WIRE_LEN));
        if total_bytes < min_bytes {
            return Err("RTCP RR declared length too small for fixed fields");
        }
        if input.len() < total_bytes {
            return Err("RTCP RR truncated by length");
        }
        let mut cursor = &input[4..total_bytes];
        let ssrc = cursor.get_u32();
        let mut report_blocks = Vec::with_capacity(rc);
        for _ in 0..rc {
            let (rb, _) = ReportBlock::decode(cursor)?;
            report_blocks.push(rb);
            cursor = &cursor[ReportBlock::WIRE_LEN..];
        }
        Ok((
            Self {
                ssrc,
                report_blocks,
            },
            total_bytes,
        ))
    }
}

/// SDES — Source Description, with the single CNAME item required by
/// RFC 3550 §6.5.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdesPacket {
    pub ssrc: u32,
    pub cname: String,
}

impl SdesPacket {
    /// Encode as a compound RTCP packet containing one SDES chunk with
    /// a CNAME item. RFC 3550 §6.5.
    ///
    /// Returns [`RtcpError::CnameTooLong`] if the CNAME exceeds 255 bytes (the
    /// 1-byte SDES item-length field cannot express it) — rather than the old
    /// panicking `len as u8` conversion. A length overflow of the 16-bit RTCP
    /// length field is impossible here (the chunk is bounded by the 255-byte
    /// CNAME) but is checked for completeness.
    pub fn encode(&self) -> Result<Vec<u8>, RtcpError> {
        // SDES item: type=1 (CNAME), length=len(cname), bytes=cname
        // Chunk: SSRC (4) + items + null terminator + padding to 4-byte boundary
        let cname_bytes = self.cname.as_bytes();
        if cname_bytes.len() > 255 {
            return Err(RtcpError::CnameTooLong(cname_bytes.len()));
        }
        let item_size = 2 + cname_bytes.len(); // type + length + value
        let chunk_size = 4 + item_size + 1; // SSRC + item + null
        let padded_chunk_size = (chunk_size + 3) & !3;
        let length_field = SenderReport::encode_length_field((4 + padded_chunk_size) / 4 - 1)?;
        let mut out = Vec::with_capacity(4 + padded_chunk_size);
        out.push(0x81); // V=2, P=0, SC=1
        out.push(RtcpPacketType::SourceDescription.to_u8());
        out.put_u16(length_field);
        out.put_u32(self.ssrc);
        out.push(1); // CNAME
        out.push(cname_bytes.len() as u8);
        out.extend_from_slice(cname_bytes);
        out.push(0); // null terminator
        while out.len() < 4 + padded_chunk_size {
            out.push(0);
        }
        Ok(out)
    }

    pub fn decode(input: &[u8]) -> Result<(Self, usize), &'static str> {
        if input.len() < 8 {
            return Err("RTCP SDES truncated");
        }
        let v = (input[0] >> 6) & 0x3;
        if v != 2 {
            return Err("RTCP SDES bad version");
        }
        let sc = (input[0] & 0x1F) as usize;
        if sc != 1 {
            return Err("RTCP SDES v1 expects exactly 1 chunk");
        }
        let pt = input[1];
        if pt != RtcpPacketType::SourceDescription.to_u8() {
            return Err("RTCP SDES wrong PT");
        }
        let length_words = u16::from_be_bytes([input[2], input[3]]) as usize;
        let total_bytes = (length_words + 1) * 4;
        // Minimum: 4-byte header + 4-byte SSRC + 1-byte item type + 1-byte item len = 10.
        if total_bytes < 10 {
            return Err("RTCP SDES declared length too small for SSRC and CNAME item");
        }
        if input.len() < total_bytes {
            return Err("RTCP SDES truncated by length");
        }
        let ssrc = u32::from_be_bytes([input[4], input[5], input[6], input[7]]);
        if input[8] != 1 {
            return Err("RTCP SDES first item must be CNAME (type=1)");
        }
        let cname_len = input[9] as usize;
        if 10 + cname_len > total_bytes {
            return Err("RTCP SDES CNAME length overflows packet");
        }
        let cname = std::str::from_utf8(&input[10..10 + cname_len])
            .map_err(|_| "RTCP SDES CNAME not UTF-8")?
            .to_string();
        Ok((Self { ssrc, cname }, total_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DoS / panic regression tests (Step 2) ---
    // Each buffer passes the initial slice-length gate but carries a declared
    // length (length_words field) that is too small to cover the fixed fields.
    // Before the fix these cause a panic (empty-cursor get_u32 / index OOB).
    // After the fix they must return Err.

    /// SR: V=2, PT=200, declared length=0 (total 4 bytes), but buffer is 32
    /// bytes — passes the `input.len() < 28` and the `input.len() < total_bytes`
    /// (4) gates, then hits `cursor.get_u32()` on a 0-byte slice.
    #[test]
    fn sr_rejects_declared_length_zero() {
        let mut buf = vec![0u8; 32];
        buf[0] = 0x80; // V=2, P=0, RC=0
        buf[1] = 200; // PT=SR
        buf[2] = 0x00; // length_words high byte = 0
        buf[3] = 0x00; // length_words low byte  = 0  → total_bytes = 4
        // bytes 4..31 are zeroes (SSRC etc.) — enough to pass the 28-byte gate
        assert!(
            SenderReport::decode(&buf).is_err(),
            "SR with declared length 0 must return Err, not panic"
        );
    }

    /// RR: V=2, PT=201, declared length=0 (total 4 bytes), buffer is 16 bytes —
    /// passes the `input.len() < 8` gate and the `input.len() < 4` gate, then
    /// hits `cursor.get_u32()` on a 0-byte slice.
    #[test]
    fn rr_rejects_declared_length_zero() {
        let mut buf = vec![0u8; 16];
        buf[0] = 0x80; // V=2, P=0, RC=0
        buf[1] = 201; // PT=RR
        buf[2] = 0x00; // length_words = 0 → total_bytes = 4
        buf[3] = 0x00;
        // bytes 4..15 are zeroes — passes the 8-byte initial check
        assert!(
            ReceiverReport::decode(&buf).is_err(),
            "RR with declared length 0 must return Err, not panic"
        );
    }

    /// SDES: V=2, SC=1, PT=202, declared length=1 (total 8 bytes), buffer is
    /// exactly 8 bytes — passes the `input.len() < 8` gate and the
    /// `input.len() < total_bytes (8)` gate, then hits `input[8]` (OOB: the
    /// slice is 8 bytes, indices 0..7 only).
    #[test]
    fn sdes_rejects_declared_length_one() {
        let mut buf = vec![0u8; 8]; // exactly total_bytes = 8
        buf[0] = 0x81; // V=2, P=0, SC=1
        buf[1] = 202; // PT=SDES
        buf[2] = 0x00; // length_words = 1 → total_bytes = 8
        buf[3] = 0x01;
        // bytes 4..7: SSRC = 0 (zeroes)
        // input[8] is out of bounds — the declared packet has no room for SDES items.
        assert!(
            SdesPacket::decode(&buf).is_err(),
            "SDES with declared length 1 (8 bytes, no room for items) must return Err, not panic"
        );
    }

    // --- Fallible/validated encoder tests ---
    // Adversarial encode inputs that an infallible encoder would silently
    // corrupt (5-bit RC mask, 16-bit length truncation) or PANIC on
    // (CNAME > 255). Every one must return Err.

    fn dummy_block() -> ReportBlock {
        ReportBlock {
            ssrc: 0,
            fraction_lost: 0,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        }
    }

    /// SR with 32 report blocks: the RC field is 5 bits (max 31). The old
    /// `len() as u8` produced 0x80 | (32 & 0x1F) = 0x80 (RC=0) — a packet
    /// that lies about its block count. Must be Err.
    #[test]
    fn sr_rejects_more_than_31_blocks() {
        let sr = SenderReport {
            ssrc: 0,
            ntp_timestamp: 0,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![dummy_block(); 32],
        };
        assert_eq!(sr.encode().unwrap_err(), RtcpError::TooManyReportBlocks(32));
    }

    /// 31 blocks is the RFC-3550 maximum — must still succeed.
    #[test]
    fn sr_accepts_31_blocks() {
        let sr = SenderReport {
            ssrc: 0,
            ntp_timestamp: 0,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![dummy_block(); 31],
        };
        let bytes = sr.encode().expect("31 blocks must encode");
        assert_eq!(bytes[0] & 0x1F, 31, "RC field must be 31");
    }

    /// RR with 32 report blocks: same 5-bit RC mask bug. Must be Err.
    #[test]
    fn rr_rejects_more_than_31_blocks() {
        let rr = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![dummy_block(); 32],
        };
        assert_eq!(rr.encode().unwrap_err(), RtcpError::TooManyReportBlocks(32));
    }

    /// SDES with a 256-byte CNAME: the item-length field is 1 byte (max 255).
    /// The old encoder `panic!`ed. Must be Err, never panic.
    #[test]
    fn sdes_rejects_cname_over_255() {
        let sdes = SdesPacket {
            ssrc: 0,
            cname: "a".repeat(256),
        };
        assert_eq!(sdes.encode().unwrap_err(), RtcpError::CnameTooLong(256));
    }

    /// A 255-byte CNAME is the maximum the 1-byte length field can express —
    /// must still encode.
    #[test]
    fn sdes_accepts_cname_255() {
        let sdes = SdesPacket {
            ssrc: 0,
            cname: "a".repeat(255),
        };
        sdes.encode().expect("255-byte CNAME must encode");
    }

    /// The 16-bit RTCP length field counts 32-bit words minus 1; a packet
    /// whose word-length exceeds u16::MAX would silently truncate. We can't
    /// realistically build a 256 KB report from report blocks alone (capped
    /// at 31), so this exercises the length helper directly: the SR header is
    /// 6 words + 6 words/block; even at 31 blocks (192 words) we never reach
    /// the ceiling, so the cap is what protects the length field. This test
    /// asserts the validated ceiling via a direct check on the helper using
    /// the largest constructible packet.
    #[test]
    fn length_word_ceiling_is_checked() {
        // Sanity: u16::MAX + 1 words is the rejection boundary the encoder
        // guards. We assert the constant relationship rather than building a
        // 256 KB allocation: payload_len_words must fit in u16.
        // (The cap at 31 blocks keeps every constructible SR/RR well under
        // u16::MAX; this test documents the guard exists and is exercised by
        // the encode_length_field helper which returns Err on overflow.)
        assert!(SenderReport::encode_length_field(u16::MAX as usize + 1).is_err());
        assert!(SenderReport::encode_length_field(u16::MAX as usize).is_ok());
    }

    // --- Existing tests below ---

    #[test]
    fn rr_roundtrip_no_blocks() {
        let rr = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![],
        };
        let bytes = rr.encode().unwrap();
        assert_eq!(bytes.len(), 8); // 2 words: header + SSRC
        let (decoded, n) = ReceiverReport::decode(&bytes).unwrap();
        assert_eq!(decoded, rr);
        assert_eq!(n, 8);
    }

    #[test]
    fn rr_roundtrip_one_block() {
        let block = ReportBlock {
            ssrc: 0x11223344,
            fraction_lost: 42,
            cumulative_lost: 1000,
            extended_highest_seq: 5000,
            jitter: 250,
            last_sr: 0xDEADBEEF,
            delay_since_last_sr: 65536, // 1 second
        };
        let rr = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![block],
        };
        let bytes = rr.encode().unwrap();
        assert_eq!(bytes.len(), 8 + 24);
        let (decoded, n) = ReceiverReport::decode(&bytes).unwrap();
        assert_eq!(decoded, rr);
        assert_eq!(n, bytes.len());
    }

    #[test]
    fn sr_roundtrip() {
        let sr = SenderReport {
            ssrc: 0x12345678,
            ntp_timestamp: 0x83AA7E80_DEADBEEFu64,
            rtp_timestamp: 90000 * 60,
            sender_packet_count: 1000,
            sender_octet_count: 1316 * 1000,
            report_blocks: vec![],
        };
        let bytes = sr.encode().unwrap();
        let (decoded, n) = SenderReport::decode(&bytes).unwrap();
        assert_eq!(decoded, sr);
        assert_eq!(n, bytes.len());
    }

    #[test]
    fn sdes_roundtrip_short_cname() {
        let sdes = SdesPacket {
            ssrc: 0xABCDEF01,
            cname: "cam1@10.0.0.1".to_string(),
        };
        let bytes = sdes.encode().unwrap();
        // Length should be multiple of 4
        assert_eq!(bytes.len() % 4, 0);
        let (decoded, n) = SdesPacket::decode(&bytes).unwrap();
        assert_eq!(decoded, sdes);
        assert_eq!(n, bytes.len());
    }

    #[test]
    fn rr_rejects_bad_version() {
        let mut bytes = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![],
        }
        .encode()
        .unwrap();
        bytes[0] = 0x40; // V=1
        assert!(ReceiverReport::decode(&bytes).is_err());
    }

    #[test]
    fn signed_cumulative_lost_roundtrips() {
        // RFC 3550 §6.4.1: cumulative_lost is signed 24-bit.
        let block = ReportBlock {
            ssrc: 0x01020304,
            fraction_lost: 0,
            cumulative_lost: -10,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0,
            report_blocks: vec![block],
        };
        let bytes = rr.encode().unwrap();
        let (decoded, _) = ReceiverReport::decode(&bytes).unwrap();
        assert_eq!(decoded.report_blocks[0].cumulative_lost, -10);
    }

    /// Hand-built spec-byte vectors — round-trip tests alone cannot catch wire-format bugs.
    ///
    /// Roundtrip tests only confirm encode/decode are mutually consistent —
    /// they don't catch a case where both sides have the same bug. This test
    /// hand-builds the wire bytes per RFC 3550 §6.4.2 (and §6.4.1 for the
    /// report block) and verifies `encode()` produces exactly those bytes.
    #[test]
    fn rr_with_block_wire_format_matches_rfc3550_layout() {
        // No-block RR: V=2, P=0, RC=0, PT=201, length=1 (8 bytes / 4 - 1), SSRC.
        let rr_empty = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![],
        };
        let bytes = rr_empty.encode().unwrap();
        assert_eq!(bytes[0], 0x80, "V=2 P=0 RC=0 byte");
        assert_eq!(bytes[1], 201, "PT=RR");
        assert_eq!(
            &bytes[2..4],
            &[0x00, 0x01],
            "length=1 (in 32-bit words minus 1)"
        );
        assert_eq!(&bytes[4..8], &[0xCA, 0xFE, 0xBA, 0xBE], "SSRC");
        assert_eq!(bytes.len(), 8);

        // RR with one report block carrying a -1 cumulative_lost (24-bit signed = 0xFFFFFF).
        let block = ReportBlock {
            ssrc: 0x01020304,
            fraction_lost: 0x80, // 128/256
            cumulative_lost: -1,
            extended_highest_seq: 0xAABBCCDD,
            jitter: 0x00112233,
            last_sr: 0x44556677,
            delay_since_last_sr: 0x8899AABB,
        };
        let rr = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![block],
        };
        let bytes = rr.encode().unwrap();
        // Header: V=2 P=0 RC=1, PT=201, length = (8 + 24)/4 - 1 = 7
        assert_eq!(bytes[0], 0x81, "V=2 P=0 RC=1");
        assert_eq!(bytes[1], 201);
        assert_eq!(&bytes[2..4], &[0x00, 0x07], "length=7");
        assert_eq!(&bytes[4..8], &[0xCA, 0xFE, 0xBA, 0xBE], "sender SSRC");
        // Report block (24 bytes starting at offset 8):
        // bytes  8..12: source SSRC = 0x01020304
        assert_eq!(&bytes[8..12], &[0x01, 0x02, 0x03, 0x04]);
        // bytes 12..16: fraction_lost (0x80) << 24 | cumulative_lost (-1 → 0xFFFFFF)
        //            = 0x80FFFFFF
        assert_eq!(&bytes[12..16], &[0x80, 0xFF, 0xFF, 0xFF]);
        // bytes 16..20: extended_highest_seq = 0xAABBCCDD
        assert_eq!(&bytes[16..20], &[0xAA, 0xBB, 0xCC, 0xDD]);
        // bytes 20..24: jitter = 0x00112233
        assert_eq!(&bytes[20..24], &[0x00, 0x11, 0x22, 0x33]);
        // bytes 24..28: last_sr = 0x44556677
        assert_eq!(&bytes[24..28], &[0x44, 0x55, 0x66, 0x77]);
        // bytes 28..32: delay_since_last_sr = 0x8899AABB
        assert_eq!(&bytes[28..32], &[0x88, 0x99, 0xAA, 0xBB]);
        assert_eq!(bytes.len(), 32);
    }
}
