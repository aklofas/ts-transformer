//! Nearest-PTS pairing state machine.
//!
//! Realtime mode: video events trigger immediate pairing against KLV
//! history. Buffered mode adds a bounded video buffer with lookahead
//! drain, and releases that buffer when the raw 33-bit PTS wraps.
//!
//! Both modes close out their state at an epoch boundary (a reconnect, or
//! a signalled discontinuity on the video PID, the KLV PID or their
//! program's PCR PID), ahead of the passed-through event: buffered videos
//! are classified against the history held so far, then its never-used
//! entries leave as `UnpairedKlv`, so the history held never pairs across
//! the boundary. The first KLV sample and the first video sample after a
//! signalled discontinuity may be the old-clock PES in flight and then
//! leave unpaired; a later old-clock sample is not marked (KLV muxed more
//! than one sample behind its video, with a new clock within the tolerance
//! of the old, can pair a second old-clock KLV sample with new frames).
//! [`EpochTracker`] has both rules.

use super::EpochTracker;
use super::types::{KlvSample, PairerOutput, VideoSample};
use std::collections::VecDeque;
use tst_core::mpegts::demux::DemuxEvent;

/// A PTS this far below the newest one seen is a raw 33-bit wrap, not
/// reordering: half the `2^33` PTS range.
const WRAP_JUMP_TICKS: i64 = 1 << 32;

/// Internal mode shape. The public [`super::PairerMode`]'s `Buffered`
/// variant carries a `Duration` (max arrival skew); we still pair
/// against a bounded video AU buffer internally. Two knobs are wired
/// onto the Buffered variant:
///   - `max_video_buffer`: count cap (memory-safety bound; mirrors
///     `PairerConfig::max_buffered_video`).
///   - `max_lag_ticks`: PTS-skew cap. A buffered video is force-released
///     once the newest observed KLV PTS is past `video.pts +
///     max_lag_ticks`. The public `Buffered { max_lag: Duration }`
///     keeps the "wait window" (max_lag) separate from the "match
///     window" (tolerance).
#[derive(Clone, Copy)]
pub(super) enum InternalMode {
    Realtime,
    Buffered {
        max_video_buffer: usize,
        max_lag_ticks: i64,
    },
}

pub(super) struct NearestState {
    video_pid: u16,
    klv_pid: u16,
    tolerance_ticks: i64,
    max_klv_history: usize,
    mode: InternalMode,
    klv_history: VecDeque<KlvEntry>,
    video_buffer: VecDeque<VideoSample>,
    /// Largest PTS fed on either route since the last wrap (Buffered
    /// mode only).
    newest_pts: Option<i64>,
    epochs: EpochTracker,
}

struct KlvEntry {
    sample: KlvSample,
    used: bool,
}

impl NearestState {
    pub(super) fn new(
        video_pid: u16,
        klv_pid: u16,
        tolerance_ticks: i64,
        max_klv_history: usize,
        mode: InternalMode,
    ) -> Self {
        Self {
            video_pid,
            klv_pid,
            tolerance_ticks,
            max_klv_history,
            mode,
            klv_history: VecDeque::with_capacity(max_klv_history),
            video_buffer: VecDeque::new(),
            newest_pts: None,
            epochs: EpochTracker::new(video_pid, klv_pid),
        }
    }

    pub(super) fn feed(&mut self, event: DemuxEvent) -> Vec<PairerOutput> {
        match super::classify(event, self.video_pid, self.klv_pid) {
            // An old-clock sample in flight leaves before `observe_pts`: its
            // PTS must not become the wrap reference of the new clock.
            super::EventClass::Video(v) => {
                if self.epochs.video_is_old(v.pts) {
                    return vec![PairerOutput::UnpairedVideo(v)];
                }
                let mut out = self.observe_pts(v.pts.as_ticks());
                out.extend(self.handle_video(v));
                out
            }
            super::EventClass::Klv(k) => {
                if self.epochs.klv_is_old(k.pts) {
                    return vec![PairerOutput::UnpairedKlv(k)];
                }
                let mut out = self.observe_pts(k.pts.as_ticks());
                out.extend(self.handle_klv(k));
                out
            }
            super::EventClass::Other(other) => {
                let mut out = if self.epochs.starts_epoch(&other) {
                    self.close_out()
                } else {
                    Vec::new()
                };
                out.push(PairerOutput::PassThrough(other));
                out
            }
        }
    }

    /// Ends the current time base: classifies every buffered video against
    /// the history held so far, then empties the history, its never-used
    /// entries leaving as `UnpairedKlv`. The order keeps a video from
    /// meeting KLV of the next time base.
    fn close_out(&mut self) -> Vec<PairerOutput> {
        let mut out = Vec::new();
        // Every Buffered feed already drains in-tolerance videos, so the
        // paired branch here is a safety net; in practice these come out
        // as `UnpairedVideo`.
        while let Some(v) = self.video_buffer.pop_front() {
            out.extend(self.match_video_against_history(v));
        }
        while let Some(entry) = self.klv_history.pop_front() {
            if !entry.used {
                out.push(PairerOutput::UnpairedKlv(entry.sample));
            }
        }
        self.newest_pts = None;
        out
    }

