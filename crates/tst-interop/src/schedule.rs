//! Shared event schedule: the single place `gen.rs`, `send.rs`, and
//! `serve.rs` each build the PTS-ordered list of pushes for one
//! [`Profile`]'s synthetic traffic. Each consumes the same schedule
//! differently — `gen::run` writes it as fast as the muxer/file allow
//! (no sleeps), `send::send_over_transport` and `serve::run_hls`/
//! `run_rtsp` pace it to wall-clock time — but the shape of the
//! traffic itself (which events, at which PTS, in which order) is
//! defined in exactly one place.

use crate::profiles::Profile;

/// 90 kHz ticks per second — the MPEG-TS PTS clock (ITU-T H.222.0 V9
/// §2.4.3.6).
pub(crate) const PTS_HZ: u32 = 90_000;

/// AAC sample rate this crate's fixtures encode at (`fixtures::aac_frame`'s
/// ADTS header). Read by `profiles::invariants` as the expected rate for
/// `oracles::audio` and used below to derive the real per-frame cadence.
pub(crate) const AUDIO_SAMPLE_RATE_HZ: u32 = 48_000;

/// PTS ticks between consecutive AAC frames: `1024` samples/frame ×
/// `PTS_HZ` / [`AUDIO_SAMPLE_RATE_HZ`] = `1024 × 90000 / 48000`.
pub(crate) const AUDIO_FRAME_TICKS: i64 = 1920;

/// One scheduled push: a frame/record to push at its paired PTS tick
/// (see [`build_schedule`]'s yielded tuples). Audio (when a profile
/// carries it) runs on its own cadence — see [`AUDIO_FRAME_TICKS`] — not
/// paced 1:1 with video: real AAC framing at 1024 samples/frame and
/// [`AUDIO_SAMPLE_RATE_HZ`] runs at ~46.9 Hz, independent of the video
/// frame rate; `oracles::audio` checks both the frame count and the PTS
/// step against that real cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    Video { frame_idx: u32 },
    Klv { seq: u32 },
    Audio { frame_idx: u32 },
}

/// The PTS-ordered event stream for one window of a profile's traffic,
/// yielded lazily by [`build_schedule`]: three arithmetic sequences
/// (video, KLV, audio) merged on the fly, so the schedule costs a few
/// counters however long the window is. A 24 h `tst-interop send`
/// (`--seconds 86460`) used to materialise ~3.5 M events (~53 MB per
/// sender) up front — invisible in a 10-min stress sweep step, so the
/// stress hold's memory sizing, derived from sweep RSS, under-predicted
/// every hold sender by that much (R3-F1, 2026-10-03).
///
/// Order is exactly what the former `Vec` + stable `sort_by_key` gave:
/// ascending PTS, and on a shared tick video before KLV before audio,
/// each kind ascending by index.
pub(crate) struct Schedule {
    start: i64,
    video_step_ticks: i64,
    klv_step_ticks: i64,
    video_next: u32,
    video_count: u32,
    klv_next: u32,
    klv_count: u32,
    audio_next: u32,
    audio_count: u32,
}

impl Schedule {
    fn video_pts(&self) -> Option<i64> {
        (self.video_next < self.video_count)
            .then(|| self.start + self.video_next as i64 * self.video_step_ticks)
    }

    fn klv_pts(&self) -> Option<i64> {
        (self.klv_next < self.klv_count)
            .then(|| self.start + self.klv_next as i64 * self.klv_step_ticks)
    }

    fn audio_pts(&self) -> Option<i64> {
        (self.audio_next < self.audio_count)
            .then(|| self.start + self.audio_next as i64 * AUDIO_FRAME_TICKS)
    }
}

impl Iterator for Schedule {
    type Item = (i64, Event);

