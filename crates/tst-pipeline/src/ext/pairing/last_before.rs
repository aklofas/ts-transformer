//! Sample-and-hold (last-before-PTS) pairing state machine.
//!
//! Keeps the last [`LAST_BEFORE_DEPTH`] KLV samples. Each video event
//! pairs with the newest one whose PTS is at or before the video PTS
//! (and within freshness, if configured). Past-only by definition: a
//! future KLV (PTS > video PTS) cannot satisfy "before". The history
//! lets a reordered frame (a B-frame whose PTS is below the newest
//! KLV's) still find the sample that preceded it.

use super::types::{KlvSample, PairerOutput, VideoSample};
use std::collections::VecDeque;
use tst_core::mpegts::demux::DemuxEvent;

/// KLV samples kept for pairing reordered frames. Eight covers the
/// reorder depth of common GOP structures at typical KLV rates.
const LAST_BEFORE_DEPTH: usize = 8;

pub(super) struct LastBeforeState {
    video_pid: u16,
    klv_pid: u16,
    freshness_ticks: Option<i64>,
    /// Oldest at the front. An entry stays after it pairs; one that
    /// leaves (eviction or flush) without ever pairing is `UnpairedKlv`.
    history: VecDeque<KlvSlot>,
}

struct KlvSlot {
    sample: KlvSample,
    used: bool,
}

