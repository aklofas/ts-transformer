//! The demuxer's two timeline memories: the opt-in PTS/DTS unwrapper and the
//! per-PID PCR history.
//!
//! [`TimestampUnwrapper`] owns the per-PROGRAM clock and the re-anchor
//! counter; the per-PID accumulator stays in the PID's `PidState` entry (so a
//! PID leaving the topology still goes with one `remove`) and is lent to
//! [`TimestampUnwrapper::unwrap`] by the caller. [`PcrHistory`] owns the last
//! 27 MHz PCR of every declared PCR PID.
//!
//! This is the 33-bit PTS/PCR unwrap of an MPEG-TS elementary stream; it is
//! unrelated to the RTP crate's 32-bit sequence/timestamp unwrapper.

use crate::mpegts::common::{Pts90khz, pts_diff_33bit};
use hashbrown::HashMap;

/// Per-PID PTS/DTS unwrap accumulator state, in raw 90 kHz ticks.
///
/// Holds the previous sample's raw wire value alongside the unwrapped
/// value that was emitted for it. Each new sample accumulates its signed
/// wrap-aware delta (via [`pts_diff_33bit`]) onto `last_unwrapped`, so the
/// epoch a sample lands in is decided by how far it actually is from its
/// predecessor — never by a latched offset that a later out-of-order arrival
/// would inherit.
///
/// The "offset" a secondary timestamp needs is derived on demand as
/// `last_unwrapped - last_raw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UnwrapState {
    /// The previous sample's raw 33-bit wire value.
    pub(super) last_raw: i64,
    /// The unwrapped value emitted for that same sample.
    pub(super) last_unwrapped: i64,
}

/// Per-PROGRAM unwrap reference, keyed by `program_number`: the most recent
/// [`UnwrapState`] emitted on ANY PID of that program, plus the count of
/// dormant-PID re-anchors.
///
/// A PID's FIRST sample anchors against its program's reference rather than
/// at its own raw value, so a PID that starts flowing after the program's
/// 33-bit clock has already wrapped lands in the epoch its siblings are
/// already in. Keyed per program because each program carries its own time
/// base (ITU-T H.222.0 §2.4.3.5) — programs are never cross-anchored. A PID
/// with no owning program anchors at its raw value and contributes nothing
/// here.
#[derive(Debug, Default)]
pub(super) struct TimestampUnwrapper {
    program_clock: HashMap<u16, UnwrapState>,
    reanchors: u64,
}

impl TimestampUnwrapper {
    /// Dormant-PID re-anchors since the last [`Self::reset_reanchors`].
    pub(super) fn reanchors(&self) -> u64 {
        self.reanchors
    }

    pub(super) fn reset_reanchors(&mut self) {
        self.reanchors = 0;
    }

    /// Forget every program reference (a reconnect restarts the timeline).
    pub(super) fn clear(&mut self) {
        self.program_clock.clear();
    }

    /// Drop a program's reference: it left the PAT, and a later program
    /// re-using the number owns a different time base.
    pub(super) fn retire_program(&mut self, program: u16) {
        self.program_clock.remove(&program);
    }

    #[cfg(test)]
    pub(super) fn has_program(&self, program: u16) -> bool {
        self.program_clock.contains_key(&program)
    }

    /// The unwrapped value for a PID's very FIRST observed sample: at its
    /// program's reference's wrap-aware distance when a sibling already set
    /// one, otherwise the raw value.
    fn anchor_first_sample(&self, program: Option<u16>, raw_ticks: i64) -> i64 {
        match program.and_then(|p| self.program_clock.get(&p)) {
            Some(reference) => reference
                .last_unwrapped
                .saturating_add(pts_diff_33bit(raw_ticks as u64, reference.last_raw as u64)),
            None => raw_ticks,
        }
    }

