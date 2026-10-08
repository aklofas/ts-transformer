//! Test-only helpers for the `h264` domain binary: the RFC 6184 payloader
//! (shared from `fixtures/h264_payloader.rs`) plus an LCG PRNG for the
//! loss-soak test.
//!
//! The payloader is the generative partner to the hand-built spec-byte unit
//! tests: it produces standards-correct RTP packets so the integration tests
//! can exercise the full `H264Receiver` path without a real encoder.

pub use crate::fixtures::h264_payloader::*;

/// A minimal deterministic LCG PRNG for the loss-soak test.
///
/// Uses the parameters of Numerical Recipes (m=2^32, a=1664525, c=1013904223)
/// which produce a good distribution for 32-bit uniform output.
pub struct Lcg(u32);

impl Lcg {
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }

    /// Advance and return the next pseudo-random u32.
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    /// Return `true` with probability `p_drop` (0.0..=1.0).
    pub fn should_drop(&mut self, p_drop: f64) -> bool {
        // Scale: if next_u32() / 2^32 < p_drop → drop.
        let threshold = (p_drop * (u32::MAX as f64)) as u32;
        self.next_u32() < threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-NALU AU: verify that the emitted packet is a valid 12-byte header
    /// followed by the raw NALU and that M=1.
    #[test]
    fn single_nalu_au_fits_mtu() {
        let nalu = vec![0x65u8, 0xAA, 0xBB]; // IDR slice
        let aus = vec![(90_000u32, vec![nalu.clone()])];
        let pkts = packetize(&aus, 1400, 1, 0xDEAD, 96);
        assert_eq!(pkts.len(), 1);
        // M=1: byte 1 has bit 7 set.
        assert_eq!(pkts[0][1], 0x80 | 96);
        // Seq=1
        assert_eq!(u16::from_be_bytes([pkts[0][2], pkts[0][3]]), 1);
        // TS=90000
        assert_eq!(
            u32::from_be_bytes([pkts[0][4], pkts[0][5], pkts[0][6], pkts[0][7]]),
            90_000
        );
        // Payload is exactly the NALU.
        assert_eq!(&pkts[0][12..], &nalu[..]);
    }

    /// FU-A split: NALU larger than MTU becomes multiple packets.
    #[test]
    fn nalu_over_mtu_splits_into_fu_a() {
        // 10-byte NALU, MTU=5 (payload budget 5, frag budget 5-2=3 bytes per chunk).
        let nalu = vec![0x41u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let aus = vec![(1000u32, vec![nalu.clone()])];
        let pkts = packetize(&aus, 5, 1, 0xBEEF, 96);
        // body = nalu[1..] = 9 bytes, frag_size = 3 → ceil(9/3) = 3 packets.
        assert_eq!(pkts.len(), 3);
        // Only the last packet has M=1.
        assert_eq!(pkts[0][1] & 0x80, 0, "first FU packet M must be 0");
        assert_eq!(pkts[1][1] & 0x80, 0, "middle FU packet M must be 0");
        assert_eq!(pkts[2][1] & 0x80, 0x80, "last FU packet M must be 1");
        // S=1 on first, E=1 on last.
        let fu_ind = pkts[0][12];
        let fh0 = pkts[0][13];
        let fh2 = pkts[2][13];
        assert_eq!(fu_ind & 0x1F, 28); // type 28 = FU-A
        assert_eq!(fh0 & 0x80, 0x80, "S bit first packet");
        assert_eq!(fh0 & 0x40, 0, "E bit first packet must be 0");
        assert_eq!(fh2 & 0x80, 0, "S bit last packet must be 0");
        assert_eq!(fh2 & 0x40, 0x40, "E bit last packet");
        // NRI preserved: nalu[0] = 0x41 → NRI = (0x41 >> 5) & 0x03 = 2.
        assert_eq!((fu_ind >> 5) & 0x03, (nalu[0] >> 5) & 0x03);
    }

    /// LCG produces distinct values and should_drop proportion is approximately correct.
    #[test]
    fn lcg_drop_distribution() {
        let mut rng = Lcg::new(42);
        let n = 100_000;
        let p = 0.2;
        let dropped: u32 = (0..n).map(|_| u32::from(rng.should_drop(p))).sum();
        // Allow ±3% relative tolerance.
        let expected = (n as f64 * p) as u32;
        let delta = (dropped as i64 - expected as i64).unsigned_abs() as u32;
        assert!(delta < n / 20, "LCG drop rate {dropped}/{n} far from {p}");
    }
}
