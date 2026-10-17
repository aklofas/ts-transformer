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
    /// Last continuity counter seen on a payload-bearing packet.
    pub(super) cc: Option<u8>,
    /// The packet before the current one was a spec-legal duplicate
    /// (H.222.0 §2.4.3.3 allows exactly one).
    pub(super) dup: bool,
    /// Bytes of the last routed payload packet, for the duplicate compare.
    pub(super) last_pkt_raw: Option<[u8; 188]>,
    pub(super) last_pts: Option<i64>,
    /// Opt-in PTS/DTS unwrap accumulator.
    pub(super) unwrap: Option<UnwrapState>,
    /// Public per-stream counters, created on the first event.
    pub(super) stats: Option<StreamStats>,
    /// Codec counters, created on the first counted event.
    pub(super) codec: Option<StreamCodecCounters>,
    /// "Emitted once for the current PMT version" flags.
    pub(super) subtitle_missing_descriptor_emitted: bool,
    pub(super) av1_registration_malformed_emitted: bool,
    pub(super) subtitle_descriptor_ambiguous_emitted: bool,
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

    /// Forget every timing baseline of the PID: continuity, PTS, unwrap.
    pub(super) fn clear_timing(&mut self) {
        self.clear_continuity();
        self.last_pts = None;
        self.unwrap = None;
    }

    /// What `Demuxer::reset_sync` clears. Counters (`stats`, `codec`) and
    /// `subtitle_seen` survive a reconnect; `reset_stats` owns those.
    pub(super) fn reset_sync(&mut self) {
        self.clear_timing();
        self.stream_kind = None;
        self.program = None;
        self.subtitle_missing_descriptor_emitted = false;
        self.av1_registration_malformed_emitted = false;
        self.subtitle_descriptor_ambiguous_emitted = false;
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
            cc,
            dup,
            last_pkt_raw,
            last_pts,
            unwrap,
            stats,
            codec,
            subtitle_missing_descriptor_emitted,
            av1_registration_malformed_emitted,
            subtitle_descriptor_ambiguous_emitted,
            subtitle_seen,
        } = self;
        stream_kind.is_none()
            && program.is_none()
            && cc.is_none()
            && !dup
            && last_pkt_raw.is_none()
            && last_pts.is_none()
            && unwrap.is_none()
            && stats.is_none()
            && codec.is_none()
            && !subtitle_missing_descriptor_emitted
            && !av1_registration_malformed_emitted
            && !subtitle_descriptor_ambiguous_emitted
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
    /// `(pid, kind, program_number)` for every accepted PMT stream: sets the
    /// PID's kind and owner (new and persisting PIDs alike).
    pub(super) added: Vec<(u16, StreamKind, u16)>,
    /// Elementary PIDs that left the topology: their whole entry goes.
    pub(super) retired: Vec<u16>,
    /// PMT PIDs of removed programs: only the continuity baseline goes.
    pub(super) retired_pmt_pids: Vec<u16>,
    /// PIDs that persist under a different kind: timing and PES buffer go.
    pub(super) kind_changed: Vec<u16>,
    /// `(old, new)` PCR_PID of every program whose declaration moved or
    /// vanished; any entry means the PCR history must be re-checked.
    pub(super) pcr_pid_changes: Vec<(Option<u16>, Option<u16>)>,
}
