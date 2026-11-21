//! Arithmetic on RTP timestamps that reads no clock.

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
