//! Sample-and-hold (last-before-PTS) pairing state machine.
//!
//! Keeps the last [`LAST_BEFORE_DEPTH`] KLV samples. Each video event
//! pairs with the one carrying the largest PTS at or before the video
//! PTS (and within freshness, if configured), or with nothing. Past-only by definition: a
//! future KLV (PTS > video PTS) cannot satisfy "before". The history
//! lets a reordered frame (a B-frame whose PTS is below the newest
//! KLV's) still find the sample that preceded it.
//!
//! The history is cleared at an epoch boundary: `ReconnectDiscontinuity`, or
//! `Discontinuity { kind: AdaptationFieldFlag }` on the video or KLV PID;
//! never-used entries leave as `UnpairedKlv` ahead of the passed-through event.
//!
//! A signalled discontinuity does not end the KLV PES in flight on the KLV
//! PID: it completes after the flag, on the old clock its header was coded
//! on (H.222.0 §2.4.3.5), whichever PID carried the flag. Only one PES is in
//! flight per PID, so only the first KLV PES completed after the flag can be
//! old. It is taken as old when its PTS lies within [`OLD_CLOCK_WINDOW`] of
//! the newest KLV PTS before the flag, and leaves as `UnpairedKlv` without
//! entering the history. A new clock that lands that close loses its first
//! KLV sample the same way: unpaired, never paired across clocks.

use super::types::{KlvSample, PairerOutput, VideoSample};
use std::collections::VecDeque;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, DiscontinuityKind};

/// KLV samples kept for pairing reordered frames. Eight covers the
/// reorder depth of common GOP structures at typical KLV rates.
const LAST_BEFORE_DEPTH: usize = 8;

/// How near the newest pre-flag KLV PTS the first KLV PES after a signalled
/// discontinuity must be to count as the old-clock PES in flight: two
/// seconds, past the sample interval of a 1 Hz KLV feed plus its jitter.
const OLD_CLOCK_WINDOW: i64 = 2 * 90_000;

/// Whether the next KLV sample may be the old-clock PES in flight.
#[derive(Clone, Copy)]
enum InFlight {
    /// Nothing old can be in flight.
    No,
    /// A discontinuity was signalled; holds the newest KLV PTS before it.
    Suspect(Pts90khz),
    /// The old-clock PES was found; its further cells share this PTS.
    Old(Pts90khz),
}

