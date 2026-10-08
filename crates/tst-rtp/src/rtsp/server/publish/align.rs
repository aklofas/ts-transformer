//! KLV-to-video alignment from RTCP sender reports (design spec §2 "Track
//! alignment").
//!
//! Each elementary track's RTP timestamp has its own random origin. The
//! video track is the master: its PTS line starts at the depacketizer's
//! PTS zero. KLV units carry a different, unrelated RTP clock and must be
//! placed on that same PTS line before they can be muxed alongside the
//! video. [`Aligner`] holds KLV units (bounded in units, bytes and age,
//! drop-oldest, every drop counted) until an RTCP
//! sender report has arrived for both tracks, at which point each report's
//! `(ntp, rtp)` pair lets every held unit's RTP timestamp be converted to an
//! NTP instant and then to the video track's RTP clock at that instant. If
//! two seconds pass without both reports, alignment falls back to
//! first-packet coincidence instead (the first KLV unit lands at the video
//! line's PTS 0) — [`ClockAlignment::Provisional`] rather than
//! [`ClockAlignment::SenderReport`]. A later report pair always recomputes
//! the mapping and may move KLV units' PTS as a result (no continuity
//! requirement for metadata); a move of more than
//! [`STEP_TOLERANCE_TICKS`] ticks [`Aligner::steps`].
//!
//! RTP timestamps are 32-bit and wrap. Each track keeps one
//! nearest-continuation unwrap chain in [`TrackClock`], and EVERY RTP value
//! the aligner uses goes through its track's chain in arrival order: KLV
//! units and KLV sender reports through the KLV chain, video sender reports
//! and the video origin through the video chain. All offset and PTS math
//! then happens in unwrapped `i64` ticks, so a wrap of either clock
//! mid-session moves nothing.
//!
//! A source restart on either track (a new SSRC, with a new random RTP
//! origin) invalidates that track's chain and report, and with them the
//! offset: [`Aligner::on_video_source_change`] and
//! [`Aligner::on_klv_source_change`] restart the track's chain, discard
//! the held units (counted in [`Aligner::dropped`]) and return alignment
//! to [`ClockAlignment::Pending`] until a fresh report pair or the
//! fallback re-establishes it. The fallback then anchors where the video
//! line is at the restart, not at its start. The adapter detects the SSRC
//! change and only feeds a track the sender reports of its current SSRC.
//!
//! A placed unit is not released at once: it waits until the video pushed
//! into the muxer has reached its PTS ([`Aligner::on_video_muxed`]). KLV
//! stamped at capture leads the encoder's output by the encoder latency,
//! and a KLV PES muxed ahead of the video drags the muxer's PCR (written
//! from the PTS of whatever is pushed) ahead of every video frame still to
//! come, so a PCR-slaved player shows those frames late. Placed units
//! leave in arrival order, so emission into the muxer stays in order.
//! The KLV PID's PTS can therefore step backwards (after a KLV restart
//! while units are waiting, or a backward report step), but it never
//! runs ahead of the muxed video. Placed units share the hold budgets
//! with the units still waiting for alignment, and a placed unit the
//! video has not reached [`PLACED_WAIT_MAX`] after its placement is
//! dropped and counted.
//! [`Aligner::drain`] releases every placed unit regardless.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::klv_depacketizer::KlvUnit;
use super::mount::ClockAlignment;
use crate::rtcp::SenderReport;

/// How long [`Aligner`] waits, from the oldest held KLV unit, for sender
/// reports on both tracks before falling back to first-packet coincidence.
pub(crate) const ALIGN_FALLBACK: Duration = Duration::from_secs(2);

/// How long a placed KLV unit waits for the video pushed into the muxer
/// to reach its PTS before it is dropped and counted. Separate from
/// [`ALIGN_FALLBACK`] and much longer: a KLV clock that disagrees with
/// the video clock by seconds (GPS-stamped KLV, encoder-clocked video) or
/// an encoder chain with seconds of latency puts KLV that far ahead, and
/// must not lose all its metadata. Memory stays bounded by
/// [`ALIGN_HOLD_MAX_UNITS`] and [`ALIGN_HOLD_MAX_BYTES`], and the PCR
/// guarantee is unchanged: a unit still leaves only once the video has
/// reached it (or is dropped).
pub(crate) const PLACED_WAIT_MAX: Duration = Duration::from_secs(10);

/// Maximum number of KLV units [`Aligner`] holds, waiting for alignment
/// and placed but waiting for the video together. Bounded, drop-oldest —
/// see [`Aligner::on_klv_unit`].
pub(crate) const ALIGN_HOLD_MAX_UNITS: usize = 4096;

/// Maximum total payload bytes [`Aligner`] holds, waiting for alignment
/// and placed but waiting for the video together, drop-oldest. With units capped at the muxer's KLV ceiling
/// (`MAX_KLV_UNIT_BYTES`, about 64 KiB) this is the bound that binds for
/// large units; [`ALIGN_HOLD_MAX_UNITS`] binds for small ones.
pub(crate) const ALIGN_HOLD_MAX_BYTES: usize = 4 * 1024 * 1024;

/// A new sender-report mapping that moves the KLV-to-video offset by at
/// most this many ticks (1 ms at 90 kHz) replaces the old one without
/// counting a step. Each report is sampled independently from the
/// sender's wall clock and RTP clock, so consecutive pairs routinely
/// differ by a tick or two; only a real move is worth counting.
pub(crate) const STEP_TOLERANCE_TICKS: i64 = 90;

