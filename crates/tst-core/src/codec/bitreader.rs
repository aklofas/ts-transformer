//! Minimal RBSP bit reader for hand-rolled H.265 / H.266 parameter set parsing.
//!
//! Codec-agnostic Annex-B reader operating on RBSP bytes with emulation
//! prevention bytes (`00 00 03`) preserved on input — `read_*` functions
//! transparently skip the `03` every time the previous two bytes were
//! `00 00`. This matches the input contract our public parsers expose
//! (raw NAL payload from the demuxer, no additional preprocessing).
//!
//! Used by `codec::h265::{vps,sps,pps,vui,short_term_rps,profile_tier_level}`
//! and `codec::h266::{vps,sps,pps,vui,profile_tier_level}`. AV1 has its own
//! separate bit-reading primitives in `codec::av1::decode::bitreader` —
//! different semantics (no emulation prevention bytes; AV1's `f(n)` /
//! `uvlc()` per spec §4.7 + §4.10).
//!
//! Reference: H.265 §7.2 (raw byte sequence payload), §9.2 (parsing process);
//! H.266 reuses the same bit-reading semantics.

use crate::codec::CodecParseError;

#[doc(hidden)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    /// Bit position within the input, counting bits skipped over EP bytes
    /// as if they were not there.
    bit_pos: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    pub fn position(&self) -> u32 {
        self.bit_pos
    }

    fn byte_at(&self, idx: usize) -> Option<u8> {
        self.bytes.get(idx).copied()
    }

    /// Read `n` bits (n ≤ 32). RBSP reading: if the previous two bytes
    /// were `00 00`, skip a single `03` byte before reading further.
    pub fn read_u(&mut self, n: u32) -> Result<u32, CodecParseError> {
        if n > 32 {
            return Err(CodecParseError::EngineError(format!("read_u({n}) > 32")));
        }
        let mut acc = 0u32;
        for _ in 0..n {
            acc = (acc << 1) | self.read_one_bit()? as u32;
        }
        Ok(acc)
    }

    pub fn read_bool(&mut self) -> Result<bool, CodecParseError> {
        Ok(self.read_one_bit()? != 0)
    }

    fn read_one_bit(&mut self) -> Result<u8, CodecParseError> {
        loop {
            let byte_idx = (self.bit_pos / 8) as usize;
            let bit_in_byte = self.bit_pos % 8;
            // EP-byte detection: at the start of a byte, if the prior two
            // bytes are 00 00 and the current byte is 03, skip it.
            if bit_in_byte == 0
                && byte_idx >= 2
                && self.bytes.get(byte_idx) == Some(&0x03)
                && self.bytes.get(byte_idx - 1) == Some(&0x00)
                && self.bytes.get(byte_idx - 2) == Some(&0x00)
            {
                self.bit_pos += 8;
                continue;
            }
            let b = self
                .byte_at(byte_idx)
                .ok_or(CodecParseError::TruncatedRbsp {
                    offset_bits: self.bit_pos,
                    needed_bits: 1,
                })?;
            let bit = (b >> (7 - bit_in_byte)) & 1;
            self.bit_pos += 1;
            return Ok(bit);
        }
    }

    /// Unsigned Exp-Golomb (ue(v)) per H.265 §9.2.2.
    pub fn read_ue(&mut self) -> Result<u32, CodecParseError> {
        let start = self.bit_pos;
        let mut zeros = 0u32;
        loop {
            // Guard at >= 32: codeNum = 2^zeros − 1 + suffix, where the
            // maximum representable codeNum in u32 is 2^32 − 1 (requires
            // zeros = 31, suffix = 2^31 − 1). zeros = 32 makes `1u32 << 32`
            // undefined behavior (panics in debug, wraps in release).
            if zeros >= 32 {
                return Err(CodecParseError::InvalidGolomb { offset_bits: start });
            }
            let b = self.read_one_bit()?;
            if b == 1 {
                break;
            }
            zeros += 1;
        }
        let suffix = if zeros == 0 { 0 } else { self.read_u(zeros)? };
        Ok((1u32 << zeros).saturating_sub(1).saturating_add(suffix))
    }

    /// Signed Exp-Golomb (se(v)) per H.265 §9.2.3.
    pub fn read_se(&mut self) -> Result<i32, CodecParseError> {
        let v = self.read_ue()?;
        // `v >> 1` is always <= i32::MAX, so the `as i32` cast can't truncate;
        // but the `+ 1` overflows when v == u32::MAX (debug-panic / release-wrap).
        // Saturate (the spec se(v) is unrepresentable for such adversarial input,
        // and codec parsers discard the value — this just avoids the panic on a
        // crafted ~62-bit Exp-Golomb codeword).
        Ok(if v & 1 == 1 {
            ((v >> 1) as i32).saturating_add(1)
        } else {
            -((v >> 1) as i32)
        })
    }

    /// Read an Exp-Golomb `ue(v)` and reject values above `max` (a
    /// spec-defined upper bound) as [`CodecParseError::ReservedValue`].
    ///
    /// Used for fields such as `sps_seq_parameter_set_id`, `chroma_sample_loc_type_*`,
    /// and `log2_max_frame_num_minus4` across H.264 / H.265 / H.266 parameter sets.
    /// The range check fires BEFORE any narrowing cast, preventing silent
    /// wrap-around of adversarial input (e.g. a crafted ue(v)=256 that would
    /// `as u8`-truncate to 0 — a valid value that aliases a different record).
    pub(crate) fn read_ue_max(
        &mut self,
        field: &'static str,
        max: u32,
    ) -> Result<u32, CodecParseError> {
        let v = self.read_ue()?;
        if v > max {
            return Err(CodecParseError::ReservedValue { field, value: v });
        }
        Ok(v)
    }

    pub fn skip(&mut self, n: u32) -> Result<(), CodecParseError> {
        for _ in 0..n {
            self.read_one_bit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u_basic() {
        let mut br = BitReader::new(&[0b10101010, 0b11110000]);
        assert_eq!(br.read_u(4).unwrap(), 0b1010);
        assert_eq!(br.read_u(4).unwrap(), 0b1010);
        assert_eq!(br.read_u(8).unwrap(), 0b11110000);
    }

    #[test]
    fn read_ue_zero_is_one_bit() {
        let mut br = BitReader::new(&[0b10000000]);
        assert_eq!(br.read_ue().unwrap(), 0);
    }

    #[test]
    fn read_ue_one_is_three_bits() {
        // 010 → 1
        let mut br = BitReader::new(&[0b01000000]);
        assert_eq!(br.read_ue().unwrap(), 1);
    }

    #[test]
    fn read_ue_seven_is_seven_bits() {
        // 0001000 → 7
        let mut br = BitReader::new(&[0b00010000]);
        assert_eq!(br.read_ue().unwrap(), 7);
    }

    #[test]
    fn read_se_basic() {
        // ue(0)=0 → se=0; ue(1)=1 → se=1; ue(2)=2 → se=-1
        let mut br = BitReader::new(&[0b10000000]);
        assert_eq!(br.read_se().unwrap(), 0);
        let mut br = BitReader::new(&[0b01000000]);
        assert_eq!(br.read_se().unwrap(), 1);
        let mut br = BitReader::new(&[0b01100000]);
        assert_eq!(br.read_se().unwrap(), -1);
    }

    #[test]
    fn ep_byte_skipped() {
        // Bytes: 00 00 03 FF → after skipping the 03, the FF is read.
        let mut br = BitReader::new(&[0x00, 0x00, 0x03, 0xff]);
        assert_eq!(br.read_u(8).unwrap(), 0);
        assert_eq!(br.read_u(8).unwrap(), 0);
        assert_eq!(br.read_u(8).unwrap(), 0xff);
    }

    #[test]
    fn truncated_input_errors() {
        let mut br = BitReader::new(&[0xff]);
        assert!(br.read_u(16).is_err());
    }

    #[test]
    fn invalid_golomb_long_zeros_errors() {
        let mut br = BitReader::new(&[0; 8]);
        assert!(matches!(
            br.read_ue(),
            Err(CodecParseError::InvalidGolomb { .. })
        ));
    }

    #[test]
    fn read_ue_max_rejects_above_max() {
        // ue(6) = "00111" → 0x38; rejecting at max=5 gives ReservedValue.
        let mut br = BitReader::new(&[0x38]);
        assert!(matches!(
            br.read_ue_max("f", 5),
            Err(CodecParseError::ReservedValue {
                field: "f",
                value: 6
            })
        ));
    }

    #[test]
    fn read_ue_max_accepts_at_max() {
        // ue(6) = "00111" → 0x38; max=6 is exactly at the bound.
        let mut br = BitReader::new(&[0x38]);
        assert_eq!(br.read_ue_max("f", 6).unwrap(), 6);
    }

    #[test]
    fn read_ue_32_leading_zeros_does_not_overflow() {
        // Per H.265 §9.2.1 Eq 9-2: codeNum = 2^leadingZeros − 1 + suffix.
        // With zeros = 32, codeNum requires (1u32 << 32) which is UB in u32:
        // panics in debug, wraps to 1 in release. The maximum representable
        // codeNum in u32 is 2^32 − 1, requiring zeros = 31 with all-ones
        // suffix yielding 2^32 − 2. zeros = 32 is unrepresentable.
        //
        // Bit stream: 32 zero bits + 1 marker bit + 32-bit suffix.
        // Bytes 0-3 = 0x00 (32 zeros), byte 4 = 0x80 (marker '1' + 7 zeros),
        // bytes 5-8 = 0x00 (suffix bits).
        let bytes = [0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00];
        let mut br = BitReader::new(&bytes);
        let result = br.read_ue();
        assert!(
            matches!(result, Err(CodecParseError::InvalidGolomb { .. })),
            "expected InvalidGolomb on 32-zero codeword, got {result:?}",
        );
    }
}
