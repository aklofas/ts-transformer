//! PSI + PCR cadence scheduling for `Muxer`.
//!
//! Reads and updates each program slot's `psi_last` / `pcr_last` state
//! through the `psi_due` / `pcr_due` predicates that gate PSI/PCR emission per
//! the configured intervals.
//!
//! `maybe_emit_psi` is the centerpiece — it emits one PAT + one PMT per
//! live program when the per-program PSI tick is due, tracks the last-emission
//! PTS, and updates every slot's `psi_last` so the next call's `psi_due`
//! returns false until the next interval boundary.

use super::Muxer;
use super::psi::{PmtStreamEntry, write_pat_packet, write_pmt_packet};
use super::state::pmt_stream_type_byte;
use super::ts::write_pcr_only_packet;
use crate::mpegts::common::{Pcr27mhz, Pts90khz};
use alloc::vec::Vec;

impl Muxer {
    pub(super) fn psi_due(&self, prog_idx: usize, pts_90khz: i64) -> bool {
        match self.slots[prog_idx].psi_last {
            None => true,
            Some(last_masked) => {
                let now_masked = Pts90khz::new(pts_90khz).masked_33bit();
                let delta = crate::mpegts::common::pts_diff_33bit(now_masked, last_masked);
                delta >= self.psi_interval_90khz
            }
        }
    }

    pub(super) fn pcr_due(&self, prog_idx: usize, pts_90khz: i64) -> bool {
        match self.slots[prog_idx].pcr_last {
            None => true,
            Some(last) => {
                // PCR is at 27 MHz; the 33-bit base wraps at 2^33 base ticks.
                // Convert both to 33-bit base and use the same modular helper,
                // then compare in 90 kHz units.
                let now_base_masked = Pts90khz::new(pts_90khz).masked_33bit();
                let last_base_masked = (last / 300) & ((1u64 << 33) - 1);
                let delta_90khz =
                    crate::mpegts::common::pts_diff_33bit(now_base_masked, last_base_masked);
                let threshold_90khz = (self.pcr_interval_27mhz / 300) as i64;
                delta_90khz >= threshold_90khz
            }
        }
    }

    /// Returns true when the configured PCR PID has fallen behind the
    /// `pcr_interval_ms` ceiling AND the current push is landing on a
    /// non-PCR PID (so the in-band PCR-on-push path won't run). Mirrors
    /// the role of `psi_due` for the PCR-only adaptation-only-packet
    /// injection.
    ///
    /// `current_pid` is the elementary PID the push path is about to
    /// write its payload onto. When it equals the PCR PID, the regular
    /// PCR-on-push path already handles emission and this predicate
    /// returns false to avoid duplicate PCR samples.
    pub(super) fn pcr_only_due(&self, prog_idx: usize, pts_90khz: i64, current_pid: u16) -> bool {
        self.slots[prog_idx].pcr_pid != current_pid && self.pcr_due(prog_idx, pts_90khz)
    }

    /// Emit one adaptation-field-only PCR packet on the program's PCR PID
    /// when [`Self::pcr_only_due`] is true. No-op otherwise. Used to keep
    /// the PCR cadence within H.222.0 Annex D's 100ms ceiling when the
    /// caller pushes only to non-PCR PIDs (e.g. KLV-only push on a video-
    /// PCR config). Updates the slot's `pcr_last` to the emitted PCR's
    /// 27 MHz value.
    pub(super) fn maybe_emit_pcr_only(
        &mut self,
        prog_idx: usize,
        pts_90khz: i64,
        current_pid: u16,
    ) {
        if !self.pcr_only_due(prog_idx, pts_90khz, current_pid) {
            return;
        }
        let pcr_pid = self.slots[prog_idx].pcr_pid;
        let pcr = Pcr27mhz::from_pts(Pts90khz::new(pts_90khz));
        let mut pkt = [0u8; 188];
        write_pcr_only_packet(&mut pkt, pcr_pid, pcr, &self.counters);
        self.queue.push_back(pkt);
        self.slots[prog_idx].pcr_last = Some(pcr.as_ticks());
    }

    pub(super) fn maybe_emit_psi(&mut self, prog_idx: usize, pts_90khz: i64) {
        if !self.psi_due(prog_idx, pts_90khz) {
            return;
        }
        // Emit one PAT that lists all live programs, then one PMT per live
        // program.
        // The PAT is emitted on every PSI tick regardless of which program
        // triggered the tick, so all programs' state is always visible to
        // receivers after a single PSI interval.
        let mut pat = [0u8; 188];
        let pat_entries = self.pat_entries();
        write_pat_packet(&mut pat, &pat_entries, self.pat_version, &mut self.counters);
        self.queue.push_back(pat);

        // One PMT per live program — iterate the full slot set so every live
        // program gets a fresh PMT on the tick (not just the triggering one).
        for slot in &self.slots {
            let Some(prog) = &slot.live else {
                continue;
            };
            let mut entries: Vec<PmtStreamEntry> = Vec::with_capacity(prog.streams.len());
            for (i, spec) in prog.streams.iter().enumerate() {
                let stream_type = pmt_stream_type_byte(spec);
                entries.push(PmtStreamEntry {
                    stream_type,
                    elementary_pid: spec.pid(),
                    descriptors: &slot.pmt_descriptor_cache[i],
                });
            }

            let mut pmt = [0u8; 188];
            write_pmt_packet(
                &mut pmt,
                prog,
                slot.pcr_pid,
                &entries,
                slot.pmt_version,
                &mut self.counters,
            )
            .expect("validated MuxerConfig must produce single-section PMT");
            self.queue.push_back(pmt);
        }

        // Update psi_last for ALL slots to the same masked timestamp.
        // maybe_emit_psi emits one PAT + one PMT per program on a single
        // tick, so every program's state is now fresh; updating only the
        // triggering index left other programs' psi_last == None, which
        // psi_due reads as "always due" and would re-fire the entire
        // PAT+PMTs set on the next push for a different program.
        let masked = Pts90khz::new(pts_90khz).masked_33bit();
        for slot in self.slots.iter_mut() {
            slot.psi_last = Some(masked);
        }
    }
}