pub(super) struct LastBeforeState {
    video_pid: u16,
    klv_pid: u16,
    freshness_ticks: Option<i64>,
    /// Oldest at the front. An entry stays after it pairs; one that
    /// leaves (eviction or flush) without ever pairing is `UnpairedKlv`.
    history: VecDeque<KlvSlot>,
    /// PTS of the newest KLV sample since the last reconnect.
    last_klv_pts: Option<Pts90khz>,
    in_flight: InFlight,
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
            last_klv_pts: None,
            in_flight: InFlight::No,
        }
    }

    pub(super) fn feed(&mut self, event: DemuxEvent) -> Vec<PairerOutput> {
        match super::classify(event, self.video_pid, self.klv_pid) {
            super::EventClass::Video(v) => self.handle_video(v),
            super::EventClass::Klv(k) => self.handle_klv(k),
            super::EventClass::Other(other) => {
                if self.is_epoch_boundary(&other) {
                    // A reconnect resets the demuxer, so no PES survives it.
                    if matches!(other, DemuxEvent::ReconnectDiscontinuity) {
                        self.last_klv_pts = None;
                        self.in_flight = InFlight::No;
                    } else if let Some(pts) = self.last_klv_pts {
                        self.in_flight = InFlight::Suspect(pts);
                    }
                    let mut out = self.flush();
                    out.push(PairerOutput::PassThrough(other));
                    out
                } else {
                    vec![PairerOutput::PassThrough(other)]
                }
            }
        }
    }

    /// True for the events that start a new epoch: a transport reconnect, or
    /// a signalled discontinuity on the video or KLV PID. A `ContinuityJump`
    /// (packet loss) and any event on another PID are not epochs.
    fn is_epoch_boundary(&self, event: &DemuxEvent) -> bool {
        match event {
            DemuxEvent::ReconnectDiscontinuity => true,
            DemuxEvent::Discontinuity {
                stream,
                kind: DiscontinuityKind::AdaptationFieldFlag,
            } => stream.pid == self.video_pid || stream.pid == self.klv_pid,
            _ => false,
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
        // The latest-timestamped entry at or before the video, not the
        // latest-arrived: KLV may arrive out of PTS order. Ties go to the
        // newer arrival. Freshness is then checked on that entry alone.
        let freshness_ticks = self.freshness_ticks;
        let slot = self
            .history
            .iter_mut()
            .filter(|s| s.sample.pts <= v.pts)
            .reduce(|best, s| {
                if s.sample.pts >= best.sample.pts {
                    s
                } else {
                    best
                }
            })
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
        self.last_klv_pts = Some(k.pts);
        let old = match self.in_flight {
            InFlight::No => false,
            InFlight::Suspect(before) => super::pts_distance(k.pts, before) <= OLD_CLOCK_WINDOW,
            InFlight::Old(pts) => k.pts == pts,
        };
        if old {
            self.in_flight = InFlight::Old(k.pts);
            return vec![PairerOutput::UnpairedKlv(k)];
        }
        self.in_flight = InFlight::No;
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
    use tst_core::mpegts::demux::{
        DiscontinuityKind, MetadataKind, SamplePayload, StreamId, StreamKind, VideoCodec,
    };
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

    fn af_discontinuity(pid: u16) -> DemuxEvent {
        DemuxEvent::Discontinuity {
            stream: StreamId {
                pid,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            kind: DiscontinuityKind::AdaptationFieldFlag,
        }
    }

    fn cc_jump(pid: u16) -> DemuxEvent {
        DemuxEvent::Discontinuity {
            stream: StreamId {
                pid,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: 1,
            },
            kind: DiscontinuityKind::ContinuityJump {
                expected: 1,
                observed: 5,
            },
        }
    }

    #[test]
    fn a_reconnect_clears_the_old_clock_history() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        assert!(s.feed(klv_event(90_000)).is_empty());
        let out = s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedKlv(k), PairerOutput::PassThrough(DemuxEvent::ReconnectDiscontinuity)] if k.pts.as_ticks() == 90_000),
            "got {out:?}"
        );
        s.feed(klv_event(0));
        let out = s.feed(video_event(90_000));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 0),
            "got {out:?}"
        );
    }

    #[test]
    fn a_signalled_discontinuity_on_the_video_pid_clears_the_history() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(3000));
        s.feed(klv_event(90_000));
        let out = s.feed(af_discontinuity(VIDEO_PID));
        assert!(
            matches!(
                &out[..],
                [PairerOutput::UnpairedKlv(_), PairerOutput::PassThrough(_)]
            ),
            "got {out:?}"
        );
        // Freshness alone would have accepted the old sample at distance zero.
        let out = s.feed(video_event(90_000));
        assert!(
            matches!(&out[..], [PairerOutput::UnpairedVideo(_)]),
            "got {out:?}"
        );
    }

    #[test]
    fn a_signalled_discontinuity_on_the_klv_pid_clears_the_history() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(90_000));
        let out = s.feed(af_discontinuity(KLV_PID));
        assert!(
            matches!(
                &out[..],
                [PairerOutput::UnpairedKlv(_), PairerOutput::PassThrough(_)]
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn the_first_klv_near_the_old_clock_after_a_flag_stays_unpaired() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(90_000));
        s.feed(af_discontinuity(VIDEO_PID));
        // The old-clock PES in flight, then a second cell of the same PES.
        let near = 90_000 + OLD_CLOCK_WINDOW;
        for _ in 0..2 {
            let out = s.feed(klv_event(near));
            assert!(
                matches!(&out[..], [PairerOutput::UnpairedKlv(k)] if k.pts.as_ticks() == near),
                "got {out:?}"
            );
        }
        assert!(matches!(
            &s.feed(video_event(900_000))[..],
            [PairerOutput::UnpairedVideo(_)]
        ));
        // The next PES is on the new clock and pairs.
        s.feed(klv_event(900_000));
        assert!(matches!(
            &s.feed(video_event(903_000))[..],
            [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 900_000
        ));
    }

    #[test]
    fn the_first_klv_far_from_the_old_clock_after_a_flag_pairs() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(90_000));
        s.feed(af_discontinuity(KLV_PID));
        let far = 90_000 + OLD_CLOCK_WINDOW + 1;
        assert!(s.feed(klv_event(far)).is_empty());
        assert!(matches!(
            &s.feed(video_event(far))[..],
            [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == far
        ));
    }

    #[test]
    fn a_reconnect_leaves_no_klv_in_flight() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(90_000));
        s.feed(af_discontinuity(VIDEO_PID));
        s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(s.feed(klv_event(93_000)).is_empty());
        assert!(matches!(
            &s.feed(video_event(93_000))[..],
            [PairerOutput::Paired { .. }]
        ));
    }

    #[test]
    fn a_used_entry_leaves_silently_at_an_epoch() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(0));
        s.feed(video_event(10)); // marks PTS 0 used
        let out = s.feed(DemuxEvent::ReconnectDiscontinuity);
        assert!(
            matches!(&out[..], [PairerOutput::PassThrough(_)]),
            "got {out:?}"
        );
    }

    #[test]
    fn a_continuity_jump_keeps_the_history() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
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
    fn a_signalled_discontinuity_on_an_unrouted_pid_keeps_the_history() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(50));
        let out = s.feed(af_discontinuity(0x200));
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
    fn out_of_order_klv_pairs_with_the_latest_timestamp() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event(100));
        s.feed(klv_event(50));
        let out = s.feed(video_event(120));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 100),
            "got {:?}",
            out
        );
    }

    #[test]
    fn a_stale_late_arrival_does_not_hide_a_fresh_entry() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, Some(30));
        s.feed(klv_event(100));
        s.feed(klv_event(50));
        let out = s.feed(video_event(120));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == 100),
            "got {:?}",
            out
        );
    }

    #[test]
    fn a_pre_wrap_frame_does_not_pair_with_a_post_wrap_klv() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        s.feed(klv_event((1 << 33) - 1000));
        s.feed(klv_event(0));
        // A B-frame from before the wrap, delivered after the post-wrap KLV.
        let out = s.feed(video_event((1 << 33) - 200));
        assert!(
            matches!(&out[..], [PairerOutput::Paired { klv, .. }] if klv.pts.as_ticks() == (1 << 33) - 1000),
            "got {:?}",
            out
        );
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
    fn flush_emits_an_unused_klv() {
        let mut s = LastBeforeState::new(VIDEO_PID, KLV_PID, None);
        let _ = s.feed(klv_event(0));
        let out = s.flush();
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PairerOutput::UnpairedKlv(_)));
    }

    #[test]
    fn flush_silent_when_the_klv_was_used() {
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
