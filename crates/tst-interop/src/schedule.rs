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
/// (see [`build_schedule`]'s returned tuples). Audio (when a profile
/// carries it) runs on its own cadence — see [`AUDIO_FRAME_TICKS`] — not
/// paced 1:1 with video: real AAC framing at 1024 samples/frame and
/// [`AUDIO_SAMPLE_RATE_HZ`] runs at ~46.9 Hz, independent of the video
/// frame rate; `oracles::audio` checks both the frame count and the PTS
/// step against that real cadence.
pub(crate) enum Event {
    Video { frame_idx: u32 },
    Klv { seq: u32 },
    Audio { frame_idx: u32 },
}

/// Build the PTS-ordered event schedule for `seconds` of profile `p`'s
/// synthetic traffic. Returns `(start_pts_ticks, events)` — `start` is
/// needed alongside the schedule by wall-clock-paced callers to compute
/// each event's target offset from the run's own start.
///
/// PTS advances `90_000 / p.fps` ticks per video frame and `90_000 /
/// p.klv_hz` ticks per KLV record, both from `p.start_pts_ticks`. Every
/// event for the whole window is computed up front, then sorted into
/// ascending PTS order, so streams sharing a muxer/sender/publisher see
/// traffic in the same relative time order a live pipeline would
/// (rather than "all video, then all KLV").
///
/// `two-program` profiles are handled by the caller pushing the same
/// video AU / KLV record onto every configured program's handles at the
/// same PTS — this schedule itself is program-agnostic.
pub(crate) fn build_schedule(p: &Profile, seconds: f64) -> (i64, Vec<(i64, Event)>) {
    let video_step_ticks = (PTS_HZ / p.fps) as i64;
    let klv_step_ticks = (PTS_HZ / p.klv_hz) as i64;
    let video_count = (seconds * p.fps as f64).round() as u32;
    let klv_count = (seconds * p.klv_hz as f64).round() as u32;
    let start = p.start_pts_ticks as i64;

    let audio_count = if p.audio {
        (seconds * AUDIO_SAMPLE_RATE_HZ as f64 / 1024.0).round() as u32
    } else {
        0
    };

    let mut events: Vec<(i64, Event)> =
        Vec::with_capacity(video_count as usize + klv_count as usize + audio_count as usize);
    for i in 0..video_count {
        events.push((
            start + i as i64 * video_step_ticks,
            Event::Video { frame_idx: i },
        ));
    }
    for i in 0..klv_count {
        events.push((start + i as i64 * klv_step_ticks, Event::Klv { seq: i }));
    }
    for i in 0..audio_count {
        events.push((
            start + i as i64 * AUDIO_FRAME_TICKS,
            Event::Audio { frame_idx: i },
        ));
    }
    events.sort_by_key(|(pts_ticks, _)| *pts_ticks);
    (start, events)
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
            .iter()
            .filter(|(_, ev)| matches!(ev, Event::Audio { .. }))
            .map(|(pts, _)| *pts)
            .collect();

        assert_eq!(audio_pts.len(), 141);
        assert_eq!(audio_pts[0], start);
        for w in audio_pts.windows(2) {
            assert_eq!(w[1] - w[0], AUDIO_FRAME_TICKS);
        }
    }
}
