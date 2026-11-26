//! AV1-specific bit reader. Per AV1 Bitstream Spec §4.7 / §4.10.
//!
//! Distinct from H.26x's Annex-B Exp-Golomb reader (which lives in
//! `crate::codec::bitreader`). AV1 has its own primitive set:
//!   * `f(n)` — fixed-width unsigned read (§4.7.2)
//!   * `uvlc()` — variable-length code (§4.10.3)
//!   * (LEB128 lives in [`super::leb128`].)
//!
//! AV1 OBU bodies do NOT use emulation-prevention bytes, so this
//! reader is byte-clean — no `00 00 03` skip logic like H.26x.

use crate::codec::CodecParseError;

#[doc(hidden)]
pub struct Av1BitReader<'a> {
    buf: &'a [u8],
    bit_pos: usize, // total bits consumed (MSB-first per byte)
}

impl<'a> Av1BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    /// `f(n)` per AV1 §4.7.2 — read n bits as unsigned.
    pub fn f(&mut self, n: usize) -> Result<u64, CodecParseError> {
        if n > 64 {
            return Err(CodecParseError::EngineError(format!(
                "f(n>64) not supported (n={n})"
            )));
        }
        let cap = self.buf.len().saturating_mul(8);
        let need = self.bit_pos.checked_add(n);
        if !matches!(need, Some(need) if need <= cap) {
            return Err(CodecParseError::TruncatedRbsp {
                offset_bits: u32::try_from(self.bit_pos).unwrap_or(u32::MAX),
                needed_bits: u32::try_from(n).unwrap_or(u32::MAX),
            });
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            let bit = (self.buf[byte_idx] >> bit_idx) & 1;
            v = (v << 1) | u64::from(bit);
            self.bit_pos += 1;
        }
        Ok(v)
    }

    /// `uvlc()` per AV1 §4.10.3.
    ///
    /// Read zero bits up to and including the terminating `1`; let `n` be
    /// the number of zeros. With `n >= 32` the result is `(1 << 32) - 1` and
    /// no value bits are read; otherwise read `n` bits as unsigned `extra`
    /// and return `(1 << n) - 1 + extra`. A buffer that ends before the `1`
    /// is `TruncatedRbsp`.
    pub fn uvlc(&mut self) -> Result<u64, CodecParseError> {
        let mut leading_zeros = 0usize;
        // Bounded by the buffer: `f(1)` fails at its end.
        while self.f(1)? == 0 {
            leading_zeros += 1;
        }
        if leading_zeros >= 32 {
            return Ok((1u64 << 32) - 1);
        }
        let extra = if leading_zeros == 0 {
            0
        } else {
            self.f(leading_zeros)?
        };
        Ok((1u64 << leading_zeros) - 1 + extra)
    }

    /// Test-only: force `bit_pos` to an arbitrary value so the
    /// overflow-guard path on `f()` can be exercised without looping.
    #[cfg(test)]
    pub(super) fn set_bit_pos_for_test(&mut self, pos: usize) {
        self.bit_pos = pos;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f_n_reads_msb_first() {
        let mut br = Av1BitReader::new(&[0b1010_0110]);
        assert_eq!(br.f(4).unwrap(), 0b1010);
        assert_eq!(br.f(4).unwrap(), 0b0110);
    }

    #[test]
    fn uvlc_zero() {
        // uvlc encoding of 0 is the single bit 1.
        let mut br = Av1BitReader::new(&[0b1000_0000]);
        assert_eq!(br.uvlc().unwrap(), 0);
    }

    #[test]
    fn uvlc_three() {
        // uvlc(3) = leading zeros count = 2; extra = 0b00; value = (1<<2) - 1 + 0 = 3.
        // Bitstream: 0 0 1 0 0 ... = 0b0010_0000
        let mut br = Av1BitReader::new(&[0b0010_0000]);
        assert_eq!(br.uvlc().unwrap(), 3);
    }

    #[test]
    fn uvlc_six() {
        // uvlc(6) = leading zeros count = 2; extra = 0b11; value = (1<<2) - 1 + 3 = 6.
        // Bitstream: 0 0 1 1 1 = 0b0011_1000
        let mut br = Av1BitReader::new(&[0b0011_1000]);
        assert_eq!(br.uvlc().unwrap(), 6);
    }

    #[test]
    fn truncated_returns_err() {
        let mut br = Av1BitReader::new(&[0xFF]);
        assert!(br.f(16).is_err());
    }

    #[test]
    fn uvlc_consumes_marker_bit_on_32_leading_zeros_overflow_sentinel() {
        // The uvlc loop must consume the trailing marker `1`-bit even when it
        // exits at `leading_zeros == 32`; otherwise a subsequent f(1) call
        // reads the marker, leaving cursors 1 bit short on every downstream
        // read.
        //
        // Stream layout: 32 leading zeros + 1 marker + sentinel-trailing bit
        // we'll read after uvlc(). 32 zero bits = 4 zero bytes. Then byte 4
        // bit 7 (MSB) is the marker `1`, bit 6 is the next caller bit (set to
        // `0` here so we can detect the off-by-one — pre-fix the caller would
        // read the marker bit `1` and post-fix gets the intended `0`).
        let buf = [0x00, 0x00, 0x00, 0x00, 0x80, 0xFF];
        let mut br = Av1BitReader::new(&buf);
        let v = br
            .uvlc()
            .expect("uvlc must succeed on 32-zero + marker form");
        assert_eq!(
            v,
            (1u64 << 32) - 1,
            "32-zero overflow returns spec sentinel"
        );
        // Cursor must now sit past the marker bit (33 bits in, byte 4 bit 6).
        // Pre-fix: cursor was at bit 32 and this read returned `1` (the
        // marker byte's MSB). Post-fix: cursor is at bit 33 and the next bit
        // is `0`.
        let next = br.f(1).expect("post-uvlc read must succeed");
        assert_eq!(
            next, 0,
            "post-uvlc cursor must be past the marker bit (pre-fix returned 1)"
        );
    }

    /// AV1 §4.10.3: `leadingZeros` counts every zero before the `1`, and 32
    /// or more give `(1 << 32) - 1` with no value bits read: the bit after
    /// the terminator is the caller's.
    #[test]
    fn uvlc_33_zeros_consumes_through_the_terminator() {
        // 33 zeros, the terminator, then `10` for the caller: a reader that
        // stops at 32 zeros, with or without taking one more bit, reads
        // `01` or `11` there.
        let mut br = Av1BitReader::new(&[0, 0, 0, 0, 0b0110_0000]);
        assert_eq!(br.uvlc().unwrap(), (1u64 << 32) - 1);
        assert_eq!(br.f(2).unwrap(), 0b10);
    }

    #[test]
    fn uvlc_31_zeros_reads_31_value_bits() {
        // 31 zeros, the terminator, 31 value bits of 1: 2^31 - 1 + 2^31 - 1.
        let bytes = [0, 0, 0, 0b0000_0001, 0xFF, 0xFF, 0xFF, 0xFE];
        let mut br = Av1BitReader::new(&bytes);
        assert_eq!(br.uvlc().unwrap(), (1u64 << 32) - 2);
        assert_eq!(br.f(1).unwrap(), 0);
    }

    #[test]
    fn uvlc_without_terminator_is_truncated() {
        for len in [4usize, 5, 8] {
            let r = Av1BitReader::new(&vec![0; len]).uvlc();
            assert!(
                matches!(r, Err(CodecParseError::TruncatedRbsp { .. })),
                "{len} zero bytes: got {r:?}"
            );
        }
    }

    #[test]
    fn av1_bitreader_overflow_safe_at_max_pos() {
        // With `bit_pos` near `usize::MAX`, the previous `bit_pos + n` add
        // wrapped around and the bounds check mis-fired, allowing a
        // panicking out-of-bounds slice access in the read loop. The
        // checked-add path must surface this as `TruncatedRbsp`.
        //
        // `n` must be ≤ 64 to skip the early `n > 64` guard and reach the
        // bounds check that we're testing.
        let mut br = Av1BitReader::new(&[0xFF; 1]);
        br.set_bit_pos_for_test(usize::MAX - 4);
        let result = br.f(64);
        assert!(matches!(result, Err(CodecParseError::TruncatedRbsp { .. })));
    }
}
