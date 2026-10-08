//! KLV-to-video alignment from RTCP sender reports (design spec §2 "Track
//! alignment").
//!
//! Each elementary track's RTP timestamp has its own random origin. The
//! video track is the master: its PTS line starts at zero at the first
//! emitted AU. KLV units carry a different, unrelated RTP clock and must be
//! placed on that same PTS line before they can be muxed alongside the
//! video. [`Aligner`] holds KLV units (bounded, drop-oldest) until an RTCP
//! sender report has arrived for both tracks, at which point each report's
//! `(ntp, rtp)` pair lets every held unit's RTP timestamp be converted to an
//! NTP instant and then to the video track's RTP clock at that instant. If
//! two seconds pass without both reports, alignment falls back to
//! first-packet coincidence instead (the held KLV queue's first unit lands
//! at the video origin's PTS 0) — [`ClockAlignment::Provisional`] rather
//! than [`ClockAlignment::SenderReport`]. A later report pair always
//! recomputes the mapping and may move KLV units' PTS as a result (no
//! continuity requirement for metadata); each such move ticks
//! [`Aligner::steps`].
//!
//! RTP timestamps are 32-bit and wrap; each track keeps its own
//! nearest-continuation unwrap state inside [`TrackClock`] so that math
//! done in `i64` ticks stays monotonic across a wrap.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::klv_depacketizer::KlvUnit;
use super::mount::ClockAlignment;
use crate::rtcp::SenderReport;

/// How long [`Aligner`] waits, from the first held KLV unit, for sender
/// reports on both tracks before falling back to first-packet coincidence.
pub(crate) const ALIGN_FALLBACK: Duration = Duration::from_secs(2);

/// Maximum number of KLV units [`Aligner`] holds while waiting for
/// alignment. Bounded, drop-oldest — see [`Aligner::on_klv_unit`].
pub(crate) const ALIGN_HOLD_MAX_UNITS: usize = 4096;

/// Per-track RTCP sender-report state plus 32-bit RTP timestamp unwrapping.
///
/// `sr` and `first_rtp` hold raw (not unwrapped) values straight off the
/// wire — they are each other's reference points over spans short enough
/// that a wrap between them is not a real concern, and keeping them raw
/// matches what [`SenderReport`] and the first observed unit actually said.
/// Continuity unwrapping (needed because a *held* unit's timestamp is
/// compared against the *previous* unit's, potentially long after both
/// arrived) lives in `last`.
#[derive(Debug, Default)]
pub(crate) struct TrackClock {
    /// This track's most recent RTCP sender report, as `(ntp 32.32, rtp)`.
    sr: Option<(u64, u32)>,
    /// Raw RTP timestamp of the first unit ever seen on this track — the
    /// anchor for first-packet-coincidence fallback.
    first_rtp: Option<u32>,
    /// Nearest-continuation unwrap state: the last raw value fed to
    /// [`Self::unwrap`] and the unwrapped `i64` it produced.
    last: Option<(u32, i64)>,
}

impl TrackClock {
    fn new() -> Self {
        Self {
            sr: None,
            first_rtp: None,
            last: None,
        }
    }

    /// Unwrap `raw` against this track's running continuity state using the
    /// half-range nearest-continuation rule (the 32-bit wrapping delta from
    /// the last call, reinterpreted as a signed `i32`), advancing that
    /// state. The first call for a track has no reference point, so it
    /// anchors the chain at `raw` itself.
    fn unwrap(&mut self, raw: u32) -> i64 {
        let unwrapped = match self.last {
            None => raw as i64,
            Some((prev_raw, prev_unwrapped)) => {
                let delta = raw.wrapping_sub(prev_raw) as i32;
                prev_unwrapped + delta as i64
            }
        };
        self.last = Some((raw, unwrapped));
        unwrapped
    }
}

