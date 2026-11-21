//! The silent-peer rule of a managed sender: decides, from the instants of
//! its successful sends and the readings of the inner transport's
//! [`peer_activity`](tst_core::transport::Transport::peer_activity), when
//! the peer has answered none of the sends for a whole window.
//!
//! A pure type: it reads no clock, touches no transport, takes no lock.
//! Every instant comes in as an argument, so its tests feed synthetic ones.
//!
//! Every time difference saturates: an instant earlier than a stored one
//! reads as no time passed. The rule adds no durations, so no window can
//! overflow it.

use std::time::{Duration, Instant};

/// The rule's state for one inner transport. [`SilentPeer::reset`] when a
/// fresh inner is installed.
#[derive(Debug)]
pub(crate) struct SilentPeer {
    window: Duration,
    /// The last `Some` reading on this inner (cleared by a `None`).
    last_reading: Option<u64>,
    /// A `Some` reading was taken on this inner.
    some_seen: bool,
    /// This inner has answered, or cannot tell.
    answered: bool,
    /// When the last reading was taken.
    last_read: Option<Instant>,
    /// The last successful send.
    prev_send: Option<Instant>,
    /// The first successful send not known to be answered.
    anchor: Option<Instant>,
}

/// What a reading says about the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Healthy,
    /// The first sign of life from this inner: its first reading that
    /// differs from an earlier one, or a first reading that cannot tell.
    /// Given once per inner.
    Answered,
    /// No send since `unanswered` ago has been answered, and that span is
    /// at least the window.
    Silent {
        unanswered: Duration,
    },
}