    /// Dormant-PID re-anchor. A PID's own signed 33-bit delta is unambiguous
    /// only while its silence stays under half an epoch (`2^32` ticks, ~13.3 h
    /// at 90 kHz); past that the signed modular difference lands a full epoch
    /// low. The program clock — the last value emitted on ANY sibling PID — is
    /// the freshest evidence available. Returns `Some(value)` when BOTH hold:
    ///
    /// 1. the program clock is fresher than this PID's last sample, and
    /// 2. placing the sample against that reference puts it more than half an
    ///    epoch from the PID's own last sample — a distance the PID's own
    ///    delta can never express.
    ///
    /// A genuinely reordered late sample fails (2), so the PID's own signed
    /// delta is kept and the reorder stays visible as a backward step. Never
    /// fires for a PID with no owning program or a program with no clock yet,
    /// and never crosses programs.
    fn dormant_reanchor(
        &self,
        program: Option<u16>,
        raw_ticks: i64,
        state: UnwrapState,
    ) -> Option<i64> {
        const HALF_EPOCH: i64 = 1 << 32;
        let reference = self.program_clock.get(&program?)?;
        if reference.last_unwrapped <= state.last_unwrapped {
            return None;
        }
        let via_program = reference
            .last_unwrapped
            .saturating_add(pts_diff_33bit(raw_ticks as u64, reference.last_raw as u64));
        let implied_gap = via_program.saturating_sub(state.last_unwrapped);
        (implied_gap.abs() > HALF_EPOCH).then_some(via_program)
    }

    /// Unwrap a raw 33-bit PTS (or a KLV Metadata PTS — same clock, same
    /// accumulator) onto a continuous `i64` timeline, advancing the PID's
    /// accumulator `own` and its program's reference.
    ///
    /// The first value for a PID is placed by the program's reference when a
    /// sibling has one, otherwise at the raw value. Each later value adds its
    /// signed wrap-aware delta from the previous RAW value onto the previous
    /// UNWRAPPED value. The timeline is never rebased to zero, so video and
    /// KLV stay comparable.
    ///
    /// Accumulating a *signed* delta is what makes reordering safe: a small
    /// backward step simply steps back, and a pre-wrap PTS delivered AFTER the
    /// wrap lands back below the boundary instead of a full `1 << 33` too high.
    ///
    /// A PID silent for more than half an epoch while a sibling advanced is
    /// re-anchored onto the program clock and counted in
    /// `DemuxerStats::unwrap_reanchors`.
    pub(super) fn unwrap(
        &mut self,
        own: &mut Option<UnwrapState>,
        program: Option<u16>,
        raw: Pts90khz,
    ) -> Pts90khz {
        let raw_ticks = raw.as_ticks();
        let unwrapped = match *own {
            Some(state) => match self.dormant_reanchor(program, raw_ticks, state) {
                Some(via_program) => {
                    self.reanchors += 1;
                    via_program
                }
                None => state
                    .last_unwrapped
                    .saturating_add(pts_diff_33bit(raw_ticks as u64, state.last_raw as u64)),
            },
            None => self.anchor_first_sample(program, raw_ticks),
        };
        let entry = UnwrapState {
            last_raw: raw_ticks,
            last_unwrapped: unwrapped,
        };
        *own = Some(entry);
        if let Some(prog) = program {
            self.program_clock.insert(prog, entry);
        }
        Pts90khz::new(unwrapped)
    }

    /// Unwrap a DTS that has a PTS on the same PES, relative to that PES's
    /// OWN raw PTS — NOT the bare per-PID offset.
    ///
    /// The 33-bit boundary can fall BETWEEN one AU's DTS and PTS (DTS ≤ PTS
    /// always, so DTS can still be pre-wrap while PTS has wrapped). Applying
    /// the PID's offset would then put the DTS a full `1 << 33` too high; this
    /// measures the DTS's wrap-aware distance from the raw PTS and adds it to
    /// the *unwrapped* PTS. Pure.
    pub(super) fn dts_with_pts(
        unwrapped_pts: Pts90khz,
        pts_raw: Pts90khz,
        dts_raw: Pts90khz,
    ) -> Pts90khz {
        let delta = pts_diff_33bit(dts_raw.as_ticks() as u64, pts_raw.as_ticks() as u64);
        Pts90khz::new(unwrapped_pts.as_ticks() + delta)
    }

    /// Unwrap a DTS that arrived with NO PTS on the same PES (not spec-legal,
    /// §2.4.3.6, but tolerated): the PID's current offset
    /// `last_unwrapped - last_raw`, no wrap detection, no mutation. Offset 0
    /// when the PID has no accumulator yet.
    pub(super) fn secondary(own: Option<UnwrapState>, raw: Pts90khz) -> Pts90khz {
        let offset = own.map_or(0, |s| s.last_unwrapped.saturating_sub(s.last_raw));
        Pts90khz::new(offset.saturating_add(raw.as_ticks()))
    }
}