/// Places KLV units on the video track's PTS line using RTCP sender
/// reports, with a first-packet-coincidence fallback. See the
/// [module docs](self).
pub(crate) struct Aligner {
    video: TrackClock,
    klv: TrackClock,
    /// RTP timestamp of the first emitted video AU (the depacketizer's PTS
    /// zero point). Recorded once, on the first [`Self::on_video_au`] call.
    video_rtp_origin: Option<u32>,
    /// KLV units held until alignment is known. Bounded at
    /// [`ALIGN_HOLD_MAX_UNITS`], drop-oldest.
    hold: VecDeque<KlvUnit>,
    /// When the first currently-held unit was received — the clock the
    /// [`ALIGN_FALLBACK`] window runs against.
    started: Option<Instant>,
    mode: ClockAlignment,
    steps: u64,
    /// `unwrap(klv_rtp) + offset == video_rtp` (both 90 kHz), once known —
    /// from a sender-report pair or the first-packet-coincidence fallback.
    offset: Option<i64>,
}

impl Aligner {
    pub(crate) fn new() -> Self {
        Self {
            video: TrackClock::new(),
            klv: TrackClock::new(),
            video_rtp_origin: None,
            hold: VecDeque::new(),
            started: None,
            mode: ClockAlignment::NotApplicable,
            steps: 0,
            offset: None,
        }
    }

    /// Record the video track's RTCP sender report and recompute the
    /// sender-report offset if the KLV track's report is also known.
    pub(crate) fn on_video_sr(&mut self, sr: &SenderReport) {
        self.video.sr = Some((sr.ntp_timestamp, sr.rtp_timestamp));
        self.recompute_sr_offset();
    }

    /// Record the KLV track's RTCP sender report and recompute the
    /// sender-report offset if the video track's report is also known.
    pub(crate) fn on_klv_sr(&mut self, sr: &SenderReport) {
        self.klv.sr = Some((sr.ntp_timestamp, sr.rtp_timestamp));
        self.recompute_sr_offset();
    }

    /// With both sender reports known, recompute
    /// `offset = (rtp_video_sr - rtp_klv_sr) + (ntp_klv_sr - ntp_video_sr) in ticks`.
    /// The first time an offset becomes known this establishes alignment
    /// (mode becomes [`ClockAlignment::SenderReport`]) without counting a
    /// step; a later report pair that changes an already-known offset
    /// replaces it and ticks [`Self::steps`].
    fn recompute_sr_offset(&mut self) {
        let (Some((ntp_v, rtp_v)), Some((ntp_k, rtp_k))) = (self.video.sr, self.klv.sr) else {
            return;
        };
        // NTP values are 32.32 fixed-point seconds; the delta in 90 kHz
        // ticks is computed in i128 to avoid overflow, then narrowed.
        let ntp_delta_ticks = (((ntp_k as i128) - (ntp_v as i128)) * 90_000) >> 32;
        let candidate = (rtp_v as i64 - rtp_k as i64) + ntp_delta_ticks as i64;
        match self.offset {
            Some(old) if old == candidate => {
                // Unchanged — still authoritative, no step.
                self.mode = ClockAlignment::SenderReport;
            }
            Some(_) => {
                self.offset = Some(candidate);
                self.mode = ClockAlignment::SenderReport;
                self.steps += 1;
            }
            None => {
                self.offset = Some(candidate);
                self.mode = ClockAlignment::SenderReport;
            }
        }
    }

    /// Record the video track's PTS zero point. Only the first call has any
    /// effect — later AUs don't move the origin.
    pub(crate) fn on_video_au(&mut self, rtp_timestamp: u32) {
        if self.video_rtp_origin.is_none() {
            self.video_rtp_origin = Some(rtp_timestamp);
        }
    }

    /// Queue a KLV unit; returns every unit now placeable, in order, as
    /// `(unit, pts_ticks)`. A unit is placeable once the video origin is
    /// known and either a sender-report pair or the two-second fallback has
    /// established an offset — at which point every held unit (this one
    /// included) is drained in one pass.
    pub(crate) fn on_klv_unit(&mut self, u: KlvUnit, now: Instant) -> Vec<(KlvUnit, i64)> {
        if self.klv.first_rtp.is_none() {
            self.klv.first_rtp = Some(u.rtp_timestamp);
        }
        if self.started.is_none() {
            self.started = Some(now);
        }
        if self.hold.len() >= ALIGN_HOLD_MAX_UNITS {
            self.hold.pop_front();
        }
        self.hold.push_back(u);
        self.release(now)
    }

