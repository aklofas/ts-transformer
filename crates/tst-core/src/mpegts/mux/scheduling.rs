//! PSI + PCR cadence scheduling for `Muxer`.
//!
//! Owns the `psi_last` / `pcr_last` per-program ring-buffer state and
//! the `psi_due` / `pcr_due` predicates that gate PSI/PCR emission per
//! the configured intervals.
//!
//! `maybe_emit_psi` is the centerpiece — it emits one PAT + one PMT per
//! program when the per-program PSI tick is due, tracks the last-emission
//! PTS, and updates `psi_last[prog_idx]` so the next call's `psi_due`
//! returns false until the next interval boundary.

use super::psi::{PmtStreamEntry, write_pat_packet, write_pmt_packet};
use super::state::{audio_stream_type_byte, klv_stream_type_byte, video_stream_type_byte};
use super::ts::write_pcr_only_packet;
use super::{Muxer, StreamSpec};
use crate::mpegts::common::{Pcr27mhz, Pts90khz};
use alloc::vec::Vec;

impl Muxer {
    pub(super) fn psi_due(&self, prog_idx: usize, pts_90khz: i64) -> bool {
        match self.psi_last[prog_idx] {
            None => true,
            Some(last_masked) => {
                let now_masked = Pts90khz::new(pts_90khz).masked_33bit();
                let delta = crate::mpegts::common::pts_diff_33bit(now_masked, last_masked);
                delta >= self.psi_interval_90khz
            }
        }
    }

    pub(super) fn pcr_due(&self, prog_idx: usize, pts_90khz: i64) -> bool {
        match self.pcr_last[prog_idx] {
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
        self.pcr_pids[prog_idx] != current_pid && self.pcr_due(prog_idx, pts_90khz)
    }

    /// Emit one adaptation-field-only PCR packet on the program's PCR PID
    /// when [`Self::pcr_only_due`] is true. No-op otherwise. Used to keep
    /// the PCR cadence within H.222.0 Annex D's 100ms ceiling when the
    /// caller pushes only to non-PCR PIDs (e.g. KLV-only push on a video-
    /// PCR config). Updates `pcr_last[prog_idx]` to the emitted PCR's
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
        let pcr_pid = self.pcr_pids[prog_idx];
        let pcr = Pcr27mhz::from_pts(Pts90khz::new(pts_90khz));
        let mut pkt = [0u8; 188];
        write_pcr_only_packet(&mut pkt, pcr_pid, pcr, &self.counters);
        self.queue.push_back(pkt);
        self.pcr_last[prog_idx] = Some(pcr.as_ticks());
    }

    pub(super) fn maybe_emit_psi(&mut self, prog_idx: usize, pts_90khz: i64) {
        if !self.psi_due(prog_idx, pts_90khz) {
            return;
        }
        // Emit one PAT that lists all programs, then one PMT per program.
        // The PAT is emitted on every PSI tick regardless of which program
        // triggered the tick, so all programs' state is always visible to
        // receivers after a single PSI interval.
        let mut pat = [0u8; 188];
        write_pat_packet(&mut pat, &self.config, &mut self.counters);
        self.queue.push_back(pat);

        // One PMT per program — iterate the full program set so every program
        // gets a fresh PMT on the tick (not just the triggering program).
        for pidx in 0..self.config.programs.len() {
            let prog = &self.config.programs[pidx];
            let mut entries: Vec<PmtStreamEntry> = Vec::with_capacity(prog.streams.len());
            for (i, spec) in prog.streams.iter().enumerate() {
                let stream_type = match spec {
                    StreamSpec::Video { codec, .. } => video_stream_type_byte(*codec),
                    StreamSpec::Klv { stream_type, .. } => klv_stream_type_byte(*stream_type),
                    StreamSpec::Audio { codec, .. } => audio_stream_type_byte(*codec),
                    // Caller-chosen byte; validate() guarantees the demux
                    // classifier maps it (with its descriptors) to Unknown.
                    StreamSpec::Data { stream_type, .. } => *stream_type,
                    // All four subtitle codecs share PMT stream_type 0x06
                    // (PrivateData); the per-stream descriptor cache carries
                    // the codec-specific disambiguator (subtitling_descriptor /
                    // teletext_descriptor / Registration "GA94" / "VTTC").
                    StreamSpec::Subtitle { .. } => super::StreamType::KlvPrivate.as_u8(),
                };
                entries.push(PmtStreamEntry {
                    stream_type,
                    elementary_pid: spec.pid(),
                    descriptors: &self.pmt_descriptor_caches[pidx][i],
                });
            }

            let mut pmt = [0u8; 188];
            write_pmt_packet(
                &mut pmt,
                prog,
                self.pcr_pids[pidx],
                &entries,
                &mut self.counters,
            )
            .expect("validated MuxerConfig must produce single-section PMT");
            self.queue.push_back(pmt);
        }

        // Update psi_last for ALL programs to the same masked timestamp.
        // maybe_emit_psi emits one PAT + one PMT per program on a single
        // tick, so every program's state is now fresh; updating only the
        // triggering index left other programs' psi_last == None, which
        // psi_due reads as "always due" and would re-fire the entire
        // PAT+PMTs set on the next push for a different program.
        let masked = Pts90khz::new(pts_90khz).masked_33bit();
        for slot in self.psi_last.iter_mut() {
            *slot = Some(masked);
        }
    }
}