    /// Buffered mode: track the newest PTS and, when `pts` lies more
    /// than [`WRAP_JUMP_TICKS`] below it (both raw 33-bit values), close
    /// out the pre-wrap state before the event is handled. Without this
    /// a buffered pre-wrap video is never released: every post-wrap KLV
    /// sits far below it, so neither the match nor the wait window
    /// closes. Realtime buffers no video and is left alone.
    fn observe_pts(&mut self, pts: i64) -> Vec<PairerOutput> {
        if matches!(self.mode, InternalMode::Realtime) {
            return Vec::new();
        }
        let out = if let Some(newest) = self.newest_pts
            && (0..1 << 33).contains(&pts)
            && (0..1 << 33).contains(&newest)
            && newest - pts > WRAP_JUMP_TICKS
        {
            self.close_out()
        } else {
            Vec::new()
        };
        self.newest_pts = Some(self.newest_pts.map_or(pts, |n| n.max(pts)));
        out
    }

    pub(super) fn flush(&mut self) -> Vec<PairerOutput> {
        self.newest_pts = None;
        self.epochs.forget_clock();
        let mut out = self.drain_buffered(true);
        // Drain remaining KLV history; emit UnpairedKlv for any !used.
        while let Some(entry) = self.klv_history.pop_front() {
            if !entry.used {
                out.push(PairerOutput::UnpairedKlv(entry.sample));
            }
        }
        out
    }

    fn handle_video(&mut self, v: VideoSample) -> Vec<PairerOutput> {
        match self.mode {
            InternalMode::Realtime => self.match_video_against_history(v),
            InternalMode::Buffered {
                max_video_buffer,
                max_lag_ticks: _,
            } => {
                self.video_buffer.push_back(v);
                let mut out = self.drain_buffered(false);
                // Buffer overflow: force-emit the oldest with a
                // best-effort match.
                while self.video_buffer.len() > max_video_buffer {
                    let oldest = self.video_buffer.pop_front().unwrap();
                    out.extend(self.match_video_against_history(oldest));
                }
                out
            }
        }
    }

    fn handle_klv(&mut self, k: KlvSample) -> Vec<PairerOutput> {
        let mut out = Vec::new();
        self.klv_history.push_back(KlvEntry {
            sample: k,
            used: false,
        });
        if self.klv_history.len() > self.max_klv_history
            && let Some(evicted) = self.klv_history.pop_front()
            && !evicted.used
        {
            out.push(PairerOutput::UnpairedKlv(evicted.sample));
        }
        // In Buffered mode, the new KLV may complete a buffered video.
        if matches!(self.mode, InternalMode::Buffered { .. }) {
            out.extend(self.drain_buffered(false));
        }
        out
    }

    /// Drain the video buffer from oldest to newest. For each buffered
    /// video, decide one of three outcomes:
    ///   - Paired: best-match in history is within tolerance.
    ///   - UnpairedVideo: the wait window has closed
    ///     (`last_klv_pts > video.pts + max_lag_ticks`), or `force_all`
    ///     is set (flush path).
    ///   - Stop draining: future KLV may still match.
    ///
    /// The "wait window" uses `max_lag_ticks`, not `tolerance_ticks` —
    /// these are decoupled knobs: tolerance is the match window
    /// (|video_pts - klv_pts| considered a match), max_lag is the wait
    /// window (how long we hold a video looking for a match before
    /// giving up). The constructor clamps `max_lag_ticks >=
    /// tolerance_ticks`, so a video is never released before the full
    /// match window has been tested.
    ///
    /// `force_all = true` means "no future KLV will arrive" (flush
    /// path); every buffered video must be classified now.
    fn drain_buffered(&mut self, force_all: bool) -> Vec<PairerOutput> {
        let max_lag_ticks = match self.mode {
            // Realtime never enters this path with a populated buffer
            // (videos pair eagerly), but flush() may call drain_buffered
            // for symmetry. Using tolerance_ticks here is a safe no-op
            // since the buffer is empty.
            InternalMode::Realtime => self.tolerance_ticks,
            InternalMode::Buffered { max_lag_ticks, .. } => max_lag_ticks,
        };
        let mut out = Vec::new();
        let last_klv_pts = self.klv_history.back().map(|e| e.sample.pts.as_ticks());
        while let Some(v) = self.video_buffer.front() {
            // Best-match scan. Doesn't mutate; we only mutate via
            // match_video_against_history below if we choose Paired.
            let best = self
                .klv_history
                .iter()
                // saturating_sub + saturating_abs guard against i64 overflow when
                // PTS values approach the limit. H.222.0 §2.4.3.7
                // bounds the demuxer's per-event PTS at 0..(2^33 − 1) ≈ 9.55e9, so
                // saturation is defensive against non-conformant sources.
                .map(|e| {
                    e.sample
                        .pts
                        .as_ticks()
                        .saturating_sub(v.pts.as_ticks())
                        .saturating_abs()
                })
                .min();
            let in_tolerance = matches!(best, Some(d) if d <= self.tolerance_ticks);
            if in_tolerance {
                let v = self.video_buffer.pop_front().unwrap();
                out.extend(self.match_video_against_history(v));
                continue;
            }
            let window_closed = match last_klv_pts {
                // saturating_add caps at i64::MAX so the comparison becomes
                // `last > i64::MAX` (always false) — keep the video buffered
                // rather than force-emit under arithmetic overflow.
                Some(last) => last > v.pts.as_ticks().saturating_add(max_lag_ticks),
                None => false,
            };
            if force_all || window_closed {
                let v = self.video_buffer.pop_front().unwrap();
                out.push(PairerOutput::UnpairedVideo(v));
                continue;
            }
            break;
        }
        out
    }