impl LastBeforeState {
    pub(super) fn new(video_pid: u16, klv_pid: u16, freshness_ticks: Option<i64>) -> Self {
        Self {
            video_pid,
            klv_pid,
            freshness_ticks,
            history: VecDeque::with_capacity(LAST_BEFORE_DEPTH + 1),
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
        self.history
            .drain(..)
            .filter(|slot| !slot.used)
            .map(|slot| PairerOutput::UnpairedKlv(slot.sample))
            .collect()
    }

    fn handle_video(&mut self, v: VideoSample) -> Vec<PairerOutput> {
        let freshness_ticks = self.freshness_ticks;
        let slot = self
            .history
            .iter_mut()
            .rev()
            .find(|s| s.sample.pts <= v.pts)
            .filter(|s| match freshness_ticks {
                Some(n) => super::pts_distance(v.pts, s.sample.pts) <= n,
                None => true,
            });
        match slot {
            Some(slot) => {
                slot.used = true;
                let klv = slot.sample.clone();
                vec![PairerOutput::Paired { video: v, klv }]
            }
            None => vec![PairerOutput::UnpairedVideo(v)],
        }
    }

    fn handle_klv(&mut self, k: KlvSample) -> Vec<PairerOutput> {
        self.history.push_back(KlvSlot {
            sample: k,
            used: false,
        });
        if self.history.len() > LAST_BEFORE_DEPTH
            && let Some(evicted) = self.history.pop_front()
            && !evicted.used
        {
            return vec![PairerOutput::UnpairedKlv(evicted.sample)];
        }
        Vec::new()
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

    #[test]
    fn a_reordered_frame_pairs_with_the_klv_before_it() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(0));
        s.feed(video_event(0));
        s.feed(klv_event(9000));
        s.feed(video_event(9000));
        let out = s.feed(video_event(3000));
        assert!(matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 0));
    }

    #[test]
    fn freshness_is_inclusive_at_the_bound() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(3000));
        s.feed(klv_event(0));
        assert!(matches!(
            &s.feed(video_event(3000))[..],
            [PairerOutput::Paired { .. }]
        ));
        assert!(matches!(
            &s.feed(video_event(3001))[..],
            [PairerOutput::UnpairedVideo(_)]
        ));
    }

    #[test]
    fn paired_when_klv_before_video_no_freshness() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(50));
        let out = s.feed(video_event(100));
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn unpaired_when_klv_after_video() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(150));
        let out = s.feed(video_event(100));
        assert!(matches!(&out[0], PairerOutput::UnpairedVideo(_)));
    }

    #[test]
    fn freshness_at_threshold_pairs() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(50));
        let _ = s.feed(klv_event(50));
        // delta == freshness → pair.
        let out = s.feed(video_event(100));
        assert!(matches!(&out[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn freshness_past_threshold_unpairs() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(50));
        let _ = s.feed(klv_event(50));
        // delta == freshness + 1 → unpaired.
        let out = s.feed(video_event(101));
        assert!(matches!(&out[0], PairerOutput::UnpairedVideo(_)));
    }

    #[test]
    fn evicted_unused_klv_emits_unpaired() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        // A newer KLV no longer displaces an older one: the older stays
        // available to reordered frames until it leaves the history.
        for i in 0..LAST_BEFORE_DEPTH as i64 {
            assert!(s.feed(klv_event(i * 50)).is_empty());
        }
        // One past the depth evicts the oldest (PTS 0), never used.
        let out = s.feed(klv_event(LAST_BEFORE_DEPTH as i64 * 50));
        assert_eq!(out.len(), 1);
        match &out[0] {
            PairerOutput::UnpairedKlv(k) => assert_eq!(k.pts.as_ticks(), 0),
            _ => panic!("expected UnpairedKlv(0), got {:?}", out[0]),
        }
    }

    #[test]
    fn evicted_used_klv_silent() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let _ = s.feed(video_event(10)); // marks PTS 0 used
        for i in 1..LAST_BEFORE_DEPTH as i64 {
            let _ = s.feed(klv_event(i * 50));
        }
        let out = s.feed(klv_event(LAST_BEFORE_DEPTH as i64 * 50));
        assert!(out.is_empty(), "expected silent eviction, got {:?}", out);
    }

    #[test]
    fn flush_emits_only_never_used_entries() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let _ = s.feed(video_event(10)); // uses PTS 0
        let _ = s.feed(klv_event(50));
        let _ = s.feed(klv_event(100));
        let _ = s.feed(video_event(120)); // uses PTS 100
        let out = s.flush();
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == 50),
            "expected only the never-used PTS 50, got {:?}",
            out
        );
    }

    #[test]
    fn sample_and_hold_reuses_klv_for_many_videos() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let p1 = s.feed(video_event(10));
        let p2 = s.feed(video_event(20));
        assert!(matches!(&p1[0], PairerOutput::Paired { .. }));
        assert!(matches!(&p2[0], PairerOutput::Paired { .. }));
    }

    #[test]
    fn flush_emits_unused_current_klv() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let out = s.flush();
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PairerOutput::UnpairedKlv(_)));
    }

    #[test]
    fn flush_silent_when_current_used() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let _ = s.feed(video_event(10));
        let out = s.flush();
        assert!(out.is_empty());
    }

    #[test]
    fn flush_idempotent() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let _ = s.flush();
        let out2 = s.flush();
        assert!(out2.is_empty());
    }

    #[test]
    fn freshness_check_no_panic_with_extreme_pts() {
        // `v.pts - s.sample.pts` with i64-extreme inputs overflows as a
        // direct subtraction. pts_distance
        // saturates instead.
        //
        // klv.pts = i64::MIN+10, video.pts = i64::MAX-10. The
        // `s.pts <= v.pts` gate passes (MIN+10 < MAX-10 in i64 order).
        // Old code: (MAX-10) - (MIN+10) overflows i64 (debug panic).
        // New code: pts_distance saturates to i64::MAX > freshness=1000
        //           → UnpairedVideo (correct: the gap is far too wide).
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(1000));
        let _ = s.feed(klv_event(i64::MIN + 10));
        let out = s.feed(video_event(i64::MAX - 10));
        assert!(
            matches!(&out[0], PairerOutput::UnpairedVideo(_)),
            "extreme PTS pair should emit UnpairedVideo without panic, got {:?}",
            out
        );
    }
}