/// A video PTS zero (`rtp − pts`) that moves by more than this many ticks
/// is a re-anchored depacketizer (a new video SSRC), not the same line.
const ORIGIN_TOLERANCE_TICKS: u32 = 1;

/// One track's RTP clock: its 32-bit unwrap chain and its latest RTCP
/// sender report, with the report's RTP value already unwrapped through
/// the chain.
#[derive(Debug, Default)]
pub(crate) struct TrackClock {
    /// This track's most recent RTCP sender report, as `(ntp 32.32,
    /// unwrapped rtp)`.
    sr: Option<(u64, i64)>,
    /// Nearest-continuation unwrap state: the last raw value fed to
    /// [`Self::unwrap`] and the unwrapped `i64` it produced.
    last: Option<(u32, i64)>,
}

impl TrackClock {
    fn new() -> Self {
        Self::default()
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

/// The video track's PTS zero point.
#[derive(Debug, Clone, Copy)]
struct VideoOrigin {
    /// `rtp − pts` as the depacketizer reported it, mod 2^32: how a
    /// re-anchored depacketizer is recognised.
    raw_zero: u32,
    /// The same zero in the video chain's unwrapped space.
    unwrapped: i64,
    /// Where first-packet coincidence puts the first KLV unit, in the
    /// video chain's unwrapped space: the zero itself (PTS 0, spec §2's
    /// `pts_k = t_k − t_k_first`) for the session's first origin, and the
    /// re-anchoring AU's own RTP time after a new video source, so the
    /// fallback lands KLV where the video line now is rather than at its
    /// start.
    fallback_anchor: i64,
}

/// A KLV unit waiting for alignment, with its RTP timestamp already
/// unwrapped through the KLV chain.
#[derive(Debug)]
struct Held {
    unit: KlvUnit,
    t_k: i64,
    /// When the unit was queued.
    at: Instant,
}

/// A KLV unit placed on the video line, waiting for the video pushed into
/// the muxer to reach its PTS.
#[derive(Debug)]
struct Placed {
    unit: KlvUnit,
    pts: i64,
    /// When the unit was placed: the clock its age bound runs against.
    at: Instant,
}

/// Places KLV units on the video track's PTS line using RTCP sender
/// reports, with a first-packet-coincidence fallback. See the
/// [module docs](self).
pub(crate) struct Aligner {
    video: TrackClock,
    klv: TrackClock,
    /// The depacketizer's PTS zero, from [`Self::on_video_au`].
    video_origin: Option<VideoOrigin>,
    /// Unwrapped RTP timestamp of the first KLV unit since the aligner
    /// started (or since the video line was re-anchored) — the anchor for
    /// first-packet-coincidence fallback.
    klv_first: Option<i64>,
    /// KLV units held until alignment is known. Bounded at
    /// [`ALIGN_HOLD_MAX_UNITS`] and [`ALIGN_HOLD_MAX_BYTES`], drop-oldest;
    /// while no video origin exists, also at [`ALIGN_FALLBACK`] of age.
    /// The oldest held unit's `at` is the clock the fallback window runs
    /// against.
    hold: VecDeque<Held>,
    /// Sum of `hold`'s payload lengths.
    held_bytes: usize,
    mode: ClockAlignment,
    steps: u64,
    /// `unwrap(klv_rtp) + offset == unwrap(video_rtp)` (both 90 kHz), once
    /// known — from a sender-report pair or the first-packet-coincidence
    /// fallback.
    offset: Option<i64>,
    /// Unplaced KLV units the aligner discarded.
    dropped: u64,
    /// A video source restart cleared `video_origin`: the next origin
    /// is a re-anchor, so its fallback anchor is that AU's own RTP time.
    reanchor_next: bool,
    /// Highest PTS [`Self::on_video_au`] has seen: where the video line
    /// is, for re-anchoring the fallback after a KLV source restart. The
    /// depacketizer keeps PTS monotonic across video restarts, so this
    /// stays meaningful across them.
    video_pts_max: Option<i64>,
    /// Placed units waiting for the video to reach them, in arrival order.
    /// Shares [`ALIGN_HOLD_MAX_UNITS`] and [`ALIGN_HOLD_MAX_BYTES`] with
    /// `hold`; each waits at most [`PLACED_WAIT_MAX`] from its placement.
    placed: VecDeque<Placed>,
    /// Sum of `placed`'s payload lengths.
    placed_bytes: usize,
    /// Highest video PTS pushed into the muxer, from
    /// [`Self::on_video_muxed`]: placed units at or below it leave.
    video_reach: Option<i64>,
    /// Placed units the aligner discarded (budget or age).
    dropped_placed: u64,
}

impl Aligner {
    pub(crate) fn new() -> Self {
        Self {
            video: TrackClock::new(),
            klv: TrackClock::new(),
            video_origin: None,
            klv_first: None,
            hold: VecDeque::new(),
            held_bytes: 0,
            mode: ClockAlignment::Pending,
            steps: 0,
            offset: None,
            dropped: 0,
            reanchor_next: false,
            video_pts_max: None,
            placed: VecDeque::new(),
            placed_bytes: 0,
            video_reach: None,
            dropped_placed: 0,
        }
    }

    /// Everything that relates the two clocks belongs to the old clock
    /// pair after either track's source restarts: the offset, the
    /// fallback's KLV anchor, and the held units (placed through the old
    /// clocks they would land on the wrong part of the line; counted in
    /// [`Self::dropped`]). Alignment reads [`ClockAlignment::Pending`]
    /// until a fresh report pair or the fallback re-establishes it.
    fn forget_mapping(&mut self) {
        self.offset = None;
        self.mode = ClockAlignment::Pending;
        self.klv_first = None;
        self.discard_held();
    }

    /// The video track's SSRC changed. The video chain and report belong
    /// to the old source and are discarded with the mapping (see
    /// [`Self::forget_mapping`]); so is the video origin, until the new
    /// source's first AU re-anchors it in [`Self::on_video_au`]. Called
    /// on the new source's first RTP packet, so a report the new source
    /// sends before its first AU completes goes through the new chain and
    /// survives the re-anchor.
    pub(crate) fn on_video_source_change(&mut self) {
        tracing::debug!(
            target: "tst_rtp::server::publish",
            held = self.hold.len(),
            "new video source; KLV alignment restarts"
        );
        self.video = TrackClock::new();
        self.forget_mapping();
        self.reanchor_next |= self.video_origin.take().is_some();
    }

    /// The KLV track's SSRC changed. The KLV chain and report belong to
    /// the old source and are discarded with the mapping (see
    /// [`Self::forget_mapping`]). The video line is unchanged, so the
    /// fallback re-anchors the new source's first unit where the video
    /// line is now (the highest video PTS seen), rather than at its start.
    pub(crate) fn on_klv_source_change(&mut self) {
        tracing::debug!(
            target: "tst_rtp::server::publish",
            held = self.hold.len(),
            "new KLV source; KLV alignment restarts"
        );
        self.klv = TrackClock::new();
        self.forget_mapping();
        if let (Some(o), Some(pts)) = (self.video_origin.as_mut(), self.video_pts_max) {
            o.fallback_anchor = o.unwrapped + pts;
        }
    }

    /// A video AU at `pts` reached the muxer. Placed units at or below
    /// the highest such PTS may leave on the next [`Self::poll`].
    pub(crate) fn on_video_muxed(&mut self, pts: i64) {
        self.video_reach = Some(self.video_reach.map_or(pts, |r| r.max(pts)));
    }

    /// Record the video track's RTCP sender report and recompute the
    /// sender-report offset if the KLV track's report is also known.
    pub(crate) fn on_video_sr(&mut self, sr: &SenderReport) {
        let rtp = self.video.unwrap(sr.rtp_timestamp);
        self.video.sr = Some((sr.ntp_timestamp, rtp));
        self.recompute_sr_offset();
    }

    /// Record the KLV track's RTCP sender report and recompute the
    /// sender-report offset if the video track's report is also known.
    pub(crate) fn on_klv_sr(&mut self, sr: &SenderReport) {
        let rtp = self.klv.unwrap(sr.rtp_timestamp);
        self.klv.sr = Some((sr.ntp_timestamp, rtp));
        self.recompute_sr_offset();
    }

    /// With both sender reports known, recompute
    /// `offset = (rtp_video_sr - rtp_klv_sr) + (ntp_klv_sr - ntp_video_sr) in ticks`,
    /// both RTP values unwrapped. The first time an offset becomes known
    /// this establishes alignment (mode becomes
    /// [`ClockAlignment::SenderReport`]) without counting a step; a later
    /// report pair replaces an already-known offset and ticks
    /// [`Self::steps`] when it moved by more than [`STEP_TOLERANCE_TICKS`].
    fn recompute_sr_offset(&mut self) {
        let (Some((ntp_v, rtp_v)), Some((ntp_k, rtp_k))) = (self.video.sr, self.klv.sr) else {
            return;
        };
        // NTP values are 32.32 fixed-point seconds; the delta in 90 kHz
        // ticks is computed in i128 to avoid overflow, then narrowed.
        let ntp_delta_ticks = (((ntp_k as i128) - (ntp_v as i128)) * 90_000) >> 32;
        let candidate = (rtp_v - rtp_k) + ntp_delta_ticks as i64;
        if self
            .offset
            .is_some_and(|old| (candidate - old).abs() > STEP_TOLERANCE_TICKS)
        {
            self.steps += 1;
        }
        self.offset = Some(candidate);
        self.mode = ClockAlignment::SenderReport;
    }

    /// Record the video track's PTS zero point from an emitted AU's RTP
    /// timestamp and PTS. The first call sets the origin; later calls with
    /// the same zero (`rtp − pts`, mod 2^32) change nothing. The first
    /// origin after [`Self::on_video_source_change`] is a re-anchor. A
    /// CHANGED zero without that call also means the depacketizer
    /// re-anchored on a new video source: the new zero is adopted, the
    /// video chain restarts from it, and everything tied to the old video
    /// clock is discarded — the video sender report and the mapping (see
    /// [`Self::forget_mapping`]).
    pub(crate) fn on_video_au(&mut self, rtp_timestamp: u32, pts: i64) {
        self.video_pts_max = Some(self.video_pts_max.map_or(pts, |m| m.max(pts)));
        let raw_zero = rtp_timestamp.wrapping_sub(pts as u32);
        let restarted = if let Some(o) = self.video_origin {
            let moved = (raw_zero.wrapping_sub(o.raw_zero) as i32).unsigned_abs();
            if moved <= ORIGIN_TOLERANCE_TICKS {
                return;
            }
            tracing::debug!(
                target: "tst_rtp::server::publish",
                held = self.hold.len(),
                "video PTS zero moved (new video source); KLV alignment restarts"
            );
            self.video = TrackClock::new();
            self.forget_mapping();
            true
        } else {
            std::mem::take(&mut self.reanchor_next)
        };
        let rtp = self.video.unwrap(rtp_timestamp);
        let unwrapped = rtp - pts;
        self.video_origin = Some(VideoOrigin {
            raw_zero,
            unwrapped,
            fallback_anchor: if restarted { rtp } else { unwrapped },
        });
    }

    /// Queue a KLV unit; returns every unit now due, in order, as
    /// `(unit, pts_ticks)`. A unit is placed once the video origin is
    /// known and either a sender-report pair or the two-second fallback has
    /// established an offset — at which point every held unit (this one
    /// included) is placed in one pass — and due once the video pushed into
    /// the muxer has reached its PTS (see the [module docs](self)).
    ///
    /// Held and placed units together are bounded drop-oldest at
    /// [`ALIGN_HOLD_MAX_UNITS`] units and [`ALIGN_HOLD_MAX_BYTES`] bytes,
    /// placed units first (they arrived first). While no video origin
    /// exists the fallback cannot engage, so held units older than
    /// [`ALIGN_FALLBACK`] are evicted instead: a publisher that sends only
    /// KLV holds at most two seconds of it (spec §4). Every unit dropped
    /// here counts in [`Self::dropped`].
    pub(crate) fn on_klv_unit(&mut self, u: KlvUnit, now: Instant) -> Vec<(KlvUnit, i64)> {
        let t_k = self.klv.unwrap(u.rtp_timestamp);
        self.klv_first.get_or_insert(t_k);
        self.held_bytes += u.bytes.len();
        self.hold.push_back(Held {
            unit: u,
            t_k,
            at: now,
        });
        while self.hold.len() + self.placed.len() > ALIGN_HOLD_MAX_UNITS
            || self.held_bytes + self.placed_bytes > ALIGN_HOLD_MAX_BYTES
        {
            self.drop_oldest();
        }
        self.release(now)
    }

    /// Drop the oldest unit the aligner holds, counting it: the front of
    /// `placed` if any (it arrived before everything in `hold`), else the
    /// front of `hold`.
    fn drop_oldest(&mut self) {
        if let Some(p) = self.placed.pop_front() {
            self.placed_bytes -= p.unit.bytes.len();
            self.dropped_placed += 1;
        } else if let Some(h) = self.hold.pop_front() {
            self.held_bytes -= h.unit.bytes.len();
            self.dropped += 1;
        }
    }

    /// Drop every held unit, counting each.
    fn discard_held(&mut self) {
        self.dropped += self.hold.len() as u64;
        self.hold.clear();
        self.held_bytes = 0;
    }

    /// With no video origin there is no line to place on and no fallback
    /// to wait for: evict held (unplaced) units older than
    /// [`ALIGN_FALLBACK`]. Placed units have their own age bound, in
    /// [`Self::take_due`].
    fn evict_stale(&mut self, now: Instant) {
        if self.video_origin.is_some() {
            return;
        }
        while self
            .hold
            .front()
            .is_some_and(|h| now.saturating_duration_since(h.at) >= ALIGN_FALLBACK)
        {
            let h = self.hold.pop_front().expect("front exists");
            self.held_bytes -= h.unit.bytes.len();
            self.dropped += 1;
        }
    }

    /// If no offset is known yet and [`ALIGN_FALLBACK`] has elapsed since
    /// the first held unit, establish the first-packet-coincidence offset
    /// (the first KLV unit lands at the video line's PTS 0, or where the
    /// line was re-anchored — see `VideoOrigin::fallback_anchor`). This is always the first establishment of
    /// an offset, so it never counts a step.
    fn maybe_engage_fallback(&mut self, now: Instant) {
        if self.offset.is_some() {
            return;
        }
        let Some(started) = self.hold.front().map(|h| h.at) else {
            return;
        };
        if now.saturating_duration_since(started) >= ALIGN_FALLBACK {
            self.engage_fallback();
        }
    }

    fn engage_fallback(&mut self) {
        if let (Some(origin), Some(first)) = (self.video_origin, self.klv_first) {
            self.offset = Some(origin.fallback_anchor - first);
            self.mode = ClockAlignment::Provisional;
        }
    }

    /// Place everything currently held, if alignment is known, then
    /// return the placed units now due. Shared by [`Self::on_klv_unit`],
    /// [`Self::poll`] and [`Self::drain`].
    fn release(&mut self, now: Instant) -> Vec<(KlvUnit, i64)> {
        self.evict_stale(now);
        self.maybe_engage_fallback(now);
        self.place_held(now);
        self.take_due(now)
    }

    /// Move every held unit to `placed`, if alignment is known.
    fn place_held(&mut self, now: Instant) {
        let (Some(origin), Some(offset)) = (self.video_origin, self.offset) else {
            return;
        };
        self.placed_bytes += self.held_bytes;
        self.held_bytes = 0;
        self.placed.extend(self.hold.drain(..).map(|h| Placed {
            pts: h.t_k + offset - origin.unwrapped,
            unit: h.unit,
            at: now,
        }));
    }

    /// Pop placed units from the front while the video pushed into the
    /// muxer has reached them. A front unit it has not reached waits
    /// (everything behind it too, keeping arrival order) until it is
    /// [`PLACED_WAIT_MAX`] old, then is dropped and counted.
    fn take_due(&mut self, now: Instant) -> Vec<(KlvUnit, i64)> {
        let mut due = Vec::new();
        while let Some(p) = self.placed.front() {
            let reached = self.video_reach.is_some_and(|r| p.pts <= r);
            if !reached && now.saturating_duration_since(p.at) < PLACED_WAIT_MAX {
                break;
            }
            let p = self.placed.pop_front().expect("front exists");
            self.placed_bytes -= p.unit.bytes.len();
            if reached {
                due.push((p.unit, p.pts));
            } else {
                tracing::debug!(
                    target: "tst_rtp::server::publish",
                    pts = p.pts,
                    video = ?self.video_reach,
                    "placed KLV unit never reached by the video; dropped"
                );
                self.dropped_placed += 1;
            }
        }
        due
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

    /// KLV units the aligner discarded, held or placed, cumulative.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped + self.dropped_placed
    }

    /// Flush: place whatever is held, using the current mapping if one is
    /// already known, otherwise forcing the first-packet-coincidence
    /// fallback now (ignoring the [`ALIGN_FALLBACK`] window) so an
    /// end-of-stream flush doesn't lose units that never got a sender
    /// report, and release every placed unit whether or not the video has
    /// reached it. While the video origin is unknown there is no line to
    /// place anything on: every held unit is abandoned and counted in
    /// [`Self::dropped`] (units already placed are still released).
    pub(crate) fn drain(&mut self, now: Instant) -> Vec<(KlvUnit, i64)> {
        if self.video_origin.is_none() {
            self.discard_held();
        } else if self.offset.is_none() {
            self.engage_fallback();
        }
        self.place_held(now);
        self.placed_bytes = 0;
        self.placed.drain(..).map(|p| (p.unit, p.pts)).collect()
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
        /// Test-only: an aligner whose video has already been pushed past
        /// any PTS, so placed units come back at once. For tests of the
        /// clock mapping; the hold behind the video has its own tests.
        fn caught_up() -> Self {
            let mut a = Aligner::new();
            a.on_video_muxed(i64::MAX);
            a
        }

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
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(90_000, 0); // video origin = 90 000 (PTS 0)
        assert!(a.on_klv_unit(unit(500_000), t0).is_empty(), "held");
        assert_eq!(a.mode(), ClockAlignment::Pending);
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
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(1_000, 0);
        assert!(a.on_klv_unit(unit(7_000), t0).is_empty());
        let placed = a.on_klv_unit(unit(7_900), t0 + ALIGN_FALLBACK + Duration::from_millis(1));
        // first-packet coincidence: first klv unit ↔ pts 0; second is 900 ticks later
        assert_eq!(placed, vec![(unit(7_000), 0), (unit(7_900), 900)]);
        assert_eq!(a.mode(), ClockAlignment::Provisional);
    }

    #[test]
    fn poll_releases_held_units_after_the_fallback() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(1_000, 0);
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
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
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
    fn report_jitter_within_a_millisecond_is_not_a_step() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
        a.on_video_sr(&sr(10, 0, 0));
        a.on_klv_sr(&sr(10, 0, 0));
        // One tick of sampling jitter on the next KLV report: no step.
        a.on_klv_sr(&sr(11, 0, 90_001));
        assert_eq!(a.steps(), 0);
        // A 100-tick move (> 90 ticks, 1 ms) is a step.
        a.on_klv_sr(&sr(12, 0, 180_100));
        assert_eq!(a.steps(), 1);
        assert_eq!(
            a.on_klv_unit(unit(180_100), t0),
            vec![(unit(180_100), 180_000)]
        );
    }

    #[test]
    fn reports_after_the_fallback_switch_to_the_report_mapping_with_one_step() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(1_000, 0);
        assert!(a.on_klv_unit(unit(7_000), t0).is_empty());
        let late = t0 + ALIGN_FALLBACK;
        assert_eq!(
            a.on_klv_unit(unit(7_900), late),
            vec![(unit(7_000), 0), (unit(7_900), 900)]
        );
        assert_eq!((a.mode(), a.steps()), (ClockAlignment::Provisional, 0));
        // Reports arrive late: ntp 10.0 ↔ video 1 000 and KLV 2 500. KLV
        // 8 800 is 6 300 ticks after that instant: video 7 300, PTS 6 300
        // (the provisional mapping would have said 1 800).
        a.on_video_sr(&sr(10, 0, 1_000));
        a.on_klv_sr(&sr(10, 0, 2_500));
        assert_eq!((a.mode(), a.steps()), (ClockAlignment::SenderReport, 1));
        assert_eq!(a.on_klv_unit(unit(8_800), late), vec![(unit(8_800), 6_300)]);
    }

