//! Per-PID demuxer state in one map, and the topology diff that edits it.
//!
//! Everything the demuxer remembers about one PID lives in one [`PidState`]
//! entry of `Demuxer::pid_state`. A PID that leaves the topology is therefore
//! removed with ONE `remove` (plus the reassembler's own buffer, which is a
//! standalone machine) instead of a hand-kept list of maps, so a new per-PID
//! field cannot be forgotten at teardown.
//!
//! PSI topology changes are reported as a [`TopologyDiff`]; the coordinator
//! applies it in one place (`Demuxer::apply_topology_diff`).

use crate::mpegts::demux::event::StreamKind;
use crate::mpegts::demux::timestamp::UnwrapState;
use crate::mpegts::stats::{CodecKind, StreamCodecCounters, StreamStats};
use alloc::vec::Vec;

/// Everything the demuxer tracks for one PID. An entry exists only while at
/// least one field is set; a PID with nothing to remember has no entry.
#[derive(Debug, Default)]
pub(super) struct PidState {
    /// PES dispatch kind, set by the PMT that declares the PID.
    pub(super) stream_kind: Option<StreamKind>,
    /// Owning `program_number`, set with `stream_kind`.
    pub(super) program: Option<u16>,
    /// PMT `stream_type` byte that declared the PID, set with `stream_kind`.
    pub(super) stream_type: Option<u8>,
    /// Last continuity counter seen on a payload-bearing packet.
    pub(super) cc: Option<u8>,
    /// The packet before the current one was a spec-legal duplicate
    /// (H.222.0 §2.4.3.3 allows exactly one).
    pub(super) dup: bool,
    /// Bytes of the last routed payload packet, for the duplicate compare.
    pub(super) last_pkt_raw: Option<[u8; 188]>,
    pub(super) last_pts: Option<i64>,
    /// A signalled time-base reset arrived while a PES was in flight: that
    /// PES belongs to the old clock and is judged against `last_pts`, then
    /// `last_pts` is cleared so the next PES starts the new baseline.
    pub(super) pts_baseline_clears_after_pes: bool,
    /// A PES without a PTS arrived since the last coded PTS on an audio or
    /// video PID; the next coded PTS judges the spacing around the run.
    pub(super) pts_less_since_pts: bool,
    /// The program's PCR (as 90 kHz ticks) when the current PTS-less run
    /// started on an audio or video PID; `None` before the first PCR.
    pub(super) pts_less_run_pcr: Option<u64>,
    /// The current PTS-less run already raised `MissingRequiredPts`.
    pub(super) pts_less_run_reported: bool,
    /// Opt-in PTS/DTS unwrap accumulator.
    pub(super) unwrap: Option<UnwrapState>,
    /// AAC-LATM: bytes the last LOAS record still owes the next PES
    /// (ISO/IEC 14496-3 §1.7.2); `None` = unknown.
    pub(super) latm_carry: Option<usize>,
    /// AAC-LATM: consecutive carry-unknown PES that showed no LOAS record,
    /// saturating at the count that raised the issue.
    pub(super) latm_unsynced: u8,
    /// Public per-stream counters, created on the first event.
    pub(super) stats: Option<StreamStats>,
    /// Codec counters, created on the first counted event.
    pub(super) codec: Option<StreamCodecCounters>,
    /// "Emitted once for the current PMT version" flags.
    pub(super) subtitle_missing_descriptor_emitted: bool,
    pub(super) av1_registration_malformed_emitted: bool,
    pub(super) subtitle_descriptor_ambiguous_emitted: bool,
    pub(super) sync_pes_shape_emitted: bool,
    /// A subtitle sample has been counted for this PID.
    pub(super) subtitle_seen: bool,
}

impl PidState {
    /// Forget the continuity baseline: counter, duplicate flag, raw bytes.
    pub(super) fn clear_continuity(&mut self) {
        self.cc = None;
        self.dup = false;
        self.last_pkt_raw = None;
    }

    /// Forget the PTS baseline and any PTS-less run measured against it.
    pub(super) fn clear_pts_baseline(&mut self) {
        self.last_pts = None;
        self.pts_less_since_pts = false;
        self.pts_less_run_pcr = None;
        self.pts_less_run_reported = false;
    }

    /// Forget every timing baseline of the PID: continuity, PTS, unwrap,
    /// and the LATM record position.
    pub(super) fn clear_timing(&mut self) {
        self.clear_continuity();
        self.clear_pts_baseline();
        self.pts_baseline_clears_after_pes = false;
        self.unwrap = None;
        self.latm_carry = None;
        self.latm_unsynced = 0;
    }