    /// Linear-scan history for nearest-PTS KLV; emit Paired (and mark
    /// the entry used) if within tolerance, else UnpairedVideo. Equidistant
    /// entries go to the newer one (higher index in the FIFO), matching the
    /// spec's "most recently sent telemetry wins" rule: the scan walks
    /// oldest to newest and replaces the best on `<=`, so that rule depends
    /// on the traversal order.
    fn match_video_against_history(&mut self, v: VideoSample) -> Vec<PairerOutput> {
        let mut best: Option<(usize, i64)> = None;
        for (i, entry) in self.klv_history.iter().enumerate() {
            // saturating_sub + saturating_abs guards against i64 overflow
            // — see drain_buffered for the rationale.
            let dist = entry
                .sample
                .pts
                .as_ticks()
                .saturating_sub(v.pts.as_ticks())
                .saturating_abs();
            match best {
                None => best = Some((i, dist)),
                Some((_, best_dist)) if dist <= best_dist => best = Some((i, dist)),
                _ => {}
            }
        }
        match best {
            Some((i, dist)) if dist <= self.tolerance_ticks => {
                let entry = &mut self.klv_history[i];
                entry.used = true;
                let klv = entry.sample.clone();
                vec![PairerOutput::Paired { video: v, klv }]
            }
            _ => vec![PairerOutput::UnpairedVideo(v)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::OLD_CLOCK_WINDOW;
    use super::super::test_support::{af_discontinuity, cc_jump, program_map};
    use super::*;
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::demux::{MetadataKind, SamplePayload, StreamId, StreamKind, VideoCodec};
    use tst_core::shared::SharedBytes;

    const VIDEO_PID: u16 = 0x100;
    const KLV_PID: u16 = 0x102;

    fn video_event(pts: i64) -> DemuxEvent {
        DemuxEvent::Sample {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            pts: Pts90khz::new(pts),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: SharedBytes::from_vec(Vec::new()),
                random_access_indicator: false,
                av1_carriage: None,
            },
        }
    }

    fn klv_event(pts: i64) -> DemuxEvent {
        DemuxEvent::Metadata {
            stream: StreamId {
                pid: KLV_PID,
                kind: StreamKind::KlvAsync,
                program_number: 1,
            },
            pts: Pts90khz::new(pts),
            kind: MetadataKind::KlvAsync,
            payload: vec![0xAA, 0xBB],
        }
    }

    fn nearest_realtime() -> NearestState {
        NearestState::new(VIDEO_PID, KLV_PID, 100, 4, InternalMode::Realtime)
    }

    #[test]
    fn matching_pts_emits_paired() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(50));
        let out = s.feed(video_event(50));
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn delta_at_tolerance_accepts() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let out = s.feed(video_event(100)); // |delta| == tolerance
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn delta_one_past_tolerance_rejects() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let out = s.feed(video_event(101)); // |delta| == tolerance + 1
        assert!(matches!(&out[0], PairerOutput::UnpairedVideo(_)));
    }