    #[test]
    fn hold_queue_is_bounded_drop_oldest() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
        for i in 0..(ALIGN_HOLD_MAX_UNITS as u32 + 5) {
            assert!(a.on_klv_unit(unit(i), t0).is_empty());
        }
        a.on_video_sr(&sr(1, 0, 0));
        let placed = a.on_klv_sr_then_release(&sr(1, 0, 0), t0);
        assert_eq!(placed.len(), ALIGN_HOLD_MAX_UNITS);
        assert_eq!(placed[0].0.rtp_timestamp, 5, "oldest five dropped");
        assert_eq!(a.dropped(), 5, "and counted");
    }

    fn big_unit(ts: u32, len: usize) -> KlvUnit {
        KlvUnit {
            bytes: vec![0xAB; len],
            rtp_timestamp: ts,
        }
    }

    #[test]
    fn hold_queue_is_bounded_in_bytes_drop_oldest() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
        // 100 units of 60 000 bytes = 6 MB offered, all at one instant.
        for i in 0..100u32 {
            assert!(a.on_klv_unit(big_unit(i, 60_000), t0).is_empty());
            assert!(a.held_bytes <= ALIGN_HOLD_MAX_BYTES);
        }
        let kept = ALIGN_HOLD_MAX_BYTES / 60_000; // 69
        assert_eq!(a.hold.len(), kept);
        assert_eq!(a.held_bytes, kept * 60_000);
        assert_eq!(a.dropped(), (100 - kept) as u64);
        assert_eq!(
            a.hold.front().unwrap().unit.rtp_timestamp,
            (100 - kept) as u32
        );
    }

    #[test]
    fn klv_only_publisher_holds_at_most_two_seconds_and_counts_evictions() {
        // No video origin ever: the fallback cannot engage, so age bounds
        // the hold. One 50 000-byte unit every 100 ms for 10 s.
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        for i in 0..100u32 {
            let now = t0 + Duration::from_millis(100 * u64::from(i));
            assert!(a.on_klv_unit(big_unit(i * 9_000, 50_000), now).is_empty());
            assert!(a.held_bytes <= ALIGN_HOLD_MAX_BYTES);
            let oldest = a.hold.front().unwrap().at;
            assert!(now.saturating_duration_since(oldest) < ALIGN_FALLBACK);
        }
        // 20 units fit in the window (ages 0..1.9 s); 80 were evicted.
        assert_eq!(a.hold.len(), 20);
        assert_eq!(a.held_bytes, 20 * 50_000);
        assert_eq!(a.dropped(), 80);
        assert_eq!(a.mode(), ClockAlignment::Pending);
    }

    #[test]
    fn drain_without_a_video_origin_abandons_and_counts_held_units() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        assert!(a.on_klv_unit(unit(1), t0).is_empty());
        assert!(a.on_klv_unit(unit(2), t0).is_empty());
        assert!(a.drain(t0).is_empty());
        assert_eq!(a.dropped(), 2);
        assert_eq!(a.held_bytes, 0);
        assert!(a.hold.is_empty());
    }

    /// Ideal publisher: both tracks' 90 kHz clocks locked to NTP, an SR
    /// pair every 5 s, one KLV unit a second for 200 s, the video PTS zero
    /// at t = 0. Every unit must land at exactly `t × 90 000`, and no SR
    /// may count as a step: the mapping never really changes.
    fn assert_exact_line_across_a_wrap(v0: u32, k0: u32) {
        let v = |t: u32| v0.wrapping_add(t * 90_000);
        let k = |t: u32| k0.wrapping_add(t * 90_000);
        let mut a = Aligner::caught_up();
        let now = Instant::now();
        a.on_video_au(v(0), 0);
        for t in 0..=200u32 {
            if t % 5 == 0 {
                a.on_video_sr(&sr(1000 + u64::from(t), 0, v(t)));
                a.on_klv_sr(&sr(1000 + u64::from(t), 0, k(t)));
            }
            let placed = a.on_klv_unit(unit(k(t)), now);
            assert_eq!(
                placed,
                vec![(unit(k(t)), i64::from(t) * 90_000)],
                "t = {t} s"
            );
        }
        assert_eq!(a.steps(), 0);
        assert_eq!(a.mode(), ClockAlignment::SenderReport);
    }

    #[test]
    fn klv_clock_wrapping_mid_session_keeps_the_pts_line() {
        // The KLV clock wraps about 100 s in, with SR pairs on both sides.
        assert_exact_line_across_a_wrap(1_000_000, u32::MAX - 90_000 * 100);
    }

    #[test]
    fn video_clock_wrapping_mid_session_keeps_the_pts_line() {
        // The video clock wraps about 100 s in, with SR pairs on both sides.
        assert_exact_line_across_a_wrap(u32::MAX - 90_000 * 100, 1_000_000);
    }

    #[test]
    fn a_new_video_source_discards_the_stale_report_until_a_fresh_one() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(10_000, 0);
        a.on_video_sr(&sr(100, 0, 10_000));
        a.on_klv_sr(&sr(100, 0, 50_000));
        assert_eq!(a.on_klv_unit(unit(59_000), t0), vec![(unit(59_000), 9_000)]);
        // The encoder restarts on a new SSRC with a new random RTP origin.
        // The depacketizer keeps its PTS line monotonic, so the zero
        // (`rtp − pts`) moves: 7 000 000 − 12 003.
        a.on_video_au(7_000_000, 12_003);
        assert_eq!(a.mode(), ClockAlignment::Pending);
        // A later AU of the same source changes nothing.
        a.on_video_au(7_003_003, 15_006);
        // The old video report no longer describes the video clock: held.
        assert!(a.on_klv_unit(unit(140_900), t0).is_empty());
        // A fresh video report: ntp 101.0 s ↔ the new clock's 7 000 000.
        // The KLV clock reads 50 000 + 90 000 = 140 000 at ntp 101.0, so
        // the unit at 140 900 is 900 ticks later: video rtp 7 000 900,
        // PTS 7 000 900 − (7 000 000 − 12 003) = 12 903.
        a.on_video_sr(&sr(101, 0, 7_000_000));
        assert_eq!(a.poll(t0), vec![(unit(140_900), 12_903)]);
        assert_eq!(a.mode(), ClockAlignment::SenderReport);
        assert_eq!(a.steps(), 0, "a re-established mapping is not a step");
        assert_eq!(a.dropped(), 0);
    }

    #[test]
    fn a_new_video_source_drops_held_units_and_restarts_the_fallback() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(10_000, 0);
        assert!(a.on_klv_unit(unit(1), t0).is_empty());
        a.on_video_au(9_000_000, 3_003);
        assert_eq!(a.dropped(), 1, "the held unit is discarded and counted");
        // The fallback window restarts from the next held unit.
        let t1 = t0 + Duration::from_secs(10);
        assert!(a.on_klv_unit(unit(5_000), t1).is_empty());
        let placed = a.poll(t1 + ALIGN_FALLBACK);
        // Coincidence with the re-anchoring AU, not the line's start.
        assert_eq!(placed, vec![(unit(5_000), 3_003)]);
        assert_eq!(a.mode(), ClockAlignment::Provisional);
    }

    #[test]
    fn a_new_klv_source_under_provisional_lands_on_the_video_line() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        // The video line has reached PTS 369 000.
        a.on_video_au(1_000, 0);
        a.on_video_au(1_000 + 369_000, 369_000);
        // KLV at 10 Hz from raw 50 000 for 4 s, no reports: Provisional.
        for i in 0..40u32 {
            let now = t0 + Duration::from_millis(100 * u64::from(i));
            a.on_klv_unit(unit(50_000 + 9_000 * i), now);
        }
        assert_eq!(a.mode(), ClockAlignment::Provisional);
        // The KLV payloader restarts on a new SSRC at raw 1 000 000 000.
        a.on_klv_source_change();
        assert_eq!(a.mode(), ClockAlignment::Pending);
        let t1 = t0 + Duration::from_secs(4);
        assert!(a.on_klv_unit(unit(1_000_000_000), t1).is_empty(), "held");
        assert!(
            a.on_klv_unit(unit(1_000_009_000), t1 + Duration::from_millis(100))
                .is_empty()
        );
        // Fallback from the new source's first unit, anchored where the
        // video line is now — not at the old chain's continuation.
        assert_eq!(
            a.poll(t1 + ALIGN_FALLBACK),
            vec![
                (unit(1_000_000_000), 369_000),
                (unit(1_000_009_000), 378_000)
            ]
        );
        assert_eq!(
            (a.mode(), a.steps(), a.dropped()),
            (ClockAlignment::Provisional, 0, 0)
        );
    }

    #[test]
    fn a_new_klv_source_drops_the_held_units_and_counts_them() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
        assert!(a.on_klv_unit(unit(1), t0).is_empty());
        assert!(a.on_klv_unit(unit(2), t0).is_empty());
        a.on_klv_source_change();
        assert_eq!(a.dropped(), 2);
        assert!(a.hold.is_empty());
        assert_eq!(a.held_bytes, 0);
    }

    #[test]
    fn a_new_klv_source_waits_for_its_own_report() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(0, 0);
        a.on_video_sr(&sr(10, 0, 0));
        a.on_klv_sr(&sr(10, 0, 0));
        assert_eq!(a.on_klv_unit(unit(900), t0), vec![(unit(900), 900)]);
        a.on_klv_source_change();
        // The old KLV report describes the old clock: held.
        assert!(a.on_klv_unit(unit(5_000_000), t0).is_empty());
        assert_eq!(a.mode(), ClockAlignment::Pending);
        // The new source's report: ntp 11.0 s ↔ KLV 4 999 100. The unit
        // is 900 ticks later; the video clock reads 90 000 at ntp 11.0.
        assert_eq!(
            a.on_klv_sr_then_release(&sr(11, 0, 4_999_100), t0),
            vec![(unit(5_000_000), 90_900)]
        );
        assert_eq!(a.mode(), ClockAlignment::SenderReport);
        assert_eq!(a.steps(), 0, "a re-established mapping is not a step");
    }

    #[test]
    fn a_new_video_sources_report_before_its_first_au_is_kept() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(10_000, 0);
        a.on_video_sr(&sr(100, 0, 10_000));
        a.on_klv_sr(&sr(100, 0, 50_000));
        assert_eq!(a.on_klv_unit(unit(59_000), t0), vec![(unit(59_000), 9_000)]);
        // The new video source's first packet, then its report, then its
        // first whole AU (PTS kept monotonic by the depacketizer).
        a.on_video_source_change();
        assert_eq!(a.mode(), ClockAlignment::Pending);
        a.on_video_sr(&sr(101, 0, 7_000_000));
        a.on_video_au(7_000_000, 12_003);
        assert_eq!(a.mode(), ClockAlignment::SenderReport);
        // KLV 140 900 is 900 ticks after ntp 101.0: video 7 000 900, PTS
        // 7 000 900 − (7 000 000 − 12 003) = 12 903.
        assert_eq!(
            a.on_klv_unit(unit(140_900), t0),
            vec![(unit(140_900), 12_903)]
        );
        assert_eq!(a.steps(), 0);
    }

    #[test]
    fn a_new_video_source_drops_held_units_at_the_change_and_reanchors_the_fallback() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(10_000, 0);
        assert!(a.on_klv_unit(unit(1), t0).is_empty());
        a.on_video_source_change();
        assert_eq!(a.dropped(), 1, "the held unit is discarded and counted");
        let t1 = t0 + Duration::from_secs(10);
        assert!(a.on_klv_unit(unit(5_000), t1).is_empty());
        a.on_video_au(9_000_000, 3_003);
        // Coincidence with the re-anchoring AU, not the line's start.
        assert_eq!(a.poll(t1 + ALIGN_FALLBACK), vec![(unit(5_000), 3_003)]);
        assert_eq!(a.mode(), ClockAlignment::Provisional);
    }

    /// An aligner with the video origin at RTP 0 and a report pair that
    /// makes KLV RTP == video RTP: a unit's PTS is its RTP timestamp.
    /// Nothing has been pushed to the muxer yet.
    fn aligned_at_zero() -> Aligner {
        let mut a = Aligner::new();
        a.on_video_au(0, 0);
        a.on_video_sr(&sr(10, 0, 0));
        a.on_klv_sr(&sr(10, 0, 0));
        a
    }

    #[test]
    fn a_placed_unit_waits_for_the_pushed_video_to_reach_its_pts() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        // Stamped 500 ms ahead of the video pushed so far.
        assert!(a.on_klv_unit(unit(45_000), t0).is_empty(), "waits");
        assert_eq!(
            a.mode(),
            ClockAlignment::SenderReport,
            "placed, not pending"
        );
        a.on_video_muxed(44_999);
        assert!(a.poll(t0).is_empty());
        a.on_video_muxed(45_000);
        assert_eq!(a.poll(t0), vec![(unit(45_000), 45_000)]);
        assert_eq!(a.dropped(), 0);
    }

    #[test]
    fn a_placed_unit_waits_for_the_first_pushed_video() {
        // The origin is known, but no AU has reached the muxer yet.
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        assert!(a.on_klv_unit(unit(0), t0).is_empty());
        a.on_video_muxed(0);
        assert_eq!(a.poll(t0), vec![(unit(0), 0)]);
    }

    #[test]
    fn placed_units_leave_in_arrival_order() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(50_000);
        // The first unit is ahead; the second, behind, waits behind it.
        assert!(a.on_klv_unit(unit(100_000), t0).is_empty());
        assert!(a.on_klv_unit(unit(10_000), t0).is_empty());
        a.on_video_muxed(100_000);
        assert_eq!(
            a.poll(t0),
            vec![(unit(100_000), 100_000), (unit(10_000), 10_000)]
        );
    }

    #[test]
    fn a_placed_unit_the_video_never_reaches_is_dropped_after_the_age_bound() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        assert!(a.on_klv_unit(unit(45_000), t0).is_empty());
        // Waiting past the 2 s alignment fallback is not an age-out: a KLV
        // clock seconds ahead of the video keeps its metadata.
        assert!(a.poll(t0 + ALIGN_FALLBACK).is_empty());
        assert!(
            a.poll(t0 + PLACED_WAIT_MAX - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(a.dropped(), 0, "slow video alone does not drop it");
        assert!(a.poll(t0 + PLACED_WAIT_MAX).is_empty());
        assert_eq!(a.dropped(), 1, "aged out and counted");
        a.on_video_muxed(90_000);
        assert!(a.poll(t0 + PLACED_WAIT_MAX).is_empty(), "gone");
    }

    #[test]
    fn placed_and_unplaced_units_share_the_hold_budgets() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        let placed = ALIGN_HOLD_MAX_UNITS as u32 - 6;
        for i in 0..placed {
            assert!(a.on_klv_unit(unit(1_000 + i), t0).is_empty());
        }
        assert_eq!(a.dropped(), 0);
        // A new KLV source: its units are unplaced until its own report.
        a.on_klv_source_change();
        for i in 0..10u32 {
            assert!(a.on_klv_unit(unit(9_000_000 + i), t0).is_empty());
        }
        assert_eq!(a.hold.len() + a.placed.len(), ALIGN_HOLD_MAX_UNITS);
        assert_eq!(a.dropped(), 4, "the oldest placed units");
        assert_eq!(a.hold.len(), 10);
        assert_eq!(a.placed.front().unwrap().unit.rtp_timestamp, 1_004);
    }

    #[test]
    fn placed_units_count_toward_the_byte_budget() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        for i in 0..100u32 {
            assert!(a.on_klv_unit(big_unit(1_000 + i, 60_000), t0).is_empty());
            assert!(a.held_bytes + a.placed_bytes <= ALIGN_HOLD_MAX_BYTES);
        }
        let kept = ALIGN_HOLD_MAX_BYTES / 60_000;
        assert_eq!(a.placed.len(), kept);
        assert_eq!(a.dropped(), (100 - kept) as u64);
    }

    #[test]
    fn drain_releases_placed_units_the_video_has_not_reached() {
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        assert!(a.on_klv_unit(unit(45_000), t0).is_empty());
        assert_eq!(a.drain(t0), vec![(unit(45_000), 45_000)]);
        assert_eq!(a.placed_bytes, 0);
    }

    #[test]
    fn placed_units_outlive_a_video_source_change() {
        // Placed units sit on the PTS line, which the depacketizer keeps
        // continuous across a video restart: they still wait for it.
        let mut a = aligned_at_zero();
        let t0 = Instant::now();
        a.on_video_muxed(0);
        assert!(a.on_klv_unit(unit(45_000), t0).is_empty());
        a.on_video_source_change();
        assert_eq!(a.dropped(), 0);
        a.on_video_au(7_000_000, 3_003);
        a.on_video_muxed(45_000);
        assert_eq!(a.poll(t0), vec![(unit(45_000), 45_000)]);
    }

    #[test]
    fn rtp_timestamp_wrap_is_unwrapped() {
        let mut a = Aligner::caught_up();
        let t0 = Instant::now();
        a.on_video_au(u32::MAX - 1000, 0);
        a.on_video_sr(&sr(1, 0, u32::MAX - 1000));
        a.on_klv_sr(&sr(1, 0, u32::MAX - 1000));
        assert_eq!(
            a.on_klv_unit(unit(u32::MAX - 1000), t0),
            vec![(unit(u32::MAX - 1000), 0)]
        );
        assert_eq!(a.on_klv_unit(unit(2000), t0), vec![(unit(2000), 3001)]);
    }
}
