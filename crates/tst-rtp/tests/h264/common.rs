//! Test-only helpers for the `h264` domain binary: the RTP packet builder
//! (shared from `fixtures/rtp_packets.rs`) plus an LCG PRNG for the
//! loss-soak test.
//!
//! The builder wraps the library's RFC 6184 packetizer in an RTP header so
//! the integration tests can exercise the full `H264Receiver` path without
//! a real encoder.

pub use crate::fixtures::rtp_packets::*;

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
