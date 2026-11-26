//! ST 0601 §6.3 16-bit running-sum checksum.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! For each byte at index `i`, contribute `byte << (8 * ((i + 1) % 2))` to a
//! `u16` accumulator with wrapping arithmetic. Even-indexed bytes go into the
//! high byte, odd-indexed bytes into the low byte.
//!
//! ST 0601 places this as Tag 1 (the last field of the local set) covering
//! all bytes from the 16-byte UL through the Tag 1 length byte (but not the
//! Tag 1 value itself).

/// Compute the ST 0601 16-bit running-sum checksum.
pub fn checksum_running_sum_16(buf: &[u8]) -> u16 {
    let mut bcc: u16 = 0;
    for (i, b) in buf.iter().enumerate() {
        let shift = 8 * (((i + 1) % 2) as u16);
        bcc = bcc.wrapping_add((*b as u16) << shift);
    }
    bcc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_zero() {
        assert_eq!(checksum_running_sum_16(&[]), 0);
    }

    #[test]
    fn single_byte_high_position() {
        // i=0 → shift=8 → high byte
        assert_eq!(checksum_running_sum_16(&[0x12]), 0x1200);
    }

    #[test]
    fn two_bytes_alternating() {
        // i=0 → high (0x12 << 8 = 0x1200), i=1 → low (0x34 << 0 = 0x0034)
        // sum = 0x1234
        assert_eq!(checksum_running_sum_16(&[0x12, 0x34]), 0x1234);
    }

    #[test]
    fn four_bytes_alternating() {
        // i=0 → 0xAA00, i=1 → 0x00BB, i=2 → 0xCC00, i=3 → 0x00DD
        // sum (wrapping) = 0xAA00 + 0x00BB + 0xCC00 + 0x00DD = 0x17798 → 0x7798 (mod 2^16)
        let v = checksum_running_sum_16(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(v, 0x7798);
    }

    #[test]
    fn wrapping_overflow() {
        // Many 0xFF bytes — verify wrapping doesn't panic and produces a deterministic value.
        let buf = [0xFFu8; 256];
        let v = checksum_running_sum_16(&buf);
        // 128 bytes contribute 0xFF00 each, 128 bytes contribute 0x00FF each.
        // sum = 128 * (0xFF00 + 0x00FF) = 128 * 0xFFFF = 0x7F_FF80 → 0xFF80 (mod 2^16)
        assert_eq!(v, 0xFF80);
    }

    #[test]
    fn bit_flip_detection() {
        let original = [0x06, 0x0E, 0x2B, 0x34, 0x12, 0x34, 0x56, 0x78];
        let baseline = checksum_running_sum_16(&original);
        for i in 0..original.len() {
            let mut flipped = original;
            flipped[i] ^= 0x01;
            assert_ne!(
                checksum_running_sum_16(&flipped),
                baseline,
                "bit flip at byte {i} not detected"
            );
        }
    }

    #[test]
    fn known_good_st0601_prefix() {
        // The 16-byte ST 0601 UL itself.
        let ul = [
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x13,
            0x00, 0x00,
        ];
        // Hand-computed: alternating high/low contributions.
        // High bytes (even i): 0x06 + 0x2B + 0x02 + 0x01 + 0x0E + 0x03 + 0x01 + 0x00 = 0x46 → 0x4600
        // Low bytes (odd i):  0x0E + 0x34 + 0x0B + 0x01 + 0x01 + 0x01 + 0x13 + 0x00 = 0x63 → 0x0063
        // sum = 0x4663
        assert_eq!(checksum_running_sum_16(&ul), 0x4663);
    }

    /// ST 0601.19 §8.1.1.2, "Sample Checksum Data": the 64 bits
    /// `060E 2B34 0200 81BB` sum to the final checksum `B4FD`.
    #[test]
    fn running_sum_matches_spec_sample() {
        let sample = [0x06, 0x0E, 0x2B, 0x34, 0x02, 0x00, 0x81, 0xBB];
        assert_eq!(checksum_running_sum_16(&sample), 0xB4FD);
    }
}