impl SilentPeer {
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            last_reading: None,
            some_seen: false,
            answered: false,
            last_read: None,
            prev_send: None,
            anchor: None,
        }
    }

    pub(crate) fn window(&self) -> Duration {
        self.window
    }

    /// Called after every successful send. Returns whether a reading is due
    /// now: at every send until this inner has answered, then at most every
    /// quarter window, and always once the oldest unanswered send is a
    /// window old.
    ///
    /// A gap of half the window or more since the previous send restarts
    /// the count at this send: the peer may simply have had nothing to
    /// answer.
    pub(crate) fn sent(&mut self, now: Instant) -> bool {
        let restart = self
            .prev_send
            .is_some_and(|p| now.saturating_duration_since(p) >= self.window / 2);
        if self.anchor.is_none() || restart {
            self.anchor = Some(now);
        }
        self.prev_send = Some(now);
        !self.answered
            || self
                .last_read
                .is_none_or(|r| now.saturating_duration_since(r) >= self.window / 4)
            || self
                .anchor
                .is_some_and(|a| now.saturating_duration_since(a) >= self.window)
    }

    /// One reading of the inner's peer activity, taken at `now`.
    pub(crate) fn reading(&mut self, now: Instant, reading: Option<u64>) -> Verdict {
        self.last_read = Some(now);
        match reading {
            // The transport cannot tell: no run can be measured. A first
            // reading that cannot tell counts as this inner's answer, so a
            // sender over such a transport keeps its ordinary budget.
            None => {
                self.last_reading = None;
                self.anchor = None;
                if !self.answered && !self.some_seen {
                    self.answered = true;
                    Verdict::Answered
                } else {
                    Verdict::Healthy
                }
            }
            Some(v) if self.last_reading != Some(v) => {
                let had_reading = self.last_reading.is_some();
                self.last_reading = Some(v);
                self.some_seen = true;
                self.anchor = None;
                if had_reading && !self.answered {
                    self.answered = true;
                    Verdict::Answered
                } else {
                    Verdict::Healthy
                }
            }
            Some(_) => match self.anchor {
                Some(a) if now.saturating_duration_since(a) >= self.window => Verdict::Silent {
                    unanswered: now.saturating_duration_since(a),
                },
                _ => Verdict::Healthy,
            },
        }
    }

    /// A fresh inner: forget everything but the window.
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const W: Duration = Duration::from_millis(1500);
    const STEP: Duration = Duration::from_millis(33);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Synthetic instants: `at(d)` is a fixed base plus `d`.
    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Self(Instant::now())
        }
        fn at(&self, d: Duration) -> Instant {
            self.0 + d
        }
    }

    /// One send at `t`; when the rule asks for a reading, `value(t)` is read
    /// at the same instant. Returns the verdict of that reading, if taken.
    fn send(
        p: &mut SilentPeer,
        c: &Clock,
        t: Duration,
        value: &mut impl FnMut(Duration) -> Option<u64>,
    ) -> Option<Verdict> {
        if p.sent(c.at(t)) {
            Some(p.reading(c.at(t), value(t)))
        } else {
            None
        }
    }

    /// A send every `step` in `[from, to)`, reading whenever the rule asks.
    /// Returns every reading's time and verdict.
    fn drive(
        p: &mut SilentPeer,
        c: &Clock,
        from: Duration,
        to: Duration,
        step: Duration,
        mut value: impl FnMut(Duration) -> Option<u64>,
    ) -> Vec<(Duration, Verdict)> {
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            if let Some(v) = send(p, c, t, &mut value) {
                out.push((t, v));
            }
            t += step;
        }
        out
    }

    fn silent(trace: &[(Duration, Verdict)]) -> Vec<(Duration, Verdict)> {
        trace
            .iter()
            .copied()
            .filter(|(_, v)| matches!(v, Verdict::Silent { .. }))
            .collect()
    }

    fn answered(trace: &[(Duration, Verdict)]) -> usize {
        trace
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Answered))
            .count()
    }

    /// A reading that moves at every read.
    fn moving() -> impl FnMut(Duration) -> Option<u64> {
        let mut n = 0u64;
        move |_| {
            n += 1;
            Some(n)
        }
    }

    /// The first send at or after `t` on a grid of `step` from `from`.
    fn first_send_at_or_after(from: Duration, step: Duration, t: Duration) -> Duration {
        let mut s = from;
        while s < t {
            s += step;
        }
        s
    }

    /// An inner that has answered: two reads that differ, the second at 33 ms.
    fn answered_inner(c: &Clock) -> SilentPeer {
        let mut p = SilentPeer::new(W);
        assert!(p.sent(c.at(ms(0))));
        assert_eq!(p.reading(c.at(ms(0)), Some(1)), Verdict::Healthy);
        assert!(p.sent(c.at(ms(33))));
        assert_eq!(p.reading(c.at(ms(33)), Some(2)), Verdict::Answered);
        p
    }

    #[test]
    fn healthy_readings_never_trip() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let trace = drive(&mut p, &c, ms(0), ms(10_000), STEP, moving());
        assert!(silent(&trace).is_empty(), "{trace:?}");
        assert_eq!(trace[0].1, Verdict::Healthy);
        assert_eq!(trace[1], (ms(33), Verdict::Answered));
        assert_eq!(answered(&trace), 1);
        // Once answered, reads come every quarter window, not at every send:
        // a run whose anchor is never cleared would read at every send once
        // it is a window old.
        assert!(trace.len() < 40, "{} reads in 10 s", trace.len());
    }

    #[test]
    fn an_idle_pause_then_resumed_sends_do_not_trip() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let mut n = 0u64;
        let before = drive(&mut p, &c, ms(0), ms(2_000), STEP, |_| {
            n += 1;
            Some(n)
        });
        assert!(silent(&before).is_empty());
        // 10 s idle. The first reads after the resume still show the value
        // read before the pause: the peer's answer to the resumed sends has
        // not come yet. It comes 200 ms after the resume.
        let resume = ms(12_000);
        let frozen = n;
        let after = drive(&mut p, &c, resume, ms(15_000), STEP, |t| {
            if t < resume + ms(200) {
                Some(frozen)
            } else {
                n += 1;
                Some(n)
            }
        });
        assert_eq!(after[0], (resume, Verdict::Healthy));
        assert!(silent(&after).is_empty(), "{after:?}");
    }

    #[test]
    fn a_frozen_reading_trips_at_the_first_read_a_window_after_the_anchor() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let mute = ms(2_000);
        let mut n = 0u64;
        let mut last_change = None;
        let trace = drive(&mut p, &c, ms(0), ms(6_000), STEP, |t| {
            if t < mute {
                n += 1;
                last_change = Some(t);
            }
            Some(n)
        });
        let last_change = last_change.expect("reads before the mute");
        let anchor = last_change + STEP;
        let trip = first_send_at_or_after(ms(0), STEP, anchor + W);
        let s = silent(&trace);
        assert_eq!(
            s.first(),
            Some(&(
                trip,
                Verdict::Silent {
                    unanswered: trip - anchor
                }
            )),
            "{trace:?}"
        );
    }

    #[test]
    fn loss_spans_shorter_than_the_window_do_not_trip() {
        for still_us in [133_400u64, 159_400] {
            for window_ms in [500u64, 1_000, 1_500, 3_000] {
                let c = Clock::new();
                let mut p = SilentPeer::new(ms(window_ms));
                // The reading moves once every `still_us`, never between.
                let trace = drive(&mut p, &c, ms(0), ms(10_000), STEP, |t| {
                    Some(t.as_micros() as u64 / still_us)
                });
                assert!(
                    silent(&trace).is_empty(),
                    "still {still_us} us, W {window_ms} ms: {trace:?}"
                );
            }
        }
    }

    #[test]
    fn reset_forgets_the_run() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // A frozen run, 1.4 s old: anchored at 66 ms.
        let trace = drive(&mut p, &c, ms(66), ms(1_467), STEP, |_| Some(2));
        assert!(silent(&trace).is_empty());
        p.reset();
        // The fresh inner: its first reading is a first reading, even with
        // the old inner's value, and nothing of the run carries over.
        let t = ms(1_500);
        assert!(p.sent(c.at(t)));
        assert_eq!(p.reading(c.at(t), Some(2)), Verdict::Healthy);
        assert!(p.sent(c.at(t + STEP)), "a fresh inner reads at every send");
        assert_eq!(p.reading(c.at(t + STEP), Some(3)), Verdict::Answered);
    }

    #[test]
    fn no_reading_never_trips() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let trace = drive(&mut p, &c, ms(0), ms(10_000), STEP, |_| None);
        assert_eq!(trace[0].1, Verdict::Answered);
        assert!(
            trace[1..].iter().all(|(_, v)| *v == Verdict::Healthy),
            "{trace:?}"
        );
    }

    #[test]
    fn an_unanswered_last_message_before_a_pause_does_not_trip() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // t_r: the last read that moved.
        let t_r = ms(66);
        p.sent(c.at(t_r));
        assert_eq!(p.reading(c.at(t_r), Some(3)), Verdict::Healthy);
        // t_p: the last send before the pause, read, its answer never comes.
        let t_p = ms(99);
        p.sent(c.at(t_p));
        assert_eq!(p.reading(c.at(t_p), Some(3)), Verdict::Healthy);
        // A pause between W/2 and W, then sends with the reading frozen.
        let resume = t_p + ms(1_000);
        let trace = drive(&mut p, &c, resume, resume + ms(3_000), STEP, |_| Some(3));
        let trip = first_send_at_or_after(resume, STEP, resume + W);
        assert_eq!(
            silent(&trace).first(),
            Some(&(
                trip,
                Verdict::Silent {
                    unanswered: trip - resume
                }
            )),
            "{trace:?}"
        );
    }

    #[test]
    fn an_answer_read_before_a_pause_does_not_anchor_the_resume() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // The read at the last send before the pause moved.
        let t_p = ms(66);
        p.sent(c.at(t_p));
        assert_eq!(p.reading(c.at(t_p), Some(3)), Verdict::Healthy);
        // A pause shorter than W/2; the reading next moves at t_p + 1 600 ms.
        let resume = t_p + ms(700);
        let mut n = 3u64;
        let trace = drive(&mut p, &c, resume, resume + ms(4_000), STEP, |t| {
            if t >= t_p + ms(1_600) {
                n += 1;
            }
            Some(n)
        });
        assert!(silent(&trace).is_empty(), "{trace:?}");
    }

    #[test]
    fn a_sender_slower_than_half_the_window_never_trips() {
        for gap in [W / 2, ms(900), ms(5_000)] {
            let c = Clock::new();
            let mut p = SilentPeer::new(W);
            let trace = drive(&mut p, &c, ms(0), ms(60_000), gap, |_| Some(5));
            assert!(silent(&trace).is_empty(), "gap {gap:?}: {trace:?}");
        }
    }

    #[test]
    fn a_reading_that_moves_just_inside_the_window_never_trips() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let every = W - ms(1);
        let trace = drive(&mut p, &c, ms(0), ms(30_000), STEP, |t| {
            Some((t.as_nanos() / every.as_nanos()) as u64)
        });
        assert!(silent(&trace).is_empty(), "{trace:?}");
    }

    #[test]
    fn the_first_change_on_an_inner_is_its_answer() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let mut t = ms(0);
        let mut next = |p: &mut SilentPeer, v| {
            p.sent(c.at(t));
            let verdict = p.reading(c.at(t), Some(v));
            t += STEP;
            verdict
        };
        assert_eq!(next(&mut p, 0), Verdict::Healthy);
        assert_eq!(next(&mut p, 1), Verdict::Answered);
        assert_eq!(next(&mut p, 2), Verdict::Healthy);
        assert_eq!(next(&mut p, 3), Verdict::Healthy);
    }

    #[test]
    fn a_none_reading_ends_the_run() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // A run anchored at t0 = 66 ms on the frozen value 2.
        let t0 = ms(66);
        p.sent(c.at(t0));
        assert_eq!(p.reading(c.at(t0), Some(2)), Verdict::Healthy);
        let mut t = t0 + STEP;
        while t < t0 + W - ms(10) {
            p.sent(c.at(t));
            t += STEP;
        }
        assert_eq!(p.reading(c.at(t0 + W - ms(10)), None), Verdict::Healthy);
        let t1 = t0 + W + ms(10);
        p.sent(c.at(t1));
        assert_eq!(p.reading(c.at(t1), Some(2)), Verdict::Healthy);
    }

    #[test]
    fn a_transport_that_cannot_tell_counts_as_answered_once() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let mut verdicts = Vec::new();
        for (i, r) in [None, Some(0), Some(1), Some(2), None, Some(3), Some(4)]
            .into_iter()
            .enumerate()
        {
            let t = STEP * i as u32;
            p.sent(c.at(t));
            verdicts.push(p.reading(c.at(t), r));
        }
        assert_eq!(verdicts[0], Verdict::Answered);
        assert!(
            verdicts[1..].iter().all(|v| *v == Verdict::Healthy),
            "{verdicts:?}"
        );
    }

    #[test]
    fn an_earlier_instant_reads_as_no_time_passed() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        let t = ms(5_000);
        p.sent(c.at(t));
        assert_eq!(p.reading(c.at(t), Some(2)), Verdict::Healthy);
        // Every instant below is earlier than the stored send, anchor and
        // read.
        p.sent(c.at(ms(4_000)));
        assert_eq!(p.reading(c.at(ms(3_000)), Some(2)), Verdict::Healthy);
        p.sent(c.at(ms(100)));
        assert_eq!(p.reading(c.at(ms(50)), Some(2)), Verdict::Healthy);
    }

    #[test]
    fn extreme_windows_never_panic_or_trip() {
        for window in [Duration::from_nanos(1), Duration::MAX] {
            let c = Clock::new();
            let mut p = SilentPeer::new(window);
            assert_eq!(p.window(), window);
            let trace = drive(&mut p, &c, ms(0), ms(10_000), STEP, |_| Some(9));
            assert!(silent(&trace).is_empty(), "{window:?}: {trace:?}");
        }
    }

    #[test]
    fn any_change_of_the_reading_is_an_answer() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let values = [u64::MAX, 0, 5, 3, u64::MAX - 1, 1];
        let mut i = 0;
        let trace = drive(&mut p, &c, ms(0), ms(10_000), STEP, |_| {
            let v = values[i % values.len()];
            i += 1;
            Some(v)
        });
        assert_eq!(trace[1].1, Verdict::Answered, "u64::MAX then 0 is a change");
        assert!(silent(&trace).is_empty(), "{trace:?}");
    }

    #[test]
    fn reads_are_asked_every_send_until_answered_then_every_quarter_window() {
        let c = Clock::new();
        let mut p = SilentPeer::new(W);
        let mut n = 0u64;
        let mut read_at = Vec::new();
        for i in 0..100u32 {
            let t = STEP * i;
            if p.sent(c.at(t)) {
                n += 1;
                p.reading(c.at(t), Some(n));
                read_at.push(i);
            }
        }
        let mut expected = vec![0u32, 1];
        let mut last = STEP;
        for i in 2..100u32 {
            if STEP * i >= last + W / 4 {
                expected.push(i);
                last = STEP * i;
            }
        }
        assert_eq!(read_at, expected);

        // An inner that never answers is read at every send.
        let mut p = SilentPeer::new(W);
        for i in 0..40u32 {
            let t = STEP * i;
            assert!(p.sent(c.at(t)), "send {i}");
            p.reading(c.at(t), Some(1));
        }
    }

    #[test]
    fn a_still_reading_trips_at_exactly_the_window_and_not_a_millisecond_before() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // Anchored at 66 ms; sends every 33 ms, no reads, up to the window.
        let a = ms(66);
        let mut t = a;
        while t < a + W - ms(1) {
            p.sent(c.at(t));
            t += STEP;
        }
        p.sent(c.at(a + W - ms(1)));
        assert_eq!(p.reading(c.at(a + W - ms(1)), Some(2)), Verdict::Healthy);
        p.sent(c.at(a + W));
        assert_eq!(
            p.reading(c.at(a + W), Some(2)),
            Verdict::Silent { unanswered: W }
        );
    }

    /// Anchors a frozen run at 66 ms, sends at 166 ms, then pauses `gap`
    /// and sends every 33 ms with no read; returns the rule and the instant
    /// of the first send after the pause.
    fn run_then_pause(c: &Clock, gap: Duration) -> (SilentPeer, Duration) {
        let mut p = answered_inner(c);
        p.sent(c.at(ms(66)));
        p.sent(c.at(ms(166)));
        let resume = ms(166) + gap;
        p.sent(c.at(resume));
        (p, resume)
    }

    #[test]
    fn a_pause_of_exactly_half_the_window_restarts_the_anchor() {
        let c = Clock::new();
        let (mut p, resume) = run_then_pause(&c, W / 2);
        let mut t = resume + STEP;
        while t < resume + W - ms(1) {
            p.sent(c.at(t));
            t += STEP;
        }
        let t = resume + W - ms(1);
        p.sent(c.at(t));
        assert_eq!(p.reading(c.at(t), Some(2)), Verdict::Healthy);
    }

    #[test]
    fn a_pause_just_under_half_the_window_keeps_it() {
        let c = Clock::new();
        let (mut p, resume) = run_then_pause(&c, W / 2 - ms(1));
        let mut t = resume + STEP;
        while t < ms(66) + W {
            p.sent(c.at(t));
            t += STEP;
        }
        let t = ms(66) + W;
        p.sent(c.at(t));
        assert_eq!(
            p.reading(c.at(t), Some(2)),
            Verdict::Silent { unanswered: W }
        );
    }

    #[test]
    fn a_reading_still_for_just_under_the_window_then_moving_is_healthy() {
        let c = Clock::new();
        let mut p = answered_inner(&c);
        // Five silences of just under a window, each ended by one answer.
        let mut v = 2u64;
        let mut a = ms(66);
        for _ in 0..5 {
            let mut t = a;
            while t < a + W - ms(1) {
                if p.sent(c.at(t)) {
                    assert_eq!(p.reading(c.at(t), Some(v)), Verdict::Healthy);
                }
                t += STEP;
            }
            let t = a + W - ms(1);
            p.sent(c.at(t));
            assert_eq!(p.reading(c.at(t), Some(v)), Verdict::Healthy);
            v += 1;
            let t = a + W - ms(1) + STEP;
            p.sent(c.at(t));
            assert_eq!(p.reading(c.at(t), Some(v)), Verdict::Healthy);
            a = t + STEP;
        }
    }

    // ---- the property ----

    /// What one event of a random schedule reads after its send.
    #[derive(Debug, Clone, Copy)]
    enum Read {
        No,
        Same,
        Changed,
        CannotTell,
    }

    /// Recomputes, for the read at `k`, the earliest send `a` such that
    /// every read in `[a, k]` equalled the one before it (a `Some` with a
    /// `Some` before it), and every send in `(a, k]` came less than W/2
    /// after the previous one. The rule must answer `Silent` exactly when
    /// that span is at least W, with the span as `unanswered`.
    fn oracle(
        window: Duration,
        times: &[Duration],
        reads: &[Option<Option<u64>>],
        k: usize,
    ) -> Option<Duration> {
        let equal_to_previous = |j: usize| -> bool {
            let Some(Some(v)) = reads[j] else {
                return false;
            };
            match reads[..j].iter().rev().find_map(|r| *r) {
                Some(Some(prev)) => prev == v,
                _ => false,
            }
        };
        if !equal_to_previous(k) {
            return None;
        }
        let mut a = k;
        while a > 0
            && times[a] - times[a - 1] < window / 2
            && (reads[a - 1].is_none() || equal_to_previous(a - 1))
        {
            a -= 1;
        }
        let span = times[k] - times[a];
        (span >= window).then_some(span)
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        #[test]
        fn silent_only_after_a_window_of_unanswered_sends(
            window_us in 1u64..=2_000_000,
            obey in proptest::bool::ANY,
            events in proptest::collection::vec((0u64..=2_000, 0u8..4), 0..=400),
        ) {
            let window = Duration::from_micros(window_us);
            let c = Clock::new();
            let mut p = SilentPeer::new(window);
            let mut times = Vec::new();
            let mut reads: Vec<Option<Option<u64>>> = Vec::new();
            let mut verdicts = Vec::new();
            let mut now = Duration::ZERO;
            let mut value = 0u64;
            for (permille, kind) in events {
                now += Duration::from_micros(window_us * permille / 1_000);
                let kind = [Read::No, Read::Same, Read::Changed, Read::CannotTell][kind as usize];
                let asked = p.sent(c.at(now));
                // `obey`: read exactly when the rule asks, as the wrapper
                // does. Otherwise: whenever the schedule says so.
                let take = if obey { asked } else { !matches!(kind, Read::No) };
                let r = if take {
                    Some(match kind {
                        Read::No | Read::Same => Some(value),
                        Read::Changed => {
                            value = value.wrapping_add(1);
                            Some(value)
                        }
                        Read::CannotTell => None,
                    })
                } else {
                    None
                };
                times.push(now);
                reads.push(r);
                verdicts.push(r.map(|r| p.reading(c.at(now), r)));
            }
            for (k, verdict) in verdicts.iter().enumerate() {
                let Some(verdict) = *verdict else { continue };
                let expected = oracle(window, &times, &reads, k);
                match (verdict, expected) {
                    (Verdict::Silent { unanswered }, Some(span)) => {
                        proptest::prop_assert_eq!(unanswered, span, "read {}", k);
                    }
                    (Verdict::Silent { .. }, None) | (_, Some(_)) => {
                        proptest::prop_assert!(
                            false,
                            "read {}: verdict {:?}, oracle {:?}", k, verdict, expected
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}