    /// If no offset is known yet and [`ALIGN_FALLBACK`] has elapsed since
    /// the first held unit, establish the first-packet-coincidence offset
    /// (the first held unit lands at the video origin's PTS 0): `offset =
    /// video_rtp_origin - klv_first_rtp`. This is always the first
    /// establishment of an offset, so it never counts a step.
    fn maybe_engage_fallback(&mut self, now: Instant) {
        if self.offset.is_some() {
            return;
        }
        let (Some(started), Some(origin), Some(first_rtp)) =
            (self.started, self.video_rtp_origin, self.klv.first_rtp)
        else {
            return;
        };
        if now.saturating_duration_since(started) >= ALIGN_FALLBACK {
            self.offset = Some(origin as i64 - first_rtp as i64);
            self.mode = ClockAlignment::Provisional;
        }
    }

    /// Place everything currently held, if alignment is known. Shared by
    /// [`Self::on_klv_unit`], [`Self::poll`] and the test-only
    /// `on_klv_sr_then_release`/[`Self::drain`] flush path.
    fn release(&mut self, now: Instant) -> Vec<(KlvUnit, i64)> {
        self.maybe_engage_fallback(now);
        let (Some(origin), Some(offset)) = (self.video_rtp_origin, self.offset) else {
            return Vec::new();
        };
        let mut placed = Vec::with_capacity(self.hold.len());
        while let Some(u) = self.hold.pop_front() {
            let t_k = self.klv.unwrap(u.rtp_timestamp);
            let pts = t_k + offset - origin as i64;
            placed.push((u, pts));
        }
        placed
    }