/// Last-seen 27 MHz PCR per declared PCR PID. Keyed by the PID of the packet
/// carrying the PCR: each program advertises its own PCR PID in its PMT and
/// has its own time base (ITU-T H.222.0 §2.4.3.5), so PCR comparison stays
/// within one PID's timeline.
#[derive(Debug, Default)]
pub(super) struct PcrHistory {
    last: HashMap<u16, u64>,
}

impl PcrHistory {
    pub(super) fn last(&self, pid: u16) -> Option<u64> {
        self.last.get(&pid).copied()
    }

    pub(super) fn record(&mut self, pid: u16, pcr_27mhz: u64) {
        self.last.insert(pid, pcr_27mhz);
    }

    pub(super) fn clear(&mut self) {
        self.last.clear();
    }

    /// Keep only the PIDs for which `declared` holds.
    ///
    /// The history belongs to the declaration, not to the PID: it is kept
    /// while at least one program names the PID as its clock (two programs may
    /// share one PCR PID, and one of them moving away must not erase the
    /// other's baseline), and dropped once none does, whatever else the PID
    /// still carries. A PID declared again later then starts from a seed
    /// instead of being judged against an earlier declaration's baseline.
    pub(super) fn retain_declared(&mut self, mut declared: impl FnMut(u16) -> bool) {
        self.last.retain(|pid, _| declared(*pid));
    }

    #[cfg(test)]
    pub(super) fn contains(&self, pid: u16) -> bool {
        self.last.contains_key(&pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DemuxError;
    use crate::mpegts::demux::StrictMode;
    use crate::mpegts::demux::demuxer::Demuxer;
    use crate::mpegts::demux::event::{DemuxEvent, NonConformantIssue};
    use crate::mpegts::demux::test_support::{
        pat_packet_with_programs, pat_packet_with_programs_cc, pcr_packet_for_test,
        pmt_packet_for_test, pmt_packet_for_test_cc,
    };
    use crate::mpegts::demux::types::DemuxerConfig;
    use crate::test_prelude::*;

    /// A PCR jump on a DEDICATED PCR PID — one
    /// the PMT names as `PCR_PID` but that carries no elementary stream
    /// (broadcast / hardware muxes, TSDuck output) — must still surface
    /// `PcrAnomaly`. The anomaly arm only queued the issue when
    /// `lookup_stream` knew the PID, so such streams never reported a jump
    /// and `StrictMode::TimingOnly` could not reject the class.
    #[test]
    fn pcr_anomaly_surfaces_on_dedicated_pcr_pid() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x0200,
                &[(0x1B, 0x0100)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        demuxer.feed(&pcr_packet_for_test(0x0200, 0)).unwrap();
        // 2 s @ 27 MHz — twice PCR_ANOMALY_THRESHOLD.
        demuxer
            .feed(&pcr_packet_for_test(0x0200, 54_000_000))
            .unwrap();

        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        let anomaly = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant {
                stream,
                issue: NonConformantIssue::PcrAnomaly { delta },
            } => Some((*stream, *delta)),
            _ => None,
        });
        let (stream, delta) = anomaly.unwrap_or_else(|| {
            panic!("expected PcrAnomaly on the dedicated PCR PID, got {events:?}")
        });
        assert_eq!(stream.pid, 0x0200);
        assert_eq!(
            stream.program_number, 1,
            "program resolved from the PMT that declared the PCR PID"
        );
        assert_eq!(delta, 54_000_000);
    }

