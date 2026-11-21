//! 90 kHz monotonic timestamp source per RFC 3551 §6 Table 5.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Each `RtpTransport` instance owns one [`RtpClock`]. The clock is
//! sampled at sendto-time and converted to a 32-bit 90 kHz tick — RFC
//! 2250 §2.1 fixes the rate; the value wraps modulo 2^32 (~13.25 h).
//!
//! We use [`Instant::now`] rather than parsing PCR out of the inbound
//! TS bytes. RFC 2250 §2 explicitly allows non-PCR-derived timestamps;
//! receivers use the RTP timestamp for jitter/RTCP synchronization, not
//! for content playback timing (which the inner PES PTS owns).

use std::time::Instant;

/// Monotonic 90 kHz timestamp source.
///
/// Stored as `Instant`, sampled lazily via [`Self::now_ticks`].
#[derive(Debug, Clone, Copy)]
pub struct RtpClock {
    /// `Instant` corresponding to RTP timestamp `start_ticks`.
    epoch: Instant,
    /// Random offset for the timestamp at `epoch` per RFC 3550 §5.1
    /// (the initial value SHOULD be random to make known-plaintext
    /// attacks harder).
    start_ticks: u32,
}

impl RtpClock {
    /// Construct a new clock with `start_ticks` as the timestamp at
    /// "now" — typically a random u32 supplied by the caller.
    #[must_use]
    pub fn new(start_ticks: u32) -> Self {
        Self {
            epoch: Instant::now(),
            start_ticks,
        }
    }

    /// Sample the clock now and return the current 90 kHz tick.
    ///
    /// Saturates the elapsed-microsecond computation at `u64::MAX` —
    /// well over 500 millennia, so the cast is purely defensive.
    #[must_use]
    pub fn now_ticks(&self) -> u32 {
        let micros = self.epoch.elapsed().as_micros();
        // 90 kHz: 1 tick = 1/90_000 s = 11.111... us. micros * 90 / 1000.
        // Cast micros (u128) to u64 saturating-ly first to avoid overflow
        // in the multiplication.
        let micros_u64 = u64::try_from(micros).unwrap_or(u64::MAX);
        // Compute elapsed ticks; saturating mul is overkill but
        // documents the intent.
        let elapsed_ticks = micros_u64.saturating_mul(90) / 1_000;
        self.start_ticks.wrapping_add(elapsed_ticks as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn clock_starts_at_start_ticks() {
        let c = RtpClock::new(12345);
        // Immediately after construction, elapsed_ticks is small. We
        // want a generous upper bound to absorb GHA-shared-runner
        // scheduler latency (workspace precedent: timing tests in
        // tst-srt allow 500 ms - 3 s budgets) while still catching an
        // arithmetic-off-by-1000x bug (1.1 s would be 100_000 ticks).
        let observed = c.now_ticks();
        let delta = observed.wrapping_sub(12345);
        assert!(
            delta < 10_000,
            "elapsed > 10_000 ticks (~111 ms) on idle: delta={delta}"
        );
    }

    #[test]
    fn clock_advances_at_90khz() {
        // Compare RTP-tick delta against the actual wall-clock elapsed
        // (sampled with `Instant::now()`), NOT against the requested
        // 50 ms sleep. macOS arm64 CI runners regularly oversleep by
        // 4x (50 ms sleep → ~200 ms elapsed) under runner load — the
        // pre-fix `(3000..=6000)` window flaked on `Instant::elapsed`
        // returning ~17_500 ticks. Bench-against-elapsed makes the
        // test invariant: "ticks advance at 90 kHz of the actual time
        // we spent in this function," regardless of sleep accuracy.
        use std::time::Instant;
        let c = RtpClock::new(0);
        let wall_start = Instant::now();
        let t0 = c.now_ticks();
        sleep(Duration::from_millis(50));
        let t1 = c.now_ticks();
        let actual_micros = wall_start.elapsed().as_micros() as u64;
        let delta = u64::from(t1.wrapping_sub(t0));
        // 90 kHz: expected = elapsed_us * 90 / 1000.
        let expected = actual_micros.saturating_mul(90) / 1_000;
        // ±5% relative tolerance with a 200-tick (~2.2 ms) absolute
        // floor — that covers sub-microsecond skew between the
        // `wall_start` and `t0` samples, plus the `actual_micros`
        // / `t1` samples on the trailing edge.
        let tol = (expected / 20).max(200);
        let lo = expected.saturating_sub(tol);
        let hi = expected.saturating_add(tol);
        assert!(
            (lo..=hi).contains(&delta),
            "delta after {actual_micros}us = {delta} ticks; \
             expected ~{expected} ± {tol}",
        );
    }

    #[test]
    fn clock_wraps_at_u32_boundary() {
        // Start near the wrap boundary; after a small elapsed, should wrap.
        let c = RtpClock::new(u32::MAX - 10);
        let t0 = c.now_ticks();
        sleep(Duration::from_millis(5));
        let t1 = c.now_ticks();
        // After 5ms we've added ~450 ticks; t0 was near u32::MAX, so t1
        // wraps below t0.
        assert!(
            t1 < t0 || t1 < 1000,
            "expected wrap around u32::MAX boundary, got t0={t0} t1={t1}",
        );
    }
}
