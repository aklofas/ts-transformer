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

use crate::timestamp::ticks_after;

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
    #[must_use]
    pub fn now_ticks(&self) -> u32 {
        ticks_after(self.start_ticks, self.epoch.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tst_test_helpers::timing::scaled;

    /// The arithmetic is `ticks_after`'s; this is only that the clock
    /// feeds it its own elapsed time.
    #[test]
    fn the_clock_leaves_its_start_ticks_as_time_passes() {
        let c = RtpClock::new(12_345);
        let deadline = Instant::now() + scaled(Duration::from_secs(10));
        while c.now_ticks() == 12_345 {
            assert!(Instant::now() < deadline, "the clock never advanced");
            std::thread::yield_now();
        }
    }
}