    #[test]
    fn fifo_eviction_emits_unpaired_klv_when_unused() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let _ = s.feed(klv_event(1));
        let _ = s.feed(klv_event(2));
        let _ = s.feed(klv_event(3));
        // 5th KLV evicts the oldest (PTS=0) which was never used.
        let out = s.feed(klv_event(4));
        assert_eq!(out.len(), 1);
        match &out[0] {
            PairerOutput::UnpairedKlv(k) => assert_eq!(k.pts.as_ticks(), 0),
            _ => panic!("expected UnpairedKlv, got {:?}", out[0]),
        }
    }

    #[test]
    fn fifo_eviction_silent_when_used() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        // Use the KLV via a video pair.
        let _ = s.feed(video_event(0));
        // Fill remaining 3 slots.
        let _ = s.feed(klv_event(1));
        let _ = s.feed(klv_event(2));
        let _ = s.feed(klv_event(3));
        // 5th KLV evicts the oldest (PTS=0) which IS used → silent.
        let out = s.feed(klv_event(4));
        assert!(
            out.is_empty(),
            "expected no eviction emission, got {:?}",
            out
        );
    }

    #[test]
    fn video_before_any_klv_emits_unpaired_video() {
        let mut s = nearest_realtime();
        let out = s.feed(video_event(50));
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PairerOutput::UnpairedVideo(_)));
    }

    #[test]
    fn equidistant_tiebreak_prefers_newer_klv() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let _ = s.feed(klv_event(100));
        // Video at PTS=50: equidistant to both KLVs (delta=50). Newer
        // (PTS=100) wins per the spec rule.
        let out = s.feed(video_event(50));
        match &out[0] {
            PairerOutput::Paired { klv, .. } => assert_eq!(klv.pts.as_ticks(), 100),
            _ => panic!("expected Paired with klv.pts=100"),
        }
    }

    fn nearest_buffered(max_video_buffer: usize) -> NearestState {
        // Default test helper: max_lag_ticks = tolerance_ticks (100).
        // Mirrors pre-Phase-3 single-knob semantics so the existing
        // Buffered-mode tests remain calibrated against the same
        // window-close threshold.
        NearestState::new(
            VIDEO_PID,
            KLV_PID,
            100,
            4,
            InternalMode::Buffered {
                max_video_buffer,
                max_lag_ticks: 100,
            },
        )
    }

    fn nearest_buffered_with_lag(max_video_buffer: usize, max_lag_ticks: i64) -> NearestState {
        NearestState::new(
            VIDEO_PID,
            KLV_PID,
            100,
            4,
            InternalMode::Buffered {
                max_video_buffer,
                max_lag_ticks,
            },
        )
    }

    fn buffered_pairer(tolerance_ticks: i64, max_lag_ticks: i64) -> NearestState {
        NearestState::new(
            VIDEO_PID,
            KLV_PID,
            tolerance_ticks,
            4,
            InternalMode::Buffered {
                max_video_buffer: 8,
                max_lag_ticks,
            },
        )
    }

    #[test]
    fn a_raw_wrap_releases_the_frame_buffered_before_it() {
        let mut p = buffered_pairer(/*tolerance*/ 0, /*max_lag*/ 9000);
        p.feed(video_event((1 << 33) - 1000));
        // The post-wrap KLV arrives first: the wrap is seen on it.
        let mut out = p.feed(klv_event(0));
        out.extend(p.feed(video_event(0)));
        assert!(out.iter().any(
            |o| matches!(o, PairerOutput::UnpairedVideo(v) if v.pts.as_ticks() == (1 << 33) - 1000)
        ));
        assert!(
            out.iter().any(
                |o| matches!(o, PairerOutput::Paired { video, .. } if video.pts.as_ticks() == 0)
            )
        );
    }

    #[test]
    fn a_raw_wrap_seen_on_video_releases_the_pre_wrap_frame() {
        let mut p = buffered_pairer(0, 9000);
        p.feed(video_event((1 << 33) - 1000));
        let out = p.feed(video_event(0));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(v)] if v.pts.as_ticks() == (1 << 33) - 1000),
            "the post-wrap video must release the pre-wrap one, got {:?}",
            out
        );
        let out = p.feed(klv_event(0));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { video, .. }] if video.pts.as_ticks() == 0),
            "the post-wrap video must stay buffered and pair, got {:?}",
            out
        );
    }

    #[test]
    fn a_raw_wrap_clears_the_history() {
        let mut p = buffered_pairer(100, 9000);
        p.feed(klv_event((1 << 33) - 5000)); // never used
        p.feed(klv_event((1 << 33) - 950)); // pairs the next video at once (distance 50)
        p.feed(video_event((1 << 33) - 1000)); // pairs at once
        p.feed(video_event((1 << 33) - 500)); // buffered: 450 from the newest KLV
        let out = p.feed(klv_event(0));
        let unpaired_klv: Vec<i64> = out
            .iter()
            .filter_map(|o| match o {
                PairerOutput::UnpairedKlv(k) => Some(k.pts.as_ticks()),
                _ => None,
            })
            .collect();
        assert_eq!(unpaired_klv, vec![(1 << 33) - 5000], "got {:?}", out);
        assert!(out.iter().any(|o| matches!(o, PairerOutput::UnpairedVideo(v) if v.pts.as_ticks() == (1 << 33) - 500)), "got {:?}", out);
        // Only the post-wrap KLV is left.
        let out = p.flush();
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == 0),
            "got {:?}",
            out
        );
    }

    #[test]
    fn buffered_holds_video_until_klv_arrives() {
        let mut s = nearest_buffered(8);
        let out_video = s.feed(video_event(50));
        assert!(
            out_video.is_empty(),
            "video should buffer, got {:?}",
            out_video
        );
        let out_klv = s.feed(klv_event(40));
        // KLV at 40 is within tolerance of buffered video at 50; emit Paired.
        assert_eq!(out_klv.len(), 1);
        assert!(matches!(&out_klv[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn buffered_window_close_emits_unpaired_video() {
        let mut s = nearest_buffered(8);
        let _ = s.feed(video_event(50));
        // KLV at 200 is past video.pts(50) + tolerance(100) = 150, so
        // the window for video=50 has provably closed. The drain pass
        // triggered by the new KLV emits UnpairedVideo for the buffered
        // video.
        let out = s.feed(klv_event(200));
        // Expect exactly one UnpairedVideo (PTS=50). The KLV at 200
        // remains in history (not unpaired yet — could match a future
        // video at 150–250).
        assert_eq!(
            out.iter()
                .filter(|o| matches!(o, PairerOutput::UnpairedVideo(_)))
                .count(),
            1
        );
    }

    #[test]
    fn buffered_overflow_force_emits_oldest() {
        let mut s = nearest_buffered(2);
        let _ = s.feed(video_event(0));
        let _ = s.feed(video_event(50));
        // Third video pushes buffer to 3 > max=2; oldest force-emits
        // (no KLV in history → UnpairedVideo for PTS=0).
        let out = s.feed(video_event(100));
        // Only the oldest force-emits; the others stay buffered.
        let unpaired: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                PairerOutput::UnpairedVideo(v) => Some(v.pts.as_ticks()),
                _ => None,
            })
            .collect();
        assert_eq!(unpaired, vec![0]);
    }

    #[test]
    fn buffered_klv_completes_held_video() {
        // Video at 100 arrives first, KLV at 99 arrives second.  When
        // KLV arrives, drain_buffered sees |99-100|=1 ≤ tolerance(100)
        // and emits Paired.  The subsequent video at 201 stays buffered
        // (KLV@99 is outside its window, but the window isn't closed
        // yet — no later KLV has arrived past 201+100=301).
        let mut s = nearest_buffered(8);
        let _ = s.feed(video_event(100));
        // KLV arrival triggers drain; video@100 pairs immediately.
        let out = s.feed(klv_event(99));
        let paired_count = out
            .iter()
            .filter(|o| matches!(o, PairerOutput::Paired { .. }))
            .count();
        assert!(
            paired_count >= 1,
            "expected at least one Paired, got {:?}",
            out
        );
        // Subsequent video stays buffered — no KLV within its window yet.
        let out2 = s.feed(video_event(201));
        assert!(
            out2.is_empty(),
            "video@201 should still be buffered, got {:?}",
            out2
        );
    }

    #[test]
    fn buffered_max_lag_holds_video_within_window() {
        // tolerance=100, max_lag=500. Video@0 buffered. KLV@300 arrives:
        // |300-0|=300 > tolerance(100), so no match; window-close check
        // is `300 > 0+500` = false, so video stays buffered.
        let mut s = nearest_buffered_with_lag(8, 500);
        let _ = s.feed(video_event(0));
        let out = s.feed(klv_event(300));
        assert!(
            out.is_empty(),
            "video@0 should still be held under max_lag=500 (KLV@300 not yet past 500), got {:?}",
            out
        );
        // Also confirm the video is still in the buffer (didn't silently
        // emit) by checking no UnpairedVideo surfaced.
        assert!(
            !out.iter()
                .any(|o| matches!(o, PairerOutput::UnpairedVideo(_))),
        );
    }

    #[test]
    fn buffered_max_lag_force_emits_past_window() {
        // tolerance=100, max_lag=500. Video@0 buffered. Advance: KLV@600
        // arrives — |600-0|=600 > tolerance(100), and window-close
        // check is `600 > 0+500` = true. Force-emit UnpairedVideo.
        //
        // Calibration: the pre-Phase-3 single-knob check would have
        // fired at KLV@101 (`101 > 0 + 100`), so this test specifically
        // verifies that max_lag=500 WIDENS the wait window beyond
        // tolerance. To prove max_lag is the binding threshold (not
        // tolerance), we feed an intermediate KLV@250 first and assert
        // the video stays buffered (`250 > 0+500` = false), then
        // advance to KLV@600 and assert it force-emits.
        let mut s = nearest_buffered_with_lag(8, 500);
        let _ = s.feed(video_event(0));
        let out_mid = s.feed(klv_event(250));
        assert!(
            out_mid.is_empty(),
            "video@0 should still be held at KLV@250 (250 < max_lag=500), got {:?}",
            out_mid
        );
        let out_past = s.feed(klv_event(600));
        let unpaired_video_count = out_past
            .iter()
            .filter(|o| matches!(o, PairerOutput::UnpairedVideo(_)))
            .count();
        assert_eq!(
            unpaired_video_count, 1,
            "video@0 should force-emit once KLV@600 advances past max_lag=500, got {:?}",
            out_past
        );
    }

    #[test]
    fn flush_realtime_drains_unused_klv_history() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let _ = s.feed(klv_event(50));
        let out = s.flush();
        let unpaired_pts: Vec<i64> = out
            .iter()
            .filter_map(|o| match o {
                PairerOutput::UnpairedKlv(k) => Some(k.pts.as_ticks()),
                _ => None,
            })
            .collect();
        assert_eq!(unpaired_pts, vec![0, 50]);
    }

    #[test]
    fn flush_buffered_drains_video_buffer_with_best_effort_match() {
        let mut s = nearest_buffered(8);
        let _ = s.feed(video_event(1000)); // no KLV → buffered
        let out = s.flush();
        let upv = out
            .iter()
            .filter(|o| matches!(o, PairerOutput::UnpairedVideo(_)))
            .count();
        assert_eq!(upv, 1, "expected 1 UnpairedVideo on flush, got {:?}", out);
    }

    #[test]
    fn flush_idempotent() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let _ = s.flush();
        let out2 = s.flush();
        assert!(out2.is_empty());
    }

    #[test]
    fn feed_after_flush_works() {
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(0));
        let _ = s.flush();
        let _ = s.feed(klv_event(100));
        let out = s.feed(video_event(100));
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    // --- epochs ---

    #[test]
    fn a_reconnect_clears_the_history() {
        let mut s = nearest_realtime();
        assert!(s.feed(klv_event(90_000)).is_empty());
        let out = s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(
            matches!(
                &out[..],
                [
                    PairerOutput::UnpairedKlv(k),
                    PairerOutput::PassThrough(DemuxEvent::ReconnectDiscontinuity)
                ] if k.pts.as_ticks() == 90_000
            ),
            "got {out:?}"
        );
        // The replacement stream's first frame arrives before its first KLV.
        let out = s.feed(video_event(90_000));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(_)]),
            "got {out:?}"
        );
    }

    #[test]
    fn a_flag_on_the_video_klv_or_pcr_pid_clears_the_history() {
        const PCR_PID: u16 = 0x101;
        for flagged in [VIDEO_PID, KLV_PID, PCR_PID] {
            let mut s = nearest_realtime();
            s.feed(program_map(1, PCR_PID, &[VIDEO_PID, PCR_PID, KLV_PID]));
            s.feed(klv_event(90_000));
            let out = s.feed(af_discontinuity(flagged));
            assert!(
                matches!(
                    &out[..],
                    [
                        PairerOutput::UnpairedKlv(k),
                        PairerOutput::PassThrough(DemuxEvent::Discontinuity { .. })
                    ] if k.pts.as_ticks() == 90_000
                ),
                "flag on {flagged:#x}: got {out:?}"
            );
            // No frame preceded the flag, so none can be in flight: this one
            // is judged against the history alone.
            let out = s.feed(video_event(90_000));
            assert!(
                matches!(&out[..], [PairerOutput::UnpairedVideo(_)]),
                "flag on {flagged:#x}: got {out:?}"
            );
        }
    }

    #[test]
    fn a_used_entry_leaves_silently_at_an_epoch() {
        let mut s = nearest_realtime();
        s.feed(klv_event(0));
        s.feed(video_event(10)); // marks PTS 0 used
        let out = s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(
            matches!(&out[..], [PairerOutput::PassThrough(_)]),
            "got {out:?}"
        );
        let out = s.feed(video_event(20));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(_)]),
            "got {out:?}"
        );
    }

    #[test]
    fn the_first_klv_and_frame_after_a_flag_follow_the_in_flight_rule() {
        let near = 90_000 + OLD_CLOCK_WINDOW;

        // The old-clock KLV PES in flight, then a second cell of the same PES.
        let mut s = nearest_realtime();
        s.feed(klv_event(90_000));
        s.feed(af_discontinuity(VIDEO_PID));
        for _ in 0..2 {
            let out = s.feed(klv_event(near));
            assert!(
                matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == near),
                "got {out:?}"
            );
        }
        // It stayed out of the history.
        let out = s.feed(video_event(near));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(_)]),
            "got {out:?}"
        );
        // The next PES is on the new clock and pairs.
        s.feed(klv_event(900_000));
        let out = s.feed(video_event(900_000));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 900_000),
            "got {out:?}"
        );

        // One tick past the window the first KLV is on the new clock.
        let mut s = nearest_realtime();
        s.feed(klv_event(90_000));
        s.feed(af_discontinuity(KLV_PID));
        assert!(s.feed(klv_event(near + 1)).is_empty());
        let out = s.feed(video_event(near + 1));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == near + 1),
            "got {out:?}"
        );

        // The old-clock frame in flight completes after a new-clock KLV that
        // lies one tick from it.
        let mut s = nearest_realtime();
        s.feed(klv_event(900_000));
        s.feed(video_event(900_000));
        s.feed(af_discontinuity(KLV_PID));
        let old_frame = 900_000 + OLD_CLOCK_WINDOW;
        assert!(s.feed(klv_event(old_frame + 1)).is_empty());
        let out = s.feed(video_event(old_frame));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(v)] if v.pts.as_ticks() == old_frame),
            "got {out:?}"
        );
        // Only the first frame after the flag is judged.
        let out = s.feed(video_event(old_frame + 2));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == old_frame + 1),
            "got {out:?}"
        );
    }

    #[test]
    fn a_reconnect_leaves_nothing_in_flight() {
        let mut s = nearest_realtime();
        s.feed(klv_event(90_000));
        s.feed(video_event(90_000));
        s.feed(af_discontinuity(VIDEO_PID));
        s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(s.feed(klv_event(93_000)).is_empty());
        let out = s.feed(video_event(93_000));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { .. }]),
            "got {out:?}"
        );
    }

    #[test]
    fn flush_forgets_the_old_clock() {
        let mut s = nearest_realtime();
        s.feed(klv_event(90_000));
        s.feed(video_event(90_000));
        s.feed(af_discontinuity(VIDEO_PID));
        s.flush();
        assert!(s.feed(klv_event(93_000)).is_empty());
        let out = s.feed(video_event(93_000));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { .. }]),
            "got {out:?}"
        );

        // A flag after the flush has no earlier PTS to judge against.
        let mut s = nearest_realtime();
        s.feed(klv_event(90_000));
        s.feed(video_event(90_000));
        s.flush();
        s.feed(af_discontinuity(VIDEO_PID));
        assert!(s.feed(klv_event(93_000)).is_empty());
        let out = s.feed(video_event(93_000));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { .. }]),
            "got {out:?}"
        );
    }

    #[test]
    fn buffered_old_video_never_pairs_with_new_klv() {
        let mut p = buffered_pairer(/*tolerance*/ 100, /*max_lag*/ 9000);
        assert!(p.feed(klv_event(500)).is_empty());
        // Buffered: 500 ticks from the only KLV, inside the wait window.
        assert!(p.feed(video_event(1000)).is_empty());
        let out = p.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(
            matches!(
                &out[..],
                [
                    PairerOutput::UnpairedVideo(v),
                    PairerOutput::UnpairedKlv(k),
                    PairerOutput::PassThrough(DemuxEvent::ReconnectDiscontinuity)
                ] if v.pts.as_ticks() == 1000 && k.pts.as_ticks() == 500
            ),
            "got {out:?}"
        );
        // The replacement stream's KLV at the old frame's PTS finds no frame.
        assert!(p.feed(klv_event(1000)).is_empty());
        let out = p.flush();
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == 1000),
            "got {out:?}"
        );
    }

    #[test]
    fn buffered_video_held_at_a_flag_never_pairs_with_later_klv() {
        let mut p = buffered_pairer(/*tolerance*/ 100, /*max_lag*/ 9000);
        assert!(p.feed(klv_event(500)).is_empty());
        // Buffered: 500 ticks from the only KLV, inside the wait window.
        assert!(p.feed(video_event(1000)).is_empty());
        let out = p.feed(af_discontinuity(KLV_PID));
        assert!(
            matches!(
                &out[..],
                [
                    PairerOutput::UnpairedVideo(v),
                    PairerOutput::UnpairedKlv(k),
                    PairerOutput::PassThrough(DemuxEvent::Discontinuity { .. })
                ] if v.pts.as_ticks() == 1000 && k.pts.as_ticks() == 500
            ),
            "got {out:?}"
        );
        // The KLV PES in flight at the flag, which the held frame was
        // waiting for, completes on the old clock.
        let out = p.feed(klv_event(1000));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == 1000),
            "got {out:?}"
        );
        // The next sample is taken as the new clock's and finds no frame.
        assert!(p.feed(klv_event(1050)).is_empty());
        let out = p.flush();
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == 1050),
            "got {out:?}"
        );
    }

    #[test]
    fn an_old_sample_in_flight_does_not_become_the_wrap_reference() {
        // An old clock near the top of the 33-bit range, a new one near zero.
        let old = (1 << 33) - 1000;

        // The old frame completes after the new clock's first KLV. Taken as
        // the newest PTS, it would make the next new-clock frame look like a
        // wrap and throw that KLV out.
        let mut p = buffered_pairer(/*tolerance*/ 100, /*max_lag*/ 9000);
        p.feed(klv_event(old));
        p.feed(video_event(old));
        p.feed(af_discontinuity(KLV_PID));
        assert!(p.feed(klv_event(0)).is_empty());
        let out = p.feed(video_event(old + 500));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(v)] if v.pts.as_ticks() == old + 500),
            "got {out:?}"
        );
        let out = p.feed(video_event(50));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { video, klv }]
                if video.pts.as_ticks() == 50 && klv.pts.as_ticks() == 0),
            "got {out:?}"
        );

        // The old KLV completes after the new clock's first frame, which is
        // buffered and waits for its own KLV.
        let mut p = buffered_pairer(100, 9000);
        p.feed(klv_event(old));
        p.feed(video_event(old));
        p.feed(af_discontinuity(VIDEO_PID));
        assert!(p.feed(video_event(50)).is_empty());
        let out = p.feed(klv_event(old + 500));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == old + 500),
            "got {out:?}"
        );
        let out = p.feed(klv_event(60));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { video, klv }]
                if video.pts.as_ticks() == 50 && klv.pts.as_ticks() == 60),
            "got {out:?}"
        );
    }

    #[test]
    fn a_continuity_jump_keeps_the_history() {
        let mut s = nearest_realtime();
        s.feed(klv_event(50));
        let out = s.feed(cc_jump(VIDEO_PID));
        assert!(
            matches!(&out[..], [PairerOutput::PassThrough(_)]),
            "got {out:?}"
        );
        assert!(matches!(
            &s.feed(video_event(100))[..],
            [PairerOutput::Paired { .. }]
        ));
    }

    #[test]
    fn a_flag_on_another_pid_keeps_the_history() {
        let mut s = nearest_realtime();
        // Another program's PCR PID is not the paired program's.
        s.feed(program_map(1, VIDEO_PID, &[VIDEO_PID, KLV_PID]));
        s.feed(program_map(2, 0x201, &[0x201, 0x202]));
        s.feed(klv_event(50));
        for other in [0x200, 0x201] {
            let out = s.feed(af_discontinuity(other));
            assert!(
                matches!(&out[..], [PairerOutput::PassThrough(_)]),
                "flag on {other:#x}: got {out:?}"
            );
        }
        assert!(matches!(
            &s.feed(video_event(100))[..],
            [PairerOutput::Paired { .. }]
        ));
    }

    // --- PTS saturation regression tests ---

    #[test]
    fn near_i64_max_pts_does_not_overflow_buffered_drain() {
        // Pre-fix: drain_buffered computes `v.pts + max_lag_ticks` raw at
        // nearest.rs:200. For v.pts close to i64::MAX, the add overflows
        // (panic in debug, silent wrap in release).
        //
        // Setup: video far from any KLV (delta > tolerance=100, so the
        // pair branch is skipped and the window-close check at line 200
        // is the path executed), AND v.pts near i64::MAX so the add
        // (MAX-100) + 1000 overflows.
        let mut s = nearest_buffered_with_lag(8, 1000);
        // Buffered video at MAX-100; drain doesn't fire yet (no KLV).
        let _ = s.feed(video_event(i64::MAX - 100));
        // KLV far enough that |delta| = 4900 > tolerance=100 → skips
        // in-tolerance branch → reaches line 200 window-close check.
        // Pre-fix debug: PANIC at `v.pts + max_lag_ticks`.
        // Post-fix: saturating_add caps at i64::MAX; last (MAX-5000) is
        // NOT > MAX, so window stays open and video stays buffered.
        let _ = s.feed(klv_event(i64::MAX - 5000));
        // Surviving this call IS the assertion (no panic on the add).
    }

    #[test]
    fn near_i64_max_pts_does_not_overflow_realtime_match() {
        // Pre-fix: match_video_against_history computes
        // `(entry.sample.pts - v.pts).abs()` raw at nearest.rs:221. For
        // entry.pts and v.pts at opposite extremes of i64, the subtract
        // wraps (panic in debug, silent wrap in release).
        let mut s = nearest_realtime();
        // KLV at i64::MIN+100 (far negative — defensive; the demuxer
        // doesn't emit negative PTS in conformant streams, but a non-
        // conformant source could).
        let _ = s.feed(klv_event(i64::MIN + 100));
        // Video at i64::MAX-100 triggers match_video_against_history,
        // which subtracts (MIN+100) - (MAX-100) — overflows i64.
        // Pre-fix debug: PANIC.
        // Post-fix: saturating_sub.saturating_abs caps at i64::MAX; dist
        // far exceeds tolerance=100, so UnpairedVideo is emitted.
        let _ = s.feed(video_event(i64::MAX - 100));
        // Surviving this call IS the assertion.
    }

    // --- raw-first VideoSample ---

    /// A video event carrying known raw bytes + random_access_indicator=true.
    fn video_event_with_raw(pts: i64, raw: Vec<u8>, rai: bool) -> DemuxEvent {
        DemuxEvent::Sample {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            pts: Pts90khz::new(pts),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: SharedBytes::from_vec(raw),
                random_access_indicator: rai,
                av1_carriage: None,
            },
        }
    }

    #[test]
    fn raw_first_video_sample_carries_exact_raw_and_rai() {
        // Prove that paired VideoSample preserves raw + RAI verbatim, and that
        // split_units() drives an opt-in parse (the raw-first contract).
        //
        // Minimal H.264 AU: AUD (nal_type=9) + IDR (nal_type=5), Annex-B.
        let raw_bytes = vec![
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, // AUD
            0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC, // IDR
        ];
        let mut s = nearest_realtime();
        let _ = s.feed(klv_event(50));
        // Feed a video event with known raw bytes + RAI=true.
        let out = s.feed(video_event_with_raw(50, raw_bytes.clone(), true));
        assert_eq!(out.len(), 1);
        let vs = match &out[0] {
            PairerOutput::Paired { video, .. } => video,
            other => panic!("expected Paired, got {:?}", other),
        };

        // The raw bytes must be carried verbatim.
        assert_eq!(
            vs.raw.as_slice(),
            raw_bytes.as_slice(),
            "VideoSample.raw must equal the original encoded AU bytes"
        );

        // The RAI must be preserved.
        assert!(
            vs.random_access_indicator,
            "VideoSample.random_access_indicator must be preserved from the demux event"
        );

        // No av1_carriage for H.264.
        assert_eq!(vs.av1_carriage, None);

        // Opt-in parse: split_units must return a non-empty payload + no issues
        // for a well-formed H.264 AU.
        let (payload, issues) = vs.split_units();
        use tst_core::mpegts::demux::VideoPayload;
        match &payload {
            VideoPayload::Nals(nals) => {
                assert!(
                    !nals.is_empty(),
                    "split_units must return at least one NAL unit"
                );
            }
            other => panic!("expected Nals payload for H.264, got {:?}", other),
        }
        assert!(
            issues.is_empty(),
            "a well-formed AU should produce no issues, got {:?}",
            issues
        );
    }

    #[test]
    fn a_klv_sample_two_behind_its_video_can_pair_across_a_flag() {
        // KLV muxed two frames behind its video (30 fps), a flag on the
        // video PID, and a new clock 100 ms after the old one's last frame,
        // inside the 300 ms tolerance. The first old-clock KLV after the
        // flag is the one the in-flight rule catches; the second one is not
        // marked, enters the new history and pairs with new-clock frames.
        const FRAME: i64 = 3000;
        const OLD: i64 = 900_000;
        let new = OLD + 9 * FRAME + 9000;
        for mode in [
            InternalMode::Realtime,
            InternalMode::Buffered {
                max_video_buffer: 32,
                max_lag_ticks: 90_000,
            },
        ] {
            let mut s = NearestState::new(VIDEO_PID, KLV_PID, 300 * 90, 32, mode);
            for n in 0..10 {
                s.feed(video_event(OLD + n * FRAME));
                if n >= 2 {
                    s.feed(klv_event(OLD + (n - 2) * FRAME));
                }
            }
            let mut after = s.feed(af_discontinuity(VIDEO_PID));
            // The two old-clock KLV samples still in flight behind the flag.
            after.extend(s.feed(klv_event(OLD + 8 * FRAME)));
            after.extend(s.feed(klv_event(OLD + 9 * FRAME)));
            for n in 0..6 {
                after.extend(s.feed(video_event(new + n * FRAME)));
                if n >= 2 {
                    after.extend(s.feed(klv_event(new + (n - 2) * FRAME)));
                }
            }
            after.extend(s.flush());

            let crossed: Vec<(i64, i64)> = after
                .iter()
                .filter_map(|o| match o {
                    PairerOutput::Paired { video, klv } if video.pts.as_ticks() >= new => {
                        Some((video.pts.as_ticks(), klv.pts.as_ticks()))
                    }
                    _ => None,
                })
                .filter(|&(_, k)| k < new)
                .collect();
            assert_eq!(
                crossed,
                vec![
                    (new + FRAME, OLD + 9 * FRAME),
                    (new + 2 * FRAME, OLD + 9 * FRAME)
                ],
            );
        }
    }
}