    /// Place held units on elapsed time alone: the same step
    /// [`Self::on_klv_unit`] runs, without queueing a new unit, so the
    /// [`ALIGN_FALLBACK`] window can expire while only video arrives.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<(KlvUnit, i64)> {
        self.release(now)
    }

    pub(crate) fn mode(&self) -> ClockAlignment {
        self.mode
    }

    pub(crate) fn steps(&self) -> u64 {
        self.steps
    }

    /// Flush: place whatever is held, using the current mapping if one is
    /// already known, otherwise forcing the first-packet-coincidence
    /// fallback now (ignoring the [`ALIGN_FALLBACK`] window) so an
    /// end-of-stream flush doesn't lose units that never got a sender
    /// report. Still a no-op while the video origin is unknown — there is
    /// no line to place anything on.
    pub(crate) fn drain(&mut self) -> Vec<(KlvUnit, i64)> {
        if self.offset.is_none() {
            if let (Some(origin), Some(first_rtp)) = (self.video_rtp_origin, self.klv.first_rtp) {
                self.offset = Some(origin as i64 - first_rtp as i64);
                self.mode = ClockAlignment::Provisional;
            }
        }
        self.release(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sr(ntp_secs: u64, ntp_frac: u32, rtp: u32) -> crate::rtcp::SenderReport {
        crate::rtcp::SenderReport {
            ssrc: 1,
            ntp_timestamp: (ntp_secs << 32) | ntp_frac as u64,
            rtp_timestamp: rtp,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        }
    }
    fn unit(ts: u32) -> KlvUnit {
        KlvUnit {
            bytes: vec![1],
            rtp_timestamp: ts,
        }
    }

    impl Aligner {
        /// Test-only: `on_klv_sr` followed by the private "place everything
        /// placeable" step that [`Aligner::on_klv_unit`] also runs, so a
        /// test can observe a sender-report pair's effect on already-held
        /// units without feeding a new one.
        fn on_klv_sr_then_release(
            &mut self,
            sr: &SenderReport,
            now: Instant,
        ) -> Vec<(KlvUnit, i64)> {
            self.on_klv_sr(sr);
            self.release(now)
        }
    }

    #[test]
    fn units_are_held_until_both_reports_then_placed_on_the_video_line() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(90_000); // video origin = 90 000 (PTS 0)
        assert!(a.on_klv_unit(unit(500_000), t0).is_empty(), "held");
        assert_eq!(a.mode(), ClockAlignment::NotApplicable);
        // video SR: ntp 100.0 s ↔ rtp 180 000 ; klv SR: ntp 100.5 s ↔ rtp 500 000
        a.on_video_sr(&sr(100, 0, 180_000));
        let placed = a.on_klv_sr_then_release(&sr(100, 1 << 31, 500_000), t0); // helper = on_klv_sr + drain of now-placeable
        // klv rtp 500 000 is AT the klv SR instant = ntp 100.5 → video rtp at 100.5 = 180 000 + 45 000 = 225 000 → pts = 225 000 − 90 000 = 135 000
        assert_eq!(placed, vec![(unit(500_000), 135_000)]);
        assert_eq!(a.mode(), ClockAlignment::SenderReport);
        assert_eq!(a.steps(), 0);
    }

    #[test]
    fn fallback_after_two_seconds_without_reports() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(1_000);
        assert!(a.on_klv_unit(unit(7_000), t0).is_empty());
        let placed = a.on_klv_unit(unit(7_900), t0 + ALIGN_FALLBACK + Duration::from_millis(1));
        // first-packet coincidence: first klv unit ↔ pts 0; second is 900 ticks later
        assert_eq!(placed, vec![(unit(7_000), 0), (unit(7_900), 900)]);
        assert_eq!(a.mode(), ClockAlignment::Provisional);
    }

    #[test]
    fn poll_releases_held_units_after_the_fallback() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(1_000);
        assert!(a.on_klv_unit(unit(7_000), t0).is_empty());
        assert!(
            a.poll(t0 + ALIGN_FALLBACK - Duration::from_millis(1))
                .is_empty()
        );
        let placed = a.poll(t0 + ALIGN_FALLBACK + Duration::from_millis(1));
        assert_eq!(placed, vec![(unit(7_000), 0)]);
        assert_eq!(a.mode(), ClockAlignment::Provisional);
    }

    #[test]
    fn a_later_report_pair_replaces_the_mapping_and_counts_a_step() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(0);
        a.on_video_sr(&sr(10, 0, 0));
        a.on_klv_sr(&sr(10, 0, 0));
        assert_eq!(a.on_klv_unit(unit(900), t0), vec![(unit(900), 900)]);
        a.on_klv_sr(&sr(11, 0, 90_000 + 450)); // klv clock now reads 450 ticks ahead of where it should
        assert_eq!(a.steps(), 1);
        assert_eq!(
            a.on_klv_unit(unit(180_000 + 450), t0),
            vec![(unit(180_000 + 450), 180_000)]
        );
    }

    #[test]
    fn hold_queue_is_bounded_drop_oldest() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(0);
        for i in 0..(ALIGN_HOLD_MAX_UNITS as u32 + 5) {
            assert!(a.on_klv_unit(unit(i), t0).is_empty());
        }
        a.on_video_sr(&sr(1, 0, 0));
        let placed = a.on_klv_sr_then_release(&sr(1, 0, 0), t0);
        assert_eq!(placed.len(), ALIGN_HOLD_MAX_UNITS);
        assert_eq!(placed[0].0.rtp_timestamp, 5, "oldest five dropped");
    }

    #[test]
    fn rtp_timestamp_wrap_is_unwrapped() {
        let mut a = Aligner::new();
        let t0 = Instant::now();
        a.on_video_au(u32::MAX - 1000);
        a.on_video_sr(&sr(1, 0, u32::MAX - 1000));
        a.on_klv_sr(&sr(1, 0, u32::MAX - 1000));
        assert_eq!(
            a.on_klv_unit(unit(u32::MAX - 1000), t0),
            vec![(unit(u32::MAX - 1000), 0)]
        );
        assert_eq!(a.on_klv_unit(unit(2000), t0), vec![(unit(2000), 3001)]);
    }
}
