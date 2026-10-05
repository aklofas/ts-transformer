//! Nearest-PTS pairing state machine.
//!
//! Realtime mode: video events trigger immediate pairing against KLV
//! history. Buffered mode adds a bounded video buffer with lookahead
//! drain.

use super::types::{KlvSample, PairerOutput, VideoSample};
use std::collections::VecDeque;
use tst_core::mpegts::demux::DemuxEvent;

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
        }
    }

    pub(super) fn feed(&mut self, event: DemuxEvent) -> Vec<PairerOutput> {
        match super::classify(event, self.video_pid, self.klv_pid) {
            super::EventClass::Video(v) => self.handle_video(v),
            super::EventClass::Klv(k) => self.handle_klv(k),
            super::EventClass::Other(other) => vec![PairerOutput::PassThrough(other)],
        }
    }

    pub(super) fn flush(&mut self) -> Vec<PairerOutput> {
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
        if self.klv_history.len() > self.max_klv_history {
            if let Some(evicted) = self.klv_history.pop_front() {
                if !evicted.used {
                    out.push(PairerOutput::UnpairedKlv(evicted.sample));
                }
            }
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
    /// the entry used) if within tolerance, else UnpairedVideo. Tie-
    /// break on equidistant entries: prefer the newer (higher index in
    /// the FIFO), matching the spec's "most recently sent telemetry
    /// wins" rule.
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
                // Strict `<` so a later equidistant entry wins (newer
                // telemetry preferred). The loop walks oldest-to-newest,
                // so the last equidistant entry naturally wins via
                // `<=` — but using `<=` here makes the intent explicit
                // even if iteration order changes.
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
}