    /// What `Demuxer::reset_sync` clears. Counters (`stats`, `codec`) and
    /// `subtitle_seen` survive a reconnect; `reset_stats` owns those.
    pub(super) fn reset_sync(&mut self) {
        self.clear_timing();
        self.stream_kind = None;
        self.program = None;
        self.stream_type = None;
        self.subtitle_missing_descriptor_emitted = false;
        self.av1_registration_malformed_emitted = false;
        self.subtitle_descriptor_ambiguous_emitted = false;
        self.sync_pes_shape_emitted = false;
    }

    /// The PID left the topology: forget everything except the lifetime
    /// counters (`stats`, `codec`, `subtitle_seen`), which `reset_stats`
    /// owns.
    pub(super) fn retire(&mut self) {
        let Self {
            stats,
            codec,
            subtitle_seen,
            ..
        } = core::mem::take(self);
        self.stats = stats;
        self.codec = codec;
        self.subtitle_seen = subtitle_seen;
    }

    /// What `Demuxer::reset_stats` clears.
    pub(super) fn reset_stats(&mut self) {
        self.stats = None;
        self.codec = None;
        self.subtitle_seen = false;
    }

    /// The codec counters, created as `kind` on first use.
    pub(super) fn codec_mut(&mut self, kind: CodecKind) -> &mut StreamCodecCounters {
        self.codec
            .get_or_insert_with(|| StreamCodecCounters::new(kind))
    }

    /// `true` when nothing is set, so the entry can be dropped.
    pub(super) fn is_vacant(&self) -> bool {
        // Exhaustive on purpose: a new field fails to compile here until it
        // is part of the vacancy test.
        let Self {
            stream_kind,
            program,
            stream_type,
            cc,
            dup,
            last_pkt_raw,
            last_pts,
            pts_baseline_clears_after_pes,
            pts_less_since_pts,
            pts_less_run_pcr,
            pts_less_run_reported,
            unwrap,
            latm_carry,
            latm_unsynced,
            stats,
            codec,
            subtitle_missing_descriptor_emitted,
            av1_registration_malformed_emitted,
            subtitle_descriptor_ambiguous_emitted,
            sync_pes_shape_emitted,
            subtitle_seen,
        } = self;
        stream_kind.is_none()
            && program.is_none()
            && stream_type.is_none()
            && cc.is_none()
            && !dup
            && last_pkt_raw.is_none()
            && last_pts.is_none()
            && !pts_baseline_clears_after_pes
            && !pts_less_since_pts
            && pts_less_run_pcr.is_none()
            && !pts_less_run_reported
            && unwrap.is_none()
            && latm_carry.is_none()
            && *latm_unsynced == 0
            && stats.is_none()
            && codec.is_none()
            && !subtitle_missing_descriptor_emitted
            && !av1_registration_malformed_emitted
            && !subtitle_descriptor_ambiguous_emitted
            && !sync_pes_shape_emitted
            && !subtitle_seen
    }
}

/// Set a once-flag; `true` when this call set it (it was clear before).
pub(super) fn set_once(flag: &mut bool) -> bool {
    !core::mem::replace(flag, true)
}

/// What one PAT or PMT change does to per-PID state. Computed without
/// touching `Demuxer::pid_state`; applied by `Demuxer::apply_topology_diff`.
#[derive(Debug, Default)]
pub(super) struct TopologyDiff {
    /// `(pid, kind, program_number, stream_type)` for every accepted PMT
    /// stream: sets the PID's kind, owner and PMT `stream_type` byte (new and
    /// persisting PIDs alike).
    pub(super) added: Vec<(u16, StreamKind, u16, u8)>,
    /// Elementary PIDs that left the topology: their state goes, their
    /// lifetime counters stay until `reset_stats`.
    pub(super) retired: Vec<u16>,
    /// PMT PIDs of removed programs: only the continuity baseline goes.
    pub(super) retired_pmt_pids: Vec<u16>,
    /// PIDs that persist under a different kind: timing and PES buffer go.
    pub(super) kind_changed: Vec<u16>,
    /// `(old, new)` PCR_PID of every program whose declaration moved or
    /// vanished; any entry means the PCR history must be re-checked.
    pub(super) pcr_pid_changes: Vec<(Option<u16>, Option<u16>)>,
}