    /// A PCR on a PID no PMT names as `PCR_PID` is not a time base this
    /// demuxer tracks (H.222.0 §2.4.4.9: the program names the one PID
    /// that carries its clock). Measured on a 72-h soak: the
    /// harness's PID-rewrite corruption moved 61 PCR-carrying packets to
    /// 0x1FFE and the demuxer answered with ~50 `PcrAnomaly` events on a
    /// PID nothing declared. Neither the seed nor the anomaly may happen.
    #[test]
    fn pcr_on_an_undeclared_pid_is_ignored() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        // Two strays on 0x1FFE, 2 s apart at 27 MHz — twice the anomaly
        // threshold, the shape that fired on the soak.
        demuxer.feed(&pcr_packet_for_test(0x1FFE, 0)).unwrap();
        demuxer
            .feed(&pcr_packet_for_test(0x1FFE, 54_000_000))
            .unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )),
            "a PCR on an undeclared PID must not report an anomaly: {events:?}"
        );
        assert!(
            !demuxer.pcr.contains(0x1FFE),
            "a stray PCR must not seed a timeline"
        );

        // The declared PCR PID is still tracked exactly as before.
        demuxer.feed(&pcr_packet_for_test(0x1011, 0)).unwrap();
        demuxer
            .feed(&pcr_packet_for_test(0x1011, 54_000_000))
            .unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { delta: 54_000_000 },
                    ..
                }
            )),
            "the declared PCR PID keeps its anomaly detection: {events:?}"
        );
    }

    /// The consequence that matters to an integrator: under
    /// `StrictMode::TimingOnly` a `PcrAnomaly` is a `StrictRejection`, so
    /// before the gate one corrupted PID field on the wire could end a
    /// strict session. A stray PCR must not be able to.
    #[test]
    fn pcr_on_an_undeclared_pid_does_not_reject_under_timing_only() {
        let mut demuxer = Demuxer::with_config(
            DemuxerConfig::builder()
                .strict(StrictMode::TimingOnly)
                .build(),
        );
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        demuxer.feed(&pcr_packet_for_test(0x1FFE, 0)).unwrap();
        assert!(
            demuxer
                .feed(&pcr_packet_for_test(0x1FFE, 54_000_000))
                .is_ok(),
            "a stray PCR jump is not a timing violation of any program"
        );
        // …while a jump on the DECLARED PCR PID still is. Same assertion
        // form as `pcr_malformed_strict_timing_rejects` below.
        demuxer.feed(&pcr_packet_for_test(0x1011, 0)).unwrap();
        assert!(matches!(
            demuxer.feed(&pcr_packet_for_test(0x1011, 54_000_000)),
            Err(DemuxError::StrictRejection(_))
        ));
    }

    /// History starts when the PMT declares the PID: a PCR that arrived
    /// before the PMT is not a baseline the first post-PMT PCR is judged
    /// against.
    #[test]
    fn pcr_before_the_pmt_declares_its_pid_seeds_no_history() {
        let mut demuxer = Demuxer::new();
        demuxer.feed(&pcr_packet_for_test(0x1011, 0)).unwrap();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}
        demuxer
            .feed(&pcr_packet_for_test(0x1011, 54_000_000))
            .unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )),
            "{events:?}"
        );
    }

    /// When a PMT re-declares `PCR_PID`, the new PID starts a fresh
    /// timeline and the old PID stops being one: its next PCR is a stray.
    #[test]
    fn pcr_pid_change_starts_a_fresh_timeline_and_retires_the_old_pid() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        demuxer.feed(&pcr_packet_for_test(0x1011, 0)).unwrap();
        // PMT v1 (CC=1 so it is not swallowed as a duplicate of v0) moves
        // the clock to a dedicated PCR PID.
        demuxer
            .feed(&pmt_packet_for_test_cc(
                0x1000,
                1,
                0x0200,
                &[(0x1B, 0x1011)],
                1,
                1,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        demuxer
            .feed(&pcr_packet_for_test(0x0200, 54_000_000))
            .unwrap(); // first PCR on the new PID: a seed, not a jump
        demuxer
            .feed(&pcr_packet_for_test(0x1011, 108_000_000))
            .unwrap(); // the old PID is no longer declared: ignored
        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )),
            "{events:?}"
        );
        assert!(demuxer.pcr.contains(0x0200));
        assert!(!demuxer.pcr.contains(0x1011));
    }

    #[test]
    fn pat_removed_program_clears_last_pcr_by_pid() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1100,
                2,
                0x1111,
                &[(0x1B, 0x1111)],
                0,
            ))
            .unwrap();
        demuxer
            .feed(&pcr_packet_for_test(0x1011, 90_000_000))
            .unwrap();
        demuxer
            .feed(&pcr_packet_for_test(0x1111, 90_000_000))
            .unwrap();
        assert!(demuxer.pcr.contains(0x1011));
        assert!(demuxer.pcr.contains(0x1111));

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        assert!(demuxer.pcr.contains(0x1011), "program 1's PCR must survive");
        assert!(
            !demuxer.pcr.contains(0x1111),
            "program 2's PCR must be cleared"
        );
    }

    // -------------------------------------------------------------------------
    // PCR field validation
    // -------------------------------------------------------------------------
    //
    // Parser-level validation lives in `ts.rs` unit tests; these tests cover
    // the demuxer's lenient-mode emission of NonConformantIssue::PcrMalformed.

    #[test]
    fn pcr_malformed_reserved_bits_emits_nonconformant() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();

        // Drain PAT/PMT events.
        while demuxer.next_event().is_some() {}

        // PCR packet with one reserved bit cleared.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20; // af-only
        buf[4] = 183;
        buf[5] = 0x10; // PCR_flag
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        // byte 10: bit 7 = base lsb (0); reserved mask 0x7E but flip bit 1 to 0 → 0x7C
        buf[10] = 0x7C;
        buf[11] = 0;
        demuxer.feed(&buf).unwrap();

        let mut found = false;
        while let Some(e) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PcrMalformed {
                        kind: crate::mpegts::demux::PcrMalformedKind::InvalidReservedBits,
                    },
                ..
            } = e
            {
                found = true;
            }
        }
        assert!(
            found,
            "expected PcrMalformed::InvalidReservedBits event from corrupt PCR field"
        );
    }

    #[test]
    fn pcr_malformed_extension_out_of_range_emits_nonconformant() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        // PCR with extension = 300 (max valid = 299).
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        // base lsb 0; reserved 0x7E; ext bit 8 = 1 (300 = 0x12C)
        buf[10] = 0x7E | 0x01;
        buf[11] = 0x2C;
        demuxer.feed(&buf).unwrap();

        let mut found = false;
        while let Some(e) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PcrMalformed {
                        kind: crate::mpegts::demux::PcrMalformedKind::ExtensionOutOfRange,
                    },
                ..
            } = e
            {
                found = true;
            }
        }
        assert!(
            found,
            "expected PcrMalformed::ExtensionOutOfRange event from ext=300 PCR field"
        );
    }

    #[test]
    fn pcr_malformed_does_not_seed_last_pcr_by_pid() {
        // A malformed PCR must NOT populate last_pcr_by_pid — otherwise the
        // next valid PCR could fire a spurious PcrAnomaly using the corrupt
        // value as the comparison baseline.
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        // Feed a malformed PCR.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0x7C; // reserved bits malformed
        buf[11] = 0;
        demuxer.feed(&buf).unwrap();

        assert!(
            !demuxer.pcr.contains(0x1011),
            "malformed PCR must not seed last_pcr_by_pid"
        );
    }

    /// `PcrMalformed` describes one packet's bytes, not a timeline — unlike
    /// `PcrAnomaly` it must keep firing on ANY PID, declared or not, so a
    /// receiver sees wire-level corruption even on a PID no PMT ever named
    /// as `PCR_PID`. The gate added for `pcr_on_an_undeclared_pid_is_ignored`
    /// must not have swallowed this.
    #[test]
    fn pcr_malformed_still_fires_on_an_undeclared_pid() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        // Same malformed-PCR shape as pcr_malformed_does_not_seed_last_pcr_by_pid
        // above (reserved bits malformed), but on 0x1FFE — a PID no PMT declared.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1FFEu16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1FFEu16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0x7C; // reserved bits malformed
        buf[11] = 0;
        demuxer.feed(&buf).unwrap();

        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrMalformed { .. },
                    ..
                }
            )),
            "PcrMalformed must still fire on an undeclared PID: {events:?}"
        );
        assert!(
            !demuxer.pcr.contains(0x1FFE),
            "a malformed PCR on an undeclared PID must not seed last_pcr_by_pid"
        );
    }

    #[test]
    fn pcr_malformed_strict_timing_rejects() {
        // StrictMode::TimingOnly must escalate PcrMalformed to StrictRejection.
        let mut demuxer = Demuxer::with_config(
            DemuxerConfig::builder()
                .strict(StrictMode::TimingOnly)
                .build(),
        );
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0x7C; // malformed reserved bits
        buf[11] = 0;
        let result = demuxer.feed(&buf);
        assert!(
            matches!(result, Err(DemuxError::StrictRejection(_))),
            "TimingOnly strict mode must reject malformed PCR, got {result:?}"
        );
    }

    // ── unwrap_timestamps accumulator (opt-in PTS/DTS unwrap) ───────────
    // White-box coverage of `unwrap_pts` / `unwrap_secondary_ts` in
    // isolation. Wire-level (mux -> bytes -> demux) coverage, including
    // the default-off byte-identity guarantee and the missing-PTS guard,
    // lives in `tests/mpegts/pts_unwrap.rs`.

    #[test]
    fn unwrap_pts_first_call_anchors_at_raw_value() {
        let mut d = Demuxer::new();
        let out = d.unwrap_pts(0x100, Pts90khz::new(12_345));
        assert_eq!(out.as_ticks(), 12_345);
        assert_eq!(
            d.unwrap_of(0x100),
            Some(&UnwrapState {
                last_raw: 12_345,
                last_unwrapped: 12_345,
            })
        );
    }

    #[test]
    fn unwrap_pts_forward_progress_within_epoch_leaves_derived_offset_at_zero() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(1_000));
        let out = d.unwrap_pts(0x100, Pts90khz::new(91_000));
        assert_eq!(out.as_ticks(), 91_000, "no wrap — derived offset stays 0");
        assert_eq!(
            d.unwrap_of(0x100),
            Some(&UnwrapState {
                last_raw: 91_000,
                last_unwrapped: 91_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_detects_forward_wrap() {
        let mut d = Demuxer::new();
        let raw1 = (1i64 << 33) - 90_000;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1));
        // Wraps forward by 180_000 ticks past the 33-bit boundary.
        let raw2 = raw1 + 180_000 - (1i64 << 33);
        let out = d.unwrap_pts(0x100, Pts90khz::new(raw2));
        assert_eq!(out.as_ticks(), raw1 + 180_000);
        assert!(out.as_ticks() > raw1, "must be monotonic across the wrap");
        assert_eq!(
            d.unwrap_of(0x100),
            Some(&UnwrapState {
                last_raw: raw2,
                last_unwrapped: raw1 + 180_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_genuine_small_backward_step_is_not_a_wrap() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(100_000));
        // A modest backward step (e.g. an out-of-order arrival) is not a
        // wrap — the derived offset stays 0 and the emitted value is
        // non-monotonic, which correctly reflects the reorder.
        let out = d.unwrap_pts(0x100, Pts90khz::new(99_000));
        assert_eq!(out.as_ticks(), 99_000);
        assert_eq!(
            d.unwrap_of(0x100),
            Some(&UnwrapState {
                last_raw: 99_000,
                last_unwrapped: 99_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_is_per_pid() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(500_000));
        // Neither PID is owned by a known program here (no PMT parsed),
        // so the second PID's first observation anchors at its own raw
        // value — untouched by PID 0x100's already-established state.
        let out = d.unwrap_pts(0x200, Pts90khz::new(1_000));
        assert_eq!(out.as_ticks(), 1_000);
    }

    #[test]
    fn unwrap_pts_first_sample_anchors_to_its_programs_clock() {
        let mut d = Demuxer::new();
        d.pid_state_mut(0x100).program = Some(1);
        d.pid_state_mut(0x101).program = Some(1);

        // PID 0x100 establishes program 1's timeline and wraps.
        let raw1 = (1i64 << 33) - 100;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1));
        let wrapped = d.unwrap_pts(0x100, Pts90khz::new(100));
        assert_eq!(wrapped.as_ticks(), (1i64 << 33) + 100);

        // PID 0x101's FIRST sample, at the same post-wrap raw instant,
        // joins the program's epoch rather than anchoring at raw 100.
        let out = d.unwrap_pts(0x101, Pts90khz::new(100));
        assert_eq!(out.as_ticks(), (1i64 << 33) + 100);
        assert_eq!(
            d.unwrap_of(0x101),
            Some(&UnwrapState {
                last_raw: 100,
                last_unwrapped: (1i64 << 33) + 100,
            })
        );
    }

    #[test]
    fn unwrap_pts_first_sample_never_anchors_across_programs() {
        let mut d = Demuxer::new();
        d.pid_state_mut(0x100).program = Some(1);
        d.pid_state_mut(0x200).program = Some(2);

        // Program 1 wraps; program 2 has no timeline of its own yet.
        let raw1 = (1i64 << 33) - 100;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1));
        let _ = d.unwrap_pts(0x100, Pts90khz::new(100));

        // Each program carries its own time base (H.222.0 §2.4.3.5) —
        // program 2 must anchor at raw, not inherit program 1's epoch.
        let out = d.unwrap_pts(0x200, Pts90khz::new(100));
        assert_eq!(out.as_ticks(), 100);
    }

    /// A PID silent for more than half an epoch while a
    /// sibling advanced must re-anchor onto the program clock (and count).
    #[test]
    fn unwrap_pts_dormant_pid_reanchors_onto_fresher_program_clock() {
        const HALF: i64 = 1 << 32;
        let mut d = Demuxer::new();
        d.pid_state_mut(0x100).program = Some(1);
        d.pid_state_mut(0x101).program = Some(1);
        let _ = d.unwrap_pts(0x101, Pts90khz::new(0));
        for pts in [0, HALF / 2, HALF] {
            let _ = d.unwrap_pts(0x100, Pts90khz::new(pts));
        }
        let out = d.unwrap_pts(0x101, Pts90khz::new(HALF + 100));
        assert_eq!(out.as_ticks(), HALF + 100);
        assert_eq!(d.stats().unwrap_reanchors, 1);
        assert_eq!(
            d.unwrap_of(0x101),
            Some(&UnwrapState {
                last_raw: HALF + 100,
                last_unwrapped: HALF + 100,
            })
        );
    }

    /// Control: a small reorder keeps the PID's own delta and
    /// does not count as a re-anchor.
    #[test]
    fn unwrap_pts_small_reorder_keeps_own_delta_and_does_not_count() {
        let mut d = Demuxer::new();
        d.pid_state_mut(0x100).program = Some(1);
        d.pid_state_mut(0x101).program = Some(1);
        let _ = d.unwrap_pts(0x101, Pts90khz::new(1_000));
        let _ = d.unwrap_pts(0x100, Pts90khz::new(1_000));
        let _ = d.unwrap_pts(0x100, Pts90khz::new(2_000));
        let out = d.unwrap_pts(0x101, Pts90khz::new(900));
        assert_eq!(out.as_ticks(), 900);
        assert_eq!(d.stats().unwrap_reanchors, 0);
    }

    #[test]
    fn unwrap_secondary_ts_reuses_current_offset_without_mutating_state() {
        let mut d = Demuxer::new();
        let raw1 = (1i64 << 33) - 90_000;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1)); // anchors, offset 0
        let raw2 = raw1 + 180_000 - (1i64 << 33);
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw2)); // wraps — offset becomes 1<<33
        let state_before = d.unwrap_of(0x100).copied();

        // A DTS just below the (already-wrapped) PTS on the same PES
        // reuses the post-wrap offset — no independent wrap check.
        let dts =
            TimestampUnwrapper::secondary(d.unwrap_of(0x100).copied(), Pts90khz::new(raw2 - 3_000));
        assert_eq!(dts.as_ticks(), (1i64 << 33) + raw2 - 3_000);
        // The DTS call must not mutate the accumulator.
        assert_eq!(d.unwrap_of(0x100).copied(), state_before);
    }

    #[test]
    fn unwrap_secondary_ts_defaults_to_zero_offset_for_unseen_pid() {
        let d = Demuxer::new();
        let out = TimestampUnwrapper::secondary(d.unwrap_of(0x999).copied(), Pts90khz::new(42));
        assert_eq!(out.as_ticks(), 42);
    }

    /// The straddle case: PTS has already wrapped (small raw) while this AU's
    /// DTS is still pre-wrap (large raw). A bare per-PID offset
    /// (`unwrap_secondary_ts`) puts the DTS a full `1 << 33` epoch above where
    /// it belongs. Worked trace: `pts_diff_33bit(1<<33-200, 100) == -300`, so
    /// the unwrapped DTS is `(1<<33+100) + (-300) == 1<<33-200` — in the
    /// PRE-wrap epoch, below the unwrapped PTS, NOT `1<<33 + (1<<33-200)`.
    #[test]
    fn unwrap_dts_with_pts_straddling_wrap_stays_in_pre_wrap_epoch() {
        let pts_raw = Pts90khz::new(100);
        let dts_raw = Pts90khz::new((1i64 << 33) - 200);
        let unwrapped_pts = Pts90khz::new((1i64 << 33) + 100);

        let dts = TimestampUnwrapper::dts_with_pts(unwrapped_pts, pts_raw, dts_raw);

        assert_eq!(dts.as_ticks(), (1i64 << 33) - 200);
        assert!(
            dts.as_ticks() < unwrapped_pts.as_ticks(),
            "DTS must stay below PTS across the straddle, got dts={} pts={}",
            dts.as_ticks(),
            unwrapped_pts.as_ticks()
        );
    }

    /// Same-epoch regression guard: when DTS and PTS are both on the same side
    /// of any wrap (the common case), the DTS must equal the plain same-epoch
    /// answer. `pts_diff_33bit(87_000, 90_000) == -3000`, so unwrapped DTS ==
    /// `90_000 + (-3000) == 87_000` (offset 0, matches the raw value).
    #[test]
    fn unwrap_dts_with_pts_same_epoch_matches_raw_delta() {
        let pts_raw = Pts90khz::new(90_000);
        let dts_raw = Pts90khz::new(87_000);
        let unwrapped_pts = Pts90khz::new(90_000); // no wrap yet — offset 0

        let dts = TimestampUnwrapper::dts_with_pts(unwrapped_pts, pts_raw, dts_raw);

        assert_eq!(dts.as_ticks(), 87_000);
    }

    // Unit tests of the types themselves, no `Demuxer` involved.

    fn step(
        u: &mut TimestampUnwrapper,
        own: &mut Option<UnwrapState>,
        program: Option<u16>,
        raw: i64,
    ) -> i64 {
        u.unwrap(own, program, Pts90khz::new(raw)).as_ticks()
    }

    #[test]
    fn unwrapper_carries_a_forward_wrap_and_a_backward_step_across_the_boundary() {
        let mut u = TimestampUnwrapper::default();
        let mut own = None;
        let before = (1i64 << 33) - 90_000;
        assert_eq!(step(&mut u, &mut own, None, before), before);
        // Forward across the 33-bit boundary: monotonic.
        assert_eq!(step(&mut u, &mut own, None, 90_000), (1i64 << 33) + 90_000);
        // A late pre-wrap arrival steps back into the pre-wrap epoch.
        assert_eq!(step(&mut u, &mut own, None, before), before);
        assert_eq!(u.reanchors(), 0);
    }

    #[test]
    fn unwrapper_reanchors_a_dormant_pid_and_counts_it() {
        const HALF: i64 = 1 << 32;
        let mut u = TimestampUnwrapper::default();
        let (mut a, mut b) = (None, None);
        step(&mut u, &mut b, Some(1), 0);
        for pts in [0, HALF / 2, HALF] {
            step(&mut u, &mut a, Some(1), pts);
        }
        assert_eq!(step(&mut u, &mut b, Some(1), HALF + 100), HALF + 100);
        assert_eq!(u.reanchors(), 1);
        u.reset_reanchors();
        assert_eq!(u.reanchors(), 0);
    }

    #[test]
    fn unwrapper_retire_program_forgets_the_reference_so_a_new_pid_anchors_at_raw() {
        let mut u = TimestampUnwrapper::default();
        let mut a = None;
        step(&mut u, &mut a, Some(1), 5_000);
        assert!(u.has_program(1));
        u.retire_program(1);
        assert!(!u.has_program(1));
        let mut fresh = None;
        assert_eq!(step(&mut u, &mut fresh, Some(1), 100), 100);
        u.clear();
        assert!(!u.has_program(1));
    }

    #[test]
    fn pcr_history_retains_only_declared_pids() {
        let mut h = PcrHistory::default();
        h.record(0x100, 27_000_000);
        h.record(0x200, 54_000_000);
        assert_eq!(h.last(0x100), Some(27_000_000));
        // PCR PID 0x100 stays declared; 0x200 is no longer named by a PMT.
        h.retain_declared(|pid| pid == 0x100);
        assert!(h.contains(0x100));
        assert!(!h.contains(0x200));
        assert_eq!(h.last(0x200), None);
        h.clear();
        assert!(!h.contains(0x100));
    }
}