    fn next(&mut self) -> Option<Self::Item> {
        let video = self.video_pts();
        let klv = self.klv_pts();
        let audio = self.audio_pts();
        // The smallest pending PTS wins; testing video, then KLV, then
        // audio against it keeps the stable sort's tie order.
        let earliest = [video, klv, audio].into_iter().flatten().min()?;
        if video == Some(earliest) {
            let idx = self.video_next;
            self.video_next += 1;
            return Some((earliest, Event::Video { frame_idx: idx }));
        }
        if klv == Some(earliest) {
            let idx = self.klv_next;
            self.klv_next += 1;
            return Some((earliest, Event::Klv { seq: idx }));
        }
        let idx = self.audio_next;
        self.audio_next += 1;
        Some((earliest, Event::Audio { frame_idx: idx }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.video_count - self.video_next) as usize
            + (self.klv_count - self.klv_next) as usize
            + (self.audio_count - self.audio_next) as usize;
        (left, Some(left))
    }
}

impl ExactSizeIterator for Schedule {}

/// Build the PTS-ordered event schedule for `seconds` of profile `p`'s
/// synthetic traffic. Returns `(start_pts_ticks, events)` — `start` is
/// needed alongside the schedule by wall-clock-paced callers to compute
/// each event's target offset from the run's own start.
///
/// PTS advances `90_000 / p.fps` ticks per video frame and `90_000 /
/// p.klv_hz` ticks per KLV record, both from `p.start_pts_ticks`. The
/// events are yielded in ascending PTS order (see [`Schedule`] for the
/// tie rule), so streams sharing a muxer/sender/publisher see traffic in
/// the same relative time order a live pipeline would (rather than "all
/// video, then all KLV").
///
/// `two-program` profiles are handled by the caller pushing the same
/// video AU / KLV record onto every configured program's handles at the
/// same PTS — this schedule itself is program-agnostic.
pub(crate) fn build_schedule(p: &Profile, seconds: f64) -> (i64, Schedule) {
    let start = p.start_pts_ticks as i64;
    let audio_count = if p.audio {
        (seconds * AUDIO_SAMPLE_RATE_HZ as f64 / 1024.0).round() as u32
    } else {
        0
    };
    let schedule = Schedule {
        start,
        video_step_ticks: (PTS_HZ / p.fps) as i64,
        klv_step_ticks: (PTS_HZ / p.klv_hz) as i64,
        video_next: 0,
        video_count: (seconds * p.fps as f64).round() as u32,
        klv_next: 0,
        klv_count: (seconds * p.klv_hz as f64).round() as u32,
        audio_next: 0,
        audio_count,
    };
    (start, schedule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles;

    /// 3s of `audio` at the real 48 kHz/1024-sample AAC cadence: `round(3
    /// × 48000/1024) = round(140.625) = 141` frames, each 1920 ticks
    /// apart, first landing exactly on the schedule's start PTS.
    #[test]
    fn audio_events_run_at_the_aac_frame_cadence() {
        let p = profiles::by_name("audio").expect("audio profile must exist");
        let (start, events) = build_schedule(p, 3.0);

        let audio_pts: Vec<i64> = events
            .filter(|(_, ev)| matches!(ev, Event::Audio { .. }))
            .map(|(pts, _)| pts)
            .collect();

        assert_eq!(audio_pts.len(), 141);
        assert_eq!(audio_pts[0], start);
        for w in audio_pts.windows(2) {
            assert_eq!(w[1] - w[0], AUDIO_FRAME_TICKS);
        }
    }

    /// The reference order the schedule must reproduce: every event of
    /// the window materialised, then STABLY sorted by PTS — video before
    /// KLV before audio on a tie, ascending index within a kind. This is
    /// what `build_schedule` did before it became lazy (R3-F1), and what
    /// every receiver-side oracle was validated against.
    fn eager_reference(p: &Profile, seconds: f64) -> Vec<(i64, Event)> {
        let start = p.start_pts_ticks as i64;
        let video_count = (seconds * p.fps as f64).round() as u32;
        let klv_count = (seconds * p.klv_hz as f64).round() as u32;
        let audio_count = if p.audio {
            (seconds * AUDIO_SAMPLE_RATE_HZ as f64 / 1024.0).round() as u32
        } else {
            0
        };
        let mut events = Vec::new();
        for i in 0..video_count {
            let pts = start + i as i64 * (PTS_HZ / p.fps) as i64;
            events.push((pts, Event::Video { frame_idx: i }));
        }
        for i in 0..klv_count {
            let pts = start + i as i64 * (PTS_HZ / p.klv_hz) as i64;
            events.push((pts, Event::Klv { seq: i }));
        }
        for i in 0..audio_count {
            events.push((
                start + i as i64 * AUDIO_FRAME_TICKS,
                Event::Audio { frame_idx: i },
            ));
        }
        events.sort_by_key(|(pts, _)| *pts);
        events
    }

    /// The lazy merge yields exactly the eager-sorted sequence, ties
    /// included: at 30 fps / 10 Hz KLV every third video frame shares its
    /// PTS with a KLV record, and the `audio` profile adds a third stream
    /// whose 1920-tick cadence collides with both at the lcm.
    #[test]
    fn lazy_schedule_matches_the_eager_stable_sort() {
        for name in ["baseline", "audio", "two-program"] {
            let p = profiles::by_name(name).expect("profile must exist");
            let lazy: Vec<(i64, Event)> = build_schedule(p, 7.3).1.collect();
            let eager = eager_reference(p, 7.3);
            assert_eq!(lazy.len(), eager.len(), "{name}: event count");
            assert_eq!(lazy, eager, "{name}: order or payload differs");
        }
    }

    /// R3-F1: a 24 h sender must not carry its whole schedule in memory
    /// — the stress hold measured ~53 MB per sender for `--seconds 86460`
    /// against ~10 MB in a 660 s sweep step, which broke the hold's
    /// memory sizing. The schedule is a handful of counters whatever the
    /// window length.
    #[test]
    fn schedule_size_does_not_grow_with_the_window() {
        let p = profiles::by_name("audio").expect("audio profile must exist");
        let (_, day) = build_schedule(p, 86_460.0);
        assert!(
            std::mem::size_of_val(&day) <= 96,
            "{}",
            std::mem::size_of_val(&day)
        );
        assert_eq!(std::mem::size_of::<Schedule>(), std::mem::size_of_val(&day));
        // And it still counts right: 30 fps + 10 Hz KLV + 46.875 Hz audio.
        let n = build_schedule(p, 86_460.0).1.count();
        let audio = (86_460.0_f64 * AUDIO_SAMPLE_RATE_HZ as f64 / 1024.0).round() as usize;
        assert_eq!(n, 86_460 * 30 + 86_460 * 10 + audio);
    }
}
