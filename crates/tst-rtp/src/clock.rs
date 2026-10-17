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

/// Nearest-continuation unwrap of a 32-bit RTP timestamp into `i64`.
///
/// Each call takes the wrapping 32-bit delta from the previous raw value,
/// reinterprets it as a signed `i32` (the half-range rule: a step of less
/// than 2^31 ticks either way is the nearest continuation) and adds it to
/// the previous unwrapped value. The first call has no reference point, so
/// it anchors the chain at the raw value itself. Only differences between
/// unwrapped values carry meaning.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Unwrap32 {
    /// The last raw value and the unwrapped value it produced.
    last: Option<(u32, i64)>,
}

impl Unwrap32 {
    /// Unwrap `raw` against the chain, advancing it.
    pub(crate) fn unwrap(&mut self, raw: u32) -> i64 {
        let unwrapped = match self.last {
            None => i64::from(raw),
            Some((prev_raw, prev_unwrapped)) => {
                prev_unwrapped + i64::from(raw.wrapping_sub(prev_raw) as i32)
            }
        };
        self.last = Some((raw, unwrapped));
        unwrapped
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

    #[test]
    fn unwrap32_first_call_anchors_at_raw() {
        let mut u = Unwrap32::default();
        assert_eq!(u.unwrap(4_000_000_000), 4_000_000_000);
    }

    #[test]
    fn unwrap32_forward_wrap_continues_upward() {
        let mut u = Unwrap32::default();
        assert_eq!(u.unwrap(u32::MAX - 5), i64::from(u32::MAX) - 5);
        assert_eq!(u.unwrap(4), i64::from(u32::MAX) + 5);
        assert_eq!(u.unwrap(100), i64::from(u32::MAX) + 101);
    }

    #[test]
    fn unwrap32_backward_step_goes_down_and_back_across_the_wrap() {
        let mut u = Unwrap32::default();
        assert_eq!(u.unwrap(10), 10);
        assert_eq!(u.unwrap(u32::MAX - 9), -10);
        assert_eq!(u.unwrap(10), 10);
    }

    /// The delta rule against an independent reference: a random walk of
    /// signed steps below the half range must unwrap to its running sum.
    #[test]
    fn unwrap32_matches_a_running_sum_over_random_steps() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let start = (next() >> 32) as u32;
        let mut u = Unwrap32::default();
        let mut expect = i64::from(start);
        assert_eq!(u.unwrap(start), expect);
        let mut raw = start;
        for _ in 0..100_000 {
            let step = (next() % 4_000_000_000) as i64 - 2_000_000_000;
            expect += step;
            raw = raw.wrapping_add(step as u32);
            assert_eq!(u.unwrap(raw), expect);
        }
    }
}
