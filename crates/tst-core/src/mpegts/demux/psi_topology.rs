//! PSI section dispatch + PAT/PMT topology tracking.
//!
//! Hosts 10 helper methods on `Demuxer`:
//!
//! - `handle_psi` — router from `process_packet` into PAT vs PMT
//!   continuation logic, including strict-mode CC-discontinuity drop.
//! - `dispatch_psi_result` — routes a completed PAT/PMT reassembly
//!   outcome to the corresponding `handle_*_section` call.
//! - `handle_pat_section` — full PAT parse + program-tracker diff
//!   (programs added / removed across PAT version bumps).
//! - `apply_pat_programs` — diffs a parsed PAT's program list against
//!   tracked state, dropping programs (and their elementary PID state)
//!   that disappeared.
//! - `retire_undeclared_pcr_history` — drops the PCR history of every PID
//!   that no program declares as its `PCR_PID` any more.
//! - `pmt_topology_diff` — what a new PMT version changes in per-PID state,
//!   as a `TopologyDiff` the coordinator applies (`apply_topology_diff`,
//!   the one place PIDs enter and leave `pid_state`).
//! - `handle_pmt_section` — full PMT parse + per-program PMT version
//!   bump + stream-binding update + KLV link inference + descriptor
//!   diagnostics + cross-program PID collision policy.
//! - `build_program_map` — composes a `ProgramMap` event from a parsed
//!   PMT after the topology has settled.
//! - `check_psi_syntax` — validates fixed/reserved PSI syntax fields
//!   (section_syntax_indicator, section_number, reserved bits) on a
//!   section whose CRC already passed; shared by both section handlers.
//! - `klv_mismatch_insert` — per-program coalesce-set helper for KLV
//!   stream-type-mismatch event deduplication.
//!
//! All items are visible only within `mpegts::demux` (`pub(super)` or
//! private).

use crate::codec::VideoCodec;
use crate::error::DemuxError;
use crate::mpegts::common::StreamTypeCode;
use crate::mpegts::demux::event::{
    DemuxEvent, KlvLink, LinkSource, NonConformantIssue, ProgramMap, PsiSyntaxKind, StreamId,
    StreamInfo, StreamKind,
};
use crate::mpegts::demux::pat_reassemble::PatReassemblyOutcome;
use crate::mpegts::demux::pid_state::{TopologyDiff, set_once};
use crate::mpegts::demux::psi::{PatEntry, Pmt, PsiParseError, parse_pat_section, parse_pmt};
use crate::mpegts::demux::psi_assembler::AssemblerError;
use crate::mpegts::demux::strict::StrictMode;
use crate::mpegts::demux::types::ProgramTracker;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

/// Three-state outcome from `Demuxer::dispatch_psi_result`. Replaces the
/// earlier two-state `bool` return that conflated "section incomplete /
/// silently dropped" with "DoS cap fired" (without disambiguation, a mid-stream
/// join that produced an `Ok(None)` from `append_continuation` was
/// indistinguishable from an overflow bail, and the new section header at
/// `payload[1+pointer_field..]` was silently discarded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PsiStep {
    /// A section was fully dispatched. Caller may loop to drain more.
    Completed,
    /// No section ready (buffer growing or continuation dropped because
    /// no prior PUSI was seen). Caller MUST keep processing — this is
    /// NOT a bail signal.
    Incomplete,
    /// 4 KiB section-size cap fired; assembler reset; NonConformant
    /// queued. Caller MUST bail.
    Overflowed,
}

impl super::demuxer::Demuxer {
    pub(super) fn handle_psi(
        &mut self,
        pid: u16,
        payload: &[u8],
        pusi: bool,
        is_pat: bool,
        cc_jumped: bool,
    ) -> Result<(), DemuxError> {
        // Strict mode: drop the partial section if a continuation packet
        // arrives with a CC jump (matches ffmpeg `mpegts.c:3118-3142`).
        // Lenient mode (opt-in via DemuxerConfig::lenient_psi_reassembly)
        // preserves today's permissive behavior — bytes are accumulated
        // regardless, either passing CRC by luck or surfacing as
        // PsiChecksumMismatch.
        if !pusi && cc_jumped && !self.options.lenient_psi_reassembly {
            if let Some(assembler) = self.psi_assemblers.get_mut(&pid) {
                assembler.reset();
            }
            let (expected, observed) = match self.last_psi_cc_jump.take() {
                Some(pair) => pair,
                None => {
                    // Invariant violation: `check_continuity` must populate
                    // `last_psi_cc_jump` before reaching this arm. Defense-in-depth:
                    // drop the event silently rather than panicking, so a future
                    // refactor that decouples `check_continuity` from `handle_psi`
                    // can't crash the demuxer in production. `debug_assert!` still
                    // catches regressions in test runs.
                    debug_assert!(
                        false,
                        "check_continuity invariant: last_psi_cc_jump should be populated"
                    );
                    return Ok(());
                }
            };
            let stream = self
                .lookup_stream(pid)
                .unwrap_or_else(|| StreamId::anonymous(pid, 0));
            self.queue_nonconformant(
                stream,
                NonConformantIssue::PsiCcDiscontinuity {
                    pid,
                    expected,
                    observed,
                },
            );
            return Ok(());
        }

        // Per ISO/IEC 13818-1 §2.4.4.1, PUSI with `pointer_field > 0`
        // signals section-mapped layout: `payload[1..1+pointer_field]`
        // is the tail of a prior partial section that started in an
        // earlier packet, and `payload[1+pointer_field..]` begins a new
        // section. Section-mapped payloads can also carry multiple
        // complete sections back-to-back.
        //
        // Sequence:
        //   1. If PUSI: append the prior-section tail (`payload[1..1+pf]`)
        //      as continuation. If it completes a section, dispatch it.
        //   2. If PUSI: start a new section at `payload[1+pf..]`. If it
        //      completes, dispatch.
        //   3. Loop `try_complete_section()` to drain any subsequent
        //      back-to-back sections from the same payload window.
        //   4. Continuation packets (PUSI=0) just append.

        let mut iter = if pusi {
            if payload.is_empty() {
                return Ok(());
            }
            let pointer_field = payload[0] as usize;
            if 1 + pointer_field > payload.len() {
                return Ok(());
            }
            // Step 1: feed the prior-section tail as continuation FIRST,
            // then start the new section. `dispatch_psi_result` handles
            // overflow + PAT/PMT routing for each completed section.
            //
            // CRITICAL: only `PsiStep::Overflowed` bails — `Incomplete`
            // here means the continuation didn't complete the prior
            // section, OR (the silent-discard case) no prior PUSI was
            // ever seen on this PID and `append_continuation` dropped
            // the bytes per §2.4.4.4. Both are normal — Step 2 must
            // still run to start the NEW section at `payload[1+pf..]`.
            // Bailing on `Ok(false)` here would silently discard the new
            // section header in the mid-stream-join scenario.
            if pointer_field > 0 {
                let cont = &payload[1..1 + pointer_field];
                let assembler = self.psi_assemblers.entry(pid).or_default();
                let res = assembler.append_continuation(cont);
                if matches!(
                    self.dispatch_psi_result(pid, is_pat, res)?,
                    PsiStep::Overflowed
                ) {
                    return Ok(());
                }
            }
            // Step 2: start the new section at the pointer-field offset.
            let new_section_payload = &payload[1 + pointer_field..];
            let assembler = self.psi_assemblers.entry(pid).or_default();
            let res = assembler.start_new_section(new_section_payload);
            match self.dispatch_psi_result(pid, is_pat, res)? {
                PsiStep::Overflowed => return Ok(()),
                PsiStep::Completed => true,
                PsiStep::Incomplete => false,
            }
        } else {
            // Continuation packet — no pointer_field, just append.
            let assembler = self.psi_assemblers.entry(pid).or_default();
            let res = assembler.append_continuation(payload);
            match self.dispatch_psi_result(pid, is_pat, res)? {
                PsiStep::Overflowed => return Ok(()),
                PsiStep::Completed => true,
                PsiStep::Incomplete => false,
            }
        };

        // Step 3: drain any subsequent complete sections in the same
        // payload (section-mapped layout per §2.4.4.1). The assembler
        // returns None when the leftover buffer is empty, starts with
        // 0xFF (stuffing), or doesn't yet contain a complete section.
        while iter {
            let assembler = self.psi_assemblers.entry(pid).or_default();
            let res = assembler.try_complete_section();
            iter = matches!(
                self.dispatch_psi_result(pid, is_pat, res)?,
                PsiStep::Completed
            );
        }
        Ok(())
    }

    /// Centralized PSI section dispatch + overflow handling.
    ///
    /// Returns a three-state result that disambiguates "no section yet"
    /// from "DoS-cap fired":
    ///
    /// - `PsiStep::Completed` — a section was dispatched (caller may
    ///   loop to drain additional sections from the same payload).
    /// - `PsiStep::Incomplete` — no section ready: the buffer is still
    ///   growing, OR `append_continuation` silently dropped bytes
    ///   because no prior PUSI was seen on this PID (mid-stream join).
    ///   Caller MUST continue processing (e.g. start a new section at
    ///   the pointer-field offset); this is NOT a bail signal.
    /// - `PsiStep::Overflowed` — the 4 KiB MAX_SECTION_SIZE cap fired
    ///   on this PID. The assembler queued a NonConformant event and
    ///   reset itself; caller MUST bail to avoid feeding more bytes
    ///   into the just-reset assembler within the same packet, which
    ///   would mask the DoS condition. Higher-bandwidth DoS attempts
    ///   are still caught on the next PUSI packet.
    fn dispatch_psi_result(
        &mut self,
        pid: u16,
        is_pat: bool,
        result: Result<Option<Vec<u8>>, AssemblerError>,
    ) -> Result<PsiStep, DemuxError> {
        let section = match result {
            Ok(Some(section)) => section,
            Ok(None) => return Ok(PsiStep::Incomplete),
            Err(AssemblerError::Overflow { observed_len })
            | Err(AssemblerError::DeclaredTooLong {
                declared_len: observed_len,
            }) => {
                // Cap fired — partial section discarded by the assembler.
                let stream = self
                    .lookup_stream(pid)
                    .unwrap_or_else(|| StreamId::anonymous(pid, 0));
                self.queue_nonconformant(
                    stream,
                    NonConformantIssue::PsiOverlongSection { pid, observed_len },
                );
                return Ok(PsiStep::Overflowed);
            }
        };
        if is_pat {
            self.handle_pat_section(&section);
        } else {
            self.handle_pmt_section(pid, &section);
        }
        Ok(PsiStep::Completed)
    }

    pub(super) fn handle_pat_section(&mut self, section: &[u8]) {
        let parsed = match parse_pat_section(section) {
            Ok(s) => s,
            Err(PsiParseError::CrcMismatch { .. }) => {
                self.queue_nonconformant(
                    StreamId::anonymous(0x0000, 0),
                    NonConformantIssue::PsiChecksumMismatch { pid: 0x0000 },
                );
                return;
            }
            Err(_) => return,
        };
        // Validate fixed/reserved PSI syntax fields. This runs per-section
        // for multi-section PATs too (intentional).
        self.check_psi_syntax(0x0000, 0x00, section);

        // Route multi-section PATs through the reassembler so
        // the topology diff fires atomically only on a complete section set.
        // Single-section PATs (last_section_number == 0) bypass the reassembler.
        let (version, programs) = if parsed.last_section_number == 0 {
            (parsed.version, parsed.programs)
        } else {
            match self.pat_reassembler.accept(&parsed) {
                PatReassemblyOutcome::Pending => return,
                PatReassemblyOutcome::Complete(programs) => (parsed.version, programs),
                PatReassemblyOutcome::Broken => {
                    self.queue_nonconformant(
                        StreamId::anonymous(0x0000, 0),
                        NonConformantIssue::PsiMultiSectionUnsupported {
                            pid: 0x0000,
                            table_id: 0x00,
                            last_section_number: parsed.last_section_number,
                        },
                    );
                    return;
                }
            }
        };

        // Same version — nothing changed, skip the diff.
        if Some(version) == self.pat_version {
            return;
        }
        self.pat_version = Some(version);
        self.apply_pat_programs(&programs);
    }

    /// Apply the program list from a newly-complete PAT version, performing
    /// the topology diff (remove programs that disappeared, add new programs).
    pub(super) fn apply_pat_programs(&mut self, programs: &[PatEntry]) {
        // Map each PMT PID in the new PAT to its program_number (skip program 0).
        let new_pmt: HashMap<u16, u16> = programs
            .iter()
            .filter(|e| e.program_number != 0)
            .map(|e| (e.pid, e.program_number))
            .collect();

        // Drop trackers whose PMT PID disappeared from this PAT version, OR
        // whose PMT PID was reassigned to a different program_number (PID
        // reuse across programs). A reassigned PMT PID must tear down the
        // stale program so the new program's PMT is adopted cleanly instead of
        // being rejected as PmtProgramNumberMismatch against the old identity.
        // Per-PID cleanup is shared with the PMT-version path through the
        // `TopologyDiff`: a PID's state entry is otherwise unreachable and
        // would leak under PAT rotation.
        let removed: Vec<u16> = self
            .programs
            .iter()
            .filter(|(pmt_pid, tracker)| {
                // Remove if the PMT PID is gone OR now maps to a different program.
                new_pmt.get(*pmt_pid).copied() != Some(tracker.program_number)
            })
            .map(|(pmt_pid, _)| *pmt_pid)
            .collect();
        let mut diff = TopologyDiff::default();
        for pmt_pid in removed {
            if let Some(tracker) = self.programs.remove(&pmt_pid) {
                diff.retired.extend(tracker.streams.iter().map(|s| s.pid));
                diff.pcr_pid_changes.push((tracker.pcr_pid, None));
                // The per-program unwrap reference is keyed by
                // program_number, so it is unreachable once the program is
                // gone — and a later program re-using the number must not
                // anchor its first sample onto this one's epoch.
                self.timestamps.retire_program(tracker.program_number);
                // Free the PSI assembly buffer + PMT-PID continuity state.
                self.psi_assemblers.remove(&pmt_pid);
                diff.retired_pmt_pids.push(pmt_pid);
            }
        }
        // The removed programs' elementary PIDs lose all state and their PCR
        // PIDs lose their history, unless a surviving program declares the
        // same PID.
        self.apply_topology_diff(diff);

        // Add empty trackers for programs that are new in this PAT version.
        // PMT contents will populate them when handle_pmt_section fires.
        for entry in programs {
            if entry.program_number == 0 {
                continue; // program 0 = Network PID, not a real program
            }
            self.programs
                .entry(entry.pid)
                .or_insert_with(|| ProgramTracker {
                    program_number: entry.program_number,
                    pmt_version: None,
                    pcr_pid: None,
                    streams: Vec::new(),
                    klv_mismatch_coalesce: HashSet::new(),
                });
            // Seed the PSI assembler for this PMT PID so handle_psi can
            // accumulate bytes without a separate "first packet" init step.
            self.psi_assemblers.entry(entry.pid).or_default();
        }
    }

    /// Retire the PCR history of every PID that no program declares as its
    /// `PCR_PID` any more. Called after every change to the program set or to
    /// a program's `PCR_PID`.
    ///
    /// The history belongs to the declaration, not to the PID: it is kept
    /// while at least one program names the PID as its clock (two programs
    /// may share one PCR PID, and one of them moving away must not erase the
    /// other's baseline), and dropped once none does, whatever else the PID
    /// still carries. A PID that is declared again later then starts from a
    /// seed instead of being judged against an earlier declaration's
    /// baseline.
    pub(super) fn retire_undeclared_pcr_history(&mut self) {
        let programs = &self.programs;
        self.pcr
            .retain_declared(|pid| programs.values().any(|t| t.pcr_pid == Some(pid)));
    }

    /// What a freshly accepted PMT version changes in per-PID state, as a
    /// diff (see [`TopologyDiff`]). Reads the tracker's PRIOR stream list and
    /// `PCR_PID`; mutates nothing.
    ///
    /// A moved `PCR_PID` is recorded in `pcr_pid_changes`; an unchanged one
    /// keeps its baseline across the version bump.
    fn pmt_topology_diff(
        &self,
        pmt_pid: u16,
        new_pcr_pid: u16,
        program_number: u16,
        stream_infos: &[StreamInfo],
    ) -> TopologyDiff {
        let mut diff = TopologyDiff::default();
        if let Some(tracker) = self.programs.get(&pmt_pid) {
            for old in &tracker.streams {
                match stream_infos.iter().find(|s| s.pid == old.pid) {
                    None => diff.retired.push(old.pid),
                    Some(new) if new.kind != old.kind => diff.kind_changed.push(old.pid),
                    Some(_) => {}
                }
            }
            if tracker.pcr_pid != Some(new_pcr_pid) {
                diff.pcr_pid_changes
                    .push((tracker.pcr_pid, Some(new_pcr_pid)));
            }
        }
        diff.added = stream_infos
            .iter()
            .map(|s| (s.pid, s.kind, program_number))
            .collect();
        diff
    }

    pub(super) fn handle_pmt_section(&mut self, pmt_pid: u16, section: &[u8]) {
        // The PMT PID itself is owned by a program (the one this PMT describes).
        // PAT pre-populates `self.programs` keyed by PMT PID, so we can resolve
        // the program_number here even when the PMT body fails to parse.
        let pmt_program_number = self
            .programs
            .get(&pmt_pid)
            .map(|t| t.program_number)
            .unwrap_or(0);
        let pmt = match parse_pmt(section) {
            Ok(p) => p,
            Err(PsiParseError::CrcMismatch { .. }) => {
                self.queue_nonconformant(
                    StreamId::anonymous(pmt_pid, pmt_program_number),
                    NonConformantIssue::PsiChecksumMismatch { pid: pmt_pid },
                );
                return;
            }
            Err(PsiParseError::MultiSectionUnsupported {
                table_id,
                last_section_number,
            }) => {
                self.queue_nonconformant(
                    StreamId::anonymous(pmt_pid, pmt_program_number),
                    NonConformantIssue::PsiMultiSectionUnsupported {
                        pid: pmt_pid,
                        table_id,
                        last_section_number,
                    },
                );
                return;
            }
            Err(_) => return,
        };

        // Look up the tracker — must exist if PAT pre-populated us.
        let program_number = match self.programs.get(&pmt_pid) {
            Some(t) => t.program_number,
            None => return, // PMT arriving on PID not in PAT — drop.
        };

        // Reject PMT whose body program_number doesn't match PAT.
        // A validly-checksummed PMT for program B arriving on program A's PMT
        // PID is a structural violation per H.222.0 §2.4.4.8. Do NOT adopt
        // topology; emit a NonConformant diagnostic.
        if pmt.program_number != program_number {
            self.queue_nonconformant(
                StreamId::anonymous(pmt_pid, program_number),
                NonConformantIssue::PmtProgramNumberMismatch {
                    pid: pmt_pid,
                    pat_program: program_number,
                    pmt_program: pmt.program_number,
                },
            );
            return;
        }

        // Validate fixed/reserved PSI syntax fields.
        self.check_psi_syntax(pmt_pid, 0x02, section);
        // Dedup: re-emit only if version changed or first ever.
        if let Some(tracker) = self.programs.get(&pmt_pid)
            && Some(pmt.version) == tracker.pmt_version
        {
            return;
        }

        // Fresh PMT version — clear the per-PID guard for SubtitleMissingDescriptor
        // and Av1RegistrationMalformed emission so that a new PMT version
        // re-fires the warning if the descriptor is still absent / still
        // malformed. Only PIDs owned by *this* PMT are dropped to leave any
        // other program's state intact.
        for s in &pmt.streams {
            if let Some(state) = self.pid_state.get_mut(&s.elementary_pid) {
                state.subtitle_missing_descriptor_emitted = false;
                state.av1_registration_malformed_emitted = false;
                state.subtitle_descriptor_ambiguous_emitted = false;
            }
        }
        // Drop any in-flight AU cell reassembly buffers wholesale on PMT
        // version change. A PMT bump may have reassigned PIDs (sync ↔ async
        // metadata, KLV ↔ video) and stale per-PID state would be
        // ambiguous to interpret. Clearing all PIDs (not just the ones in
        // this PMT) avoids any chance of cross-program leakage; the cost is
        // dropping at most one partially-reassembled AU per active sync
        // metadata PID — same shape as a reset_sync.
        self.au_reassembler.reset_all();

        // Build StreamInfo list + check cross-program PID collisions.
        // Collect work to do before mutating self — satisfies borrow checker.
        let mut stream_infos: Vec<StreamInfo> = Vec::with_capacity(pmt.streams.len());
        let mut kind_inserts: Vec<(u16, StreamKind)> = Vec::new();
        let mut collision_issues: Vec<(StreamId, NonConformantIssue)> = Vec::new();
        let mut subtitle_missing: Vec<(u16, StreamKind)> = Vec::new();
        let mut av1_malformed: Vec<(u16, StreamKind)> = Vec::new();
        let mut subtitle_ambiguous: Vec<(u16, StreamKind, Vec<u8>)> = Vec::new();

        for s in &pmt.streams {
            let kind = self.get_stream_kind(s.elementary_pid, s);

            // Cross-program PID collision check: scan all other programs'
            // stream lists. First-program-wins — skip this PID if already owned.
            let other_prog = self
                .programs
                .iter()
                .find(|(other_pmt_pid, t)| {
                    **other_pmt_pid != pmt_pid
                        && t.streams.iter().any(|st| st.pid == s.elementary_pid)
                })
                .map(|(_, t)| t.program_number);

            if let Some(other_program_number) = other_prog {
                collision_issues.push((
                    // `kind: Unknown(0)`: the PID is first-program-wins owned
                    // by `other_program_number`; this StreamId surfaces the
                    // collision attempted by *this* PMT (`program_number`).
                    StreamId::anonymous(s.elementary_pid, program_number),
                    NonConformantIssue::PidReusedAcrossPrograms {
                        pid: s.elementary_pid,
                        programs: [other_program_number, program_number],
                    },
                ));
                continue; // Skip this stream — first-program-wins.
            }

            // Subtitle-resolved PIDs without a recognized subtitle descriptor
            // (subtitling/teletext/VTTC/GA94) are non-conformant — most often
            // because a `treat_as` override forced StreamKind::Subtitle on a
            // PID whose PMT entry doesn't carry the corresponding tag.
            if matches!(kind, StreamKind::Subtitle(_))
                && !super::pmt_classify::has_recognized_subtitle_descriptor(&s.descriptors)
            {
                subtitle_missing.push((s.elementary_pid, kind));
            }

            // stream_type 0x06 entries that contain a Registration descriptor
            // whose body looks like a truncated AV01 attempt (starts with
            // "AV", < 4 bytes total) — fires only when classify_0x06 didn't
            // already return Video(Av1), i.e. the malformed registration
            // didn't match `b"AV01"` exactly. Outer length-vs-buffer overflow
            // is already caught by walk_descriptors as DescriptorLoopOverflow;
            // this is the in-bounds variant.
            if s.stream_type == 0x06
                && !matches!(kind, StreamKind::Video(VideoCodec::Av1))
                && super::pmt_classify::is_malformed_av1_registration(&s.descriptors)
            {
                av1_malformed.push((s.elementary_pid, kind));
            }

            // stream_type 0x06 entries with more than one recognized
            // subtitle/KLV codec marker — classification cascade still
            // picks the highest-priority match (subtitling > teletext >
            // VTTC > GA94 > KLVA), but the ambiguity is surfaced for
            // diagnostics. Only checked on 0x06 since the other stream
            // types disambiguate by stream_type alone. AV1 wins
            // exclusively (binding §2.1) so AV01 alongside other markers
            // is not flagged here — classify_0x06 returned Video(Av1).
            if s.stream_type == 0x06 && !matches!(kind, StreamKind::Video(VideoCodec::Av1)) {
                let (_, ambiguous_tags) =
                    super::pmt_classify::classify_0x06_with_ambiguity(&s.descriptors);
                if !ambiguous_tags.is_empty() {
                    subtitle_ambiguous.push((s.elementary_pid, kind, ambiguous_tags));
                }
            }

            stream_infos.push(StreamInfo {
                pid: s.elementary_pid,
                stream_type: StreamTypeCode::from_byte(s.stream_type),
                kind,
                program_number,
                raw_descriptors: s.descriptors.clone(),
            });
            kind_inserts.push((s.elementary_pid, kind));
        }

        // Emit collision NonConformant events.
        for (stream_id, issue) in collision_issues {
            self.queue_nonconformant(stream_id, issue);
        }

        // Emit SubtitleMissingDescriptor once per PID per fresh PMT version.
        for (pid, kind) in subtitle_missing {
            if set_once(&mut self.pid_state_mut(pid).subtitle_missing_descriptor_emitted) {
                self.queue_nonconformant(
                    StreamId {
                        pid,
                        kind,
                        program_number,
                    },
                    NonConformantIssue::SubtitleMissingDescriptor { pid },
                );
            }
        }

        // Emit Av1RegistrationMalformed once per PID per fresh PMT version.
        for (pid, kind) in av1_malformed {
            if set_once(&mut self.pid_state_mut(pid).av1_registration_malformed_emitted) {
                self.queue_nonconformant(
                    StreamId {
                        pid,
                        kind,
                        program_number,
                    },
                    NonConformantIssue::Av1RegistrationMalformed { pid },
                );
            }
        }

        // Emit SubtitleDescriptorAmbiguous once per PID per fresh PMT version.
        for (pid, kind, tags) in subtitle_ambiguous {
            if set_once(
                &mut self
                    .pid_state_mut(pid)
                    .subtitle_descriptor_ambiguous_emitted,
            ) {
                self.queue_nonconformant(
                    StreamId {
                        pid,
                        kind,
                        program_number,
                    },
                    NonConformantIssue::SubtitleDescriptorAmbiguous { pid, tags },
                );
            }
        }

        // A PMT version bump may drop or reassign elementary PIDs. Work out
        // what changed and apply it in one place: a PID the prior version
        // owned that is gone from the new stream set loses ALL its per-PID
        // state (the same cleanup PAT program removal performs), so it can no
        // longer route stale PES, timing, or stats; a PID that persists under
        // a different kind loses its PES buffer and timing baselines (a
        // partial video PES buffered before the bump must not be emitted
        // under the new kind, and stale CC / PTS baselines would produce false
        // anomaly events on the first post-bump packet); kinds and owners are
        // set for every accepted (new and persisting) stream.
        let diff = self.pmt_topology_diff(pmt_pid, pmt.pcr_pid, program_number, &stream_infos);

        // Build klv_links from the accepted streams.
        let prog_map = self.build_program_map(pmt_pid, &pmt, program_number, &stream_infos);

        // Update tracker.
        let tracker = self.programs.get_mut(&pmt_pid).expect("checked above");
        tracker.pmt_version = Some(pmt.version);
        tracker.pcr_pid = Some(pmt.pcr_pid);
        tracker.streams = stream_infos;
        tracker.klv_mismatch_coalesce.clear();

        // Applied after the tracker update: a moved `PCR_PID` retires the old
        // PID's history only if no program still declares it.
        self.apply_topology_diff(diff);

        // Emit ProgramMap event.
        self.queue.push_back(DemuxEvent::ProgramMap(prog_map));
        self.program_maps_seen += 1;
        self.pmt_versions_seen += 1;
    }

    /// Build a `ProgramMap` event payload from a parsed PMT and the accepted
    /// stream list (after cross-program collision filtering).
    pub(super) fn build_program_map(
        &mut self,
        pmt_pid: u16,
        pmt: &Pmt,
        program_number: u16,
        streams: &[StreamInfo],
    ) -> ProgramMap {
        let mut klv_pids: Vec<(u16, Option<u16>)> = Vec::new();
        let mut video_pids: Vec<u16> = Vec::new();
        for info in streams {
            if let StreamKind::Video(_) = info.kind {
                video_pids.push(info.pid);
            }
            if matches!(info.kind, StreamKind::KlvSync { .. } | StreamKind::KlvAsync) {
                let declared_link =
                    super::pmt_classify::extract_metadata_link_for_pid(pmt, info.pid);
                klv_pids.push((info.pid, declared_link));
            }
        }
        // Build klv_links table.
        let mut klv_links = Vec::new();
        for (klv_pid, declared) in klv_pids {
            // 1. Caller override wins.
            if let Some(&(_, video_pid)) = self
                .options
                .klv_link_overrides
                .iter()
                .find(|&&(k, _)| k == klv_pid)
            {
                klv_links.push(KlvLink {
                    klv_pid,
                    video_pid,
                    source: LinkSource::Override,
                });
                continue;
            }
            // 2. Declared via metadata_descriptor.
            if let Some(video_pid) = declared {
                klv_links.push(KlvLink {
                    klv_pid,
                    video_pid,
                    source: LinkSource::Declared,
                });
                continue;
            }
            // 3. Inferred — exactly one video PID in this PMT.
            if video_pids.len() == 1 {
                klv_links.push(KlvLink {
                    klv_pid,
                    video_pid: video_pids[0],
                    source: LinkSource::Inferred,
                });
            }
            // 4. Otherwise: no entry. Surface MissingMetadataDescriptor as
            // non-conformant.
            else {
                let stream = StreamId {
                    pid: klv_pid,
                    kind: StreamKind::KlvSync {
                        declared_link: None,
                    },
                    program_number,
                };
                self.queue_nonconformant(stream, NonConformantIssue::MissingMetadataDescriptor);
            }
        }
        ProgramMap {
            program_number,
            pcr_pid: pmt.pcr_pid,
            pmt_pid,
            streams: streams.to_vec(),
            klv_links,
        }
    }

    /// Validate fixed/reserved PSI syntax fields on a section
    /// whose CRC already passed. `section_syntax_indicator`/`section_number`
    /// are checked in every mode; reserved-bit checks are gated behind
    /// `strict != Off`. `pid` is the PID the section arrived on (0x0000 for
    /// PAT), `table_id` is 0x00 (PAT) or 0x02 (PMT).
    fn check_psi_syntax(&mut self, pid: u16, table_id: u8, section: &[u8]) {
        let stream = || StreamId::anonymous(pid, 0);
        // Always: section_syntax_indicator (byte 1 bit 0x80) must be 1.
        if section[1] & 0x80 == 0 {
            self.queue_nonconformant(
                stream(),
                NonConformantIssue::PsiSyntax {
                    pid,
                    table_id,
                    kind: PsiSyntaxKind::SectionSyntaxIndicatorUnset,
                },
            );
        }
        // section_number (byte 6) must be 0 only on a single-section table
        // (last_section_number, byte 7, == 0). Multi-section PATs
        // legitimately carry section_number 1..=last_section_number, so a
        // non-zero section_number there is valid, not a syntax violation.
        if section[7] == 0 && section[6] != 0 {
            self.queue_nonconformant(
                stream(),
                NonConformantIssue::PsiSyntax {
                    pid,
                    table_id,
                    kind: PsiSyntaxKind::SectionNumberNonZero {
                        observed: section[6],
                    },
                },
            );
        }
        // Gated: reserved bits (false-positives common in lenient mode).
        if self.options.strict != StrictMode::Off {
            // byte 1 bit 0x40 = reserved-zero (must be 0);
            // byte 1 bits 0x30 = reserved (must be 1);
            // byte 5 bits 0xC0 = reserved (must be 1).
            let mut bad = (section[1] & 0x40) != 0
                || (section[1] & 0x30) != 0x30
                || (section[5] & 0xC0) != 0xC0;
            if table_id == 0x02 {
                // PMT additionally: byte 8 bits 0xE0 = reserved (must be 1);
                // byte 10 bits 0x30 = reserved (must be 1).
                bad = bad || (section[8] & 0xE0) != 0xE0 || (section[10] & 0x30) != 0x30;
            }
            if bad {
                self.queue_nonconformant(
                    stream(),
                    NonConformantIssue::PsiSyntax {
                        pid,
                        table_id,
                        kind: PsiSyntaxKind::ReservedBits,
                    },
                );
            }
        }
    }

    pub(super) fn klv_mismatch_insert(&mut self, pid: u16) -> bool {
        // Find the tracker that owns this PID via its streams list.
        for tracker in self.programs.values_mut() {
            if tracker.streams.iter().any(|s| s.pid == pid) {
                return tracker.klv_mismatch_coalesce.insert(pid);
            }
        }
        // No tracker found — no suppression.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::demuxer::Demuxer;
    use super::super::event::{DemuxEvent, NonConformantIssue, PsiSyntaxKind, StreamKind};
    use super::super::strict::StrictMode;
    use super::super::types::DemuxerConfig;
    use crate::mpegts::common::Pts90khz;
    use crate::mpegts::common::crc32::crc32_mpeg2;
    use crate::mpegts::demux::test_support::{
        drain_all_events, pat_packet_with_multi_section, pat_packet_with_multi_section_cc,
        pat_packet_with_programs, pat_packet_with_programs_cc, payload_packet_for_test,
        pmt_packet_for_test, pmt_packet_for_test_cc, pmt_packet_with_multi_section,
    };
    use alloc::vec::Vec;

    use crate::mpegts::demux::test_support::{
        build_pat_section, build_pmt_section as build_pmt_section_with,
    };

    /// `streams` is `(stream_type, elementary_pid)`, no descriptors.
    fn build_pmt_section(
        program_number: u16,
        pcr_pid: u16,
        version: u8,
        streams: &[(u8, u16)],
    ) -> Vec<u8> {
        let es: Vec<(u8, u16, &[u8])> = streams.iter().map(|&(t, p)| (t, p, &[][..])).collect();
        build_pmt_section_with(program_number, pcr_pid, version, &[], &es)
    }

    /// Recompute and overwrite the CRC trailer in a section byte vector.
    /// The CRC covers section[..len-4]; the last 4 bytes are replaced with
    /// the new CRC in big-endian order.
    fn fix_crc(section: &mut [u8]) {
        let n = section.len();
        assert!(n >= 4, "section too short for CRC trailer");
        let crc = crc32_mpeg2(&section[..n - 4]);
        section[n - 4] = (crc >> 24) as u8;
        section[n - 3] = (crc >> 16) as u8;
        section[n - 2] = (crc >> 8) as u8;
        section[n - 1] = crc as u8;
    }

    /// Wrap a PSI section into a 188-byte TS packet with PUSI set, payload-only.
    fn wrap_section_in_ts_packet(pid: u16, section: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47; // sync byte
        pkt[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID hi
        pkt[2] = (pid & 0xFF) as u8; // PID lo
        pkt[3] = 0x10; // payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        let sec_end = 5 + section.len();
        assert!(sec_end <= 188, "section too large for one TS packet");
        pkt[5..sec_end].copy_from_slice(section);
        pkt
    }

    /// Like `wrap_section_in_ts_packet` but sets the 4-bit CC field to `cc`.
    ///
    /// Use this in tests that feed two PSI packets on the same PID: the second
    /// must use `cc=1` so that the demuxer's duplicate-suppression does not
    /// swallow it.
    fn wrap_section_in_ts_packet_cc(pid: u16, section: &[u8], cc: u8) -> Vec<u8> {
        let mut pkt = wrap_section_in_ts_packet(pid, section);
        pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
        pkt
    }

    /// Drain all queued events from the demuxer.
    fn drain_events(d: &mut Demuxer) -> Vec<DemuxEvent> {
        let mut events = Vec::new();
        while let Some(e) = d.next_event() {
            events.push(e);
        }
        events
    }

    // section_syntax_indicator=0 flagged on PAT
    #[test]
    fn pat_section_syntax_indicator_unset_flagged() {
        let mut section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        // Clear section_syntax_indicator (bit 0x80 of byte 1)
        section[1] &= !0x80;
        fix_crc(&mut section);

        let pkt = wrap_section_in_ts_packet(0x0000, &section);
        let mut demux = Demuxer::new();
        demux.feed(&pkt).unwrap();
        let events = drain_events(&mut demux);

        let nc = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant { issue, .. } => Some(issue.clone()),
            _ => None,
        });
        match nc {
            Some(NonConformantIssue::PsiSyntax {
                pid,
                table_id,
                kind: PsiSyntaxKind::SectionSyntaxIndicatorUnset,
            }) => {
                assert_eq!(pid, 0x0000);
                assert_eq!(table_id, 0x00);
            }
            other => panic!("expected PsiSyntax SectionSyntaxIndicatorUnset on PAT, got {other:?}"),
        }
    }

    // section_number != 0 flagged on PAT
    #[test]
    fn pat_section_number_nonzero_flagged() {
        let mut section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        // Set section_number (byte 6) to non-zero — but last_section_number stays 0
        // so parse_pat accepts this (it only checks last_section_number for multi-section)
        section[6] = 0x02;
        fix_crc(&mut section);

        let pkt = wrap_section_in_ts_packet(0x0000, &section);
        let mut demux = Demuxer::new();
        demux.feed(&pkt).unwrap();
        let events = drain_events(&mut demux);

        let nc = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant { issue, .. } => Some(issue.clone()),
            _ => None,
        });
        match nc {
            Some(NonConformantIssue::PsiSyntax {
                pid,
                table_id,
                kind: PsiSyntaxKind::SectionNumberNonZero { observed },
            }) => {
                assert_eq!(pid, 0x0000);
                assert_eq!(table_id, 0x00);
                assert_eq!(observed, 0x02);
            }
            other => panic!("expected PsiSyntax SectionNumberNonZero on PAT, got {other:?}"),
        }
    }

    // Reserved-bit violation NOT emitted under StrictMode::Off
    #[test]
    fn pat_reserved_bits_not_flagged_in_lenient_mode() {
        let mut section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        // Corrupt a reserved-zero bit (byte 1 bit 0x40 should be 0; set it)
        section[1] |= 0x40;
        fix_crc(&mut section);

        let pkt = wrap_section_in_ts_packet(0x0000, &section);
        let mut demux = Demuxer::new(); // default = StrictMode::Off
        demux.feed(&pkt).unwrap();
        let events = drain_events(&mut demux);

        let psi_syntax_nc = events.iter().any(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiSyntax {
                        kind: PsiSyntaxKind::ReservedBits,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(
            !psi_syntax_nc,
            "StrictMode::Off must NOT surface reserved-bit violations (false-positive guard)"
        );
    }

    // Reserved-bit violation emitted and rejected under StrictMode::Full
    #[test]
    fn pat_reserved_bits_flagged_and_rejected_in_full_mode() {
        let mut section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        // Corrupt a reserved-zero bit (byte 1 bit 0x40 should be 0; set it)
        section[1] |= 0x40;
        fix_crc(&mut section);

        let pkt = wrap_section_in_ts_packet(0x0000, &section);
        let config = DemuxerConfig {
            strict: StrictMode::Full,
            ..DemuxerConfig::default()
        };
        let mut demux = Demuxer::with_config(config);
        // StrictMode::Full converts NonConformant to DemuxError::StrictRejection
        let result = demux.feed(&pkt);
        assert!(
            result.is_err(),
            "StrictMode::Full must reject reserved-bit PSI violation"
        );
        // Prove the rejection was caused by the reserved-bit PsiSyntax issue
        // specifically — `queue_nonconformant` enqueues the event before
        // setting the fatal flag, so it remains drainable on a Full rejection.
        let events = drain_events(&mut demux);
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiSyntax {
                        kind: PsiSyntaxKind::ReservedBits,
                        ..
                    },
                    ..
                }
            )),
            "expected a PsiSyntax{{ReservedBits}} event before the strict rejection; got {events:?}"
        );
    }

    // section_syntax_indicator=0 flagged on PMT
    #[test]
    fn pmt_section_syntax_indicator_unset_flagged() {
        // First feed a valid PAT so the demuxer creates a tracker for the PMT PID
        let pat_section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat_section);

        // Build a PMT with ssi=0
        let mut pmt_section = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        pmt_section[1] &= !0x80; // clear section_syntax_indicator
        fix_crc(&mut pmt_section);
        let pmt_pkt = wrap_section_in_ts_packet(0x1000, &pmt_section);

        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux.feed(&pmt_pkt).unwrap();
        let events = drain_events(&mut demux);

        let nc = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PsiSyntax {
                        pid,
                        table_id,
                        kind,
                    },
                ..
            } => {
                if *table_id == 0x02 {
                    Some((*pid, *table_id, *kind))
                } else {
                    None
                }
            }
            _ => None,
        });
        match nc {
            Some((pid, table_id, PsiSyntaxKind::SectionSyntaxIndicatorUnset)) => {
                assert_eq!(pid, 0x1000);
                assert_eq!(table_id, 0x02);
            }
            other => panic!("expected PsiSyntax SectionSyntaxIndicatorUnset on PMT, got {other:?}"),
        }
    }

    // PMT-specific reserved-bit violation gated by StrictMode
    // (NOT emitted under Off; emitted AND rejected under Full).
    #[test]
    fn pmt_reserved_bits_flagged_and_rejected_in_full_mode() {
        let pat_section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat_section);

        // Corrupt a PMT-specific reserved bit: byte 8 high field (0xE0) must be
        // all-ones; clear its top bit (0x80) — leaves the low 5 pcr_pid bits
        // intact, so only a reserved bit is wrong.
        let mut pmt_section = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        pmt_section[8] &= !0x80;
        fix_crc(&mut pmt_section);
        let pmt_pkt = wrap_section_in_ts_packet(0x1000, &pmt_section);

        // Off mode: no PsiSyntax event (reserved-bit checks are gated).
        let mut lenient = Demuxer::new();
        lenient.feed(&pat_pkt).unwrap();
        lenient.feed(&pmt_pkt).unwrap();
        let lenient_events = drain_events(&mut lenient);
        assert!(
            !lenient_events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiSyntax {
                        kind: PsiSyntaxKind::ReservedBits,
                        ..
                    },
                    ..
                }
            )),
            "StrictMode::Off must NOT surface PMT reserved-bit violations; got {lenient_events:?}"
        );

        // Full mode: reject AND surface PsiSyntax{ReservedBits} on the PMT PID.
        let config = DemuxerConfig {
            strict: StrictMode::Full,
            ..DemuxerConfig::default()
        };
        let mut strict = Demuxer::with_config(config);
        strict.feed(&pat_pkt).unwrap();
        let result = strict.feed(&pmt_pkt);
        assert!(
            result.is_err(),
            "StrictMode::Full must reject PMT reserved-bit violation"
        );
        let events = drain_events(&mut strict);
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiSyntax {
                        table_id: 0x02,
                        kind: PsiSyntaxKind::ReservedBits,
                        ..
                    },
                    ..
                }
            )),
            "expected a PMT PsiSyntax{{ReservedBits}} event before the strict rejection; got {events:?}"
        );
    }

    // section_number != 0 flagged on PMT (fires in all modes).
    #[test]
    fn pmt_section_number_non_zero_flagged() {
        let pat_section = build_pat_section(0x0001, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat_section);

        // Set PMT section_number (byte 6) non-zero; last_section_number stays 0
        // so parse_pmt accepts it.
        let mut pmt_section = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        pmt_section[6] = 0x03;
        fix_crc(&mut pmt_section);
        let pmt_pkt = wrap_section_in_ts_packet(0x1000, &pmt_section);

        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux.feed(&pmt_pkt).unwrap();
        let events = drain_events(&mut demux);

        let nc = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PsiSyntax {
                        pid,
                        table_id,
                        kind,
                    },
                ..
            } if *table_id == 0x02 => Some((*pid, *table_id, *kind)),
            _ => None,
        });
        match nc {
            Some((pid, table_id, PsiSyntaxKind::SectionNumberNonZero { observed })) => {
                assert_eq!(pid, 0x1000);
                assert_eq!(table_id, 0x02);
                assert_eq!(observed, 0x03);
            }
            other => panic!("expected PsiSyntax SectionNumberNonZero on PMT, got {other:?}"),
        }
    }

    // PMT PCR-PID change must remove the stale last_pcr_by_pid entry
    // when the old PCR PID was a PCR-only PID not present in the new stream set.
    #[test]
    fn pmt_pcr_pid_change_removes_stale_pcr_entry() {
        let pat = build_pat_section(1, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat);

        // First PMT: video on 0x0101, PCR on 0x0100 (PCR-only PID).
        let pmt_v0 = build_pmt_section(1, 0x0100, 0, &[(0x1B, 0x0101)]);
        let pmt_v0_pkt = wrap_section_in_ts_packet(0x1000, &pmt_v0);

        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux.feed(&pmt_v0_pkt).unwrap();
        // Seed last_pcr_by_pid for PID 0x0100 by calling check_pcr via
        // process_packet. Build a minimal TS packet with a PCR on PID 0x0100.
        let mut pcr_pkt = [0xFFu8; 188];
        pcr_pkt[0] = 0x47;
        pcr_pkt[1] = 0x01; // PID hi = 0x0100
        pcr_pkt[2] = 0x00; // PID lo
        // afc=10, CC=0, AF with PCR flag
        pcr_pkt[3] = 0b10 << 4; // afc=10, CC=0
        pcr_pkt[4] = 183; // af_length
        pcr_pkt[5] = 0x10; // PCR_flag=1
        // PCR value = 0 (bytes 6..12 already 0xFF → set to 0x00 with reserved bits)
        for b in pcr_pkt[6..12].iter_mut() {
            *b = 0x00;
        }
        pcr_pkt[10] = 0x7E; // reserved bits per §2.4.3.5
        demux.feed(&pcr_pkt).unwrap();
        assert!(
            demux.pcr.contains(0x0100),
            "PCR PID 0x0100 should be in last_pcr_by_pid after first PMT"
        );

        // Second PMT (version bump): PCR PID moves to 0x0101 (the video PID).
        // Old PCR PID 0x0100 is no longer in the stream set.
        // CC=1 so this isn't swallowed as a duplicate of the v0 PMT on this PID.
        let pmt_v1 = build_pmt_section(1, 0x0101, 1, &[(0x1B, 0x0101)]);
        let pmt_v1_pkt = wrap_section_in_ts_packet_cc(0x1000, &pmt_v1, 1);
        demux.feed(&pmt_v1_pkt).unwrap();

        assert!(
            !demux.pcr.contains(0x0100),
            "stale last_pcr_by_pid entry for old PCR-only PID 0x0100 must be removed on PCR-PID change"
        );
    }

    // A PID that persists across a PMT version bump but changes
    // stream kind must have its PES / CC / PTS state flushed so stale reassembly
    // state from the old kind can't pollute events under the new kind.
    #[test]
    fn pmt_persisting_pid_with_kind_change_flushes_pes_cc_pts_state() {
        use super::super::demuxer::Demuxer;

        let pat = build_pat_section(1, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat);

        // First PMT v0: PID 0x0101 as video (stream_type 0x1B).
        let pmt_v0 = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        let pmt_v0_pkt = wrap_section_in_ts_packet(0x1000, &pmt_v0);

        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux.feed(&pmt_v0_pkt).unwrap();

        // Feed a PES start on PID 0x0101 (video) so the reassembler has
        // in-flight state and cc_by_pid / last_pts_by_pid are populated.
        // TS payload-only packet with PUSI, PES header for video.
        let mut pes_pkt = [0xFFu8; 188];
        pes_pkt[0] = 0x47;
        pes_pkt[1] = 0x40 | 0x01; // PUSI + PID hi
        pes_pkt[2] = 0x01; // PID lo → PID = 0x0101
        pes_pkt[3] = 0x10; // payload-only, CC=0
        // pointer_field = 0 (required for PSI, but handled as PES by demuxer
        // since this PID is now a stream PID). PES start bytes follow.
        // PES: 0x000001 start code + stream_id=0xE0 + length=0 + optional hdr
        pes_pkt[4] = 0x00;
        pes_pkt[5] = 0x00;
        pes_pkt[6] = 0x01;
        pes_pkt[7] = 0xE0; // stream_id video
        pes_pkt[8] = 0x00; // PES_packet_length hi (unbounded)
        pes_pkt[9] = 0x00; // PES_packet_length lo
        pes_pkt[10] = 0x80; // flags1: marker=10
        pes_pkt[11] = 0x80; // PTS_DTS_flags=10 (PTS only)
        pes_pkt[12] = 0x05; // header_data_length = 5 (PTS)
        // PTS = 900_000 ticks (3 × 33-bit PTS encoding)
        let pts: u64 = 900_000;
        pes_pkt[13] = 0x21 | (((pts >> 30) as u8) & 0x0E);
        pes_pkt[14] = ((pts >> 22) & 0xFF) as u8;
        pes_pkt[15] = (((pts >> 14) & 0xFE) as u8) | 0x01;
        pes_pkt[16] = ((pts >> 7) & 0xFF) as u8;
        pes_pkt[17] = (((pts << 1) & 0xFE) as u8) | 0x01;
        // payload bytes (don't matter for this test)
        demux.feed(&pes_pkt).unwrap();

        // Verify state is populated before the PMT bump.
        assert!(
            demux.pid_state.get(&0x0101).is_some_and(|s| s.cc.is_some()),
            "the PID should have a cc entry for PID 0x0101 after PES packet"
        );
        // Note: last_pts_by_pid is only populated on a *completed* PES —
        // the unbounded video PES above isn't complete yet. The PES reassembler
        // state (partial buffer) is the primary thing to check for flush.
        assert!(
            demux.pes.buffered_bytes() > 0,
            "PES reassembler should have buffered bytes from the in-flight PES"
        );

        // Second PMT v1: same PID 0x0101 but now audio (stream_type 0x03).
        // CC=1 so this isn't swallowed as a duplicate of the v0 PMT on this PID.
        let pmt_v1 = build_pmt_section(1, 0x0101, 1, &[(0x03, 0x0101)]);
        let pmt_v1_pkt = wrap_section_in_ts_packet_cc(0x1000, &pmt_v1, 1);
        demux.feed(&pmt_v1_pkt).unwrap();

        // After the PMT bump, PES/CC/PTS state for PID 0x0101 must be flushed.
        assert_eq!(
            demux.pes.buffered_bytes(),
            0,
            "in-flight PES must be dropped when PID kind changes (video→audio)"
        );
        assert!(
            demux.pid_state.get(&0x0101).is_none_or(|s| s.cc.is_none()),
            "cc entry for PID 0x0101 must be removed on kind change"
        );
    }

    // The opt-in PTS/DTS unwrap accumulator is per-PID state and must
    // follow the same topology cleanup as `last_pts_by_pid` / `cc_by_pid`. A
    // PID whose stream left the PAT/PMT can be reused by an unrelated
    // replacement stream; inheriting the dead stream's epoch would splice two
    // timelines together.
    #[test]
    fn retiring_a_pid_clears_unwrap_state() {
        let cfg = DemuxerConfig::builder().unwrap_timestamps(true).build();
        let mut demux = Demuxer::with_config(cfg);

        // Drive PID 0x0100 across the 33-bit boundary so it holds a
        // non-trivial accumulator entry (one full epoch above its raw value).
        let _ = demux.unwrap_pts(0x0100, Pts90khz::new((1i64 << 33) - 100));
        let _ = demux.unwrap_pts(0x0100, Pts90khz::new(100));
        assert!(
            demux.unwrap_of(0x0100).is_some(),
            "test setup: PID 0x0100 should hold an unwrap accumulator entry"
        );

        demux.apply_topology_diff(super::super::pid_state::TopologyDiff {
            retired: vec![0x0100],
            ..Default::default()
        });

        assert!(
            demux.unwrap_of(0x0100).is_none(),
            "the unwrap accumulator for a PID dropped from the topology must be \
             removed alongside its other per-PID state"
        );
    }

    // A PID that persists across a PMT version bump but changes stream
    // kind carries a different stream on the same PID — its unwrap accumulator
    // must be flushed with the rest of its per-PID state.
    #[test]
    fn pmt_persisting_pid_with_kind_change_clears_unwrap_state() {
        let pat = build_pat_section(1, 0, &[(1, 0x1000)]);
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &pat);

        // First PMT v0: PID 0x0101 as video (stream_type 0x1B).
        let pmt_v0 = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        let pmt_v0_pkt = wrap_section_in_ts_packet(0x1000, &pmt_v0);

        let cfg = DemuxerConfig::builder().unwrap_timestamps(true).build();
        let mut demux = Demuxer::with_config(cfg);
        demux.feed(&pat_pkt).unwrap();
        demux.feed(&pmt_v0_pkt).unwrap();

        // Seed the accumulator directly — driving it through a completed PES is
        // the wire test's job (`tests/mpegts/pts_unwrap.rs`); only the
        // topology-cleanup edge is under test here.
        let _ = demux.unwrap_pts(0x0101, Pts90khz::new(900_000));
        assert!(
            demux.unwrap_of(0x0101).is_some(),
            "test setup: PID 0x0101 should hold an unwrap accumulator entry"
        );

        // Second PMT v1: same PID 0x0101 but now audio (stream_type 0x03).
        // CC=1 so this isn't swallowed as a duplicate of the v0 PMT on this PID.
        let pmt_v1 = build_pmt_section(1, 0x0101, 1, &[(0x03, 0x0101)]);
        let pmt_v1_pkt = wrap_section_in_ts_packet_cc(0x1000, &pmt_v1, 1);
        demux.feed(&pmt_v1_pkt).unwrap();

        assert!(
            demux.unwrap_of(0x0101).is_none(),
            "the unwrap accumulator for PID 0x0101 must be flushed on kind change"
        );
    }

    // The per-PROGRAM unwrap reference is keyed by program_number, so
    // it becomes unreachable once the program leaves the PAT. A later program
    // re-using that number must not anchor its first sample onto the dead
    // program's epoch.
    #[test]
    fn pat_program_removal_clears_the_program_clock() {
        let pat_v0 = build_pat_section(1, 0, &[(1, 0x1000)]);
        let pat_v0_pkt = wrap_section_in_ts_packet(0x0000, &pat_v0);
        let pmt_v0 = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        let pmt_v0_pkt = wrap_section_in_ts_packet(0x1000, &pmt_v0);

        let cfg = DemuxerConfig::builder().unwrap_timestamps(true).build();
        let mut demux = Demuxer::with_config(cfg);
        demux.feed(&pat_v0_pkt).unwrap();
        demux.feed(&pmt_v0_pkt).unwrap();

        // One sample on program 1's video PID establishes both the per-PID
        // accumulator and program 1's running clock.
        let _ = demux.unwrap_pts(0x0101, Pts90khz::new(900_000));
        assert!(
            demux.timestamps.has_program(1),
            "test setup: program 1 should hold a running unwrap reference"
        );

        // PAT v1 drops program 1 (program 2 takes its place on a new PMT PID).
        // CC=1 so this isn't swallowed as a duplicate of the v0 PAT.
        let pat_v1 = build_pat_section(1, 1, &[(2, 0x1100)]);
        let pat_v1_pkt = wrap_section_in_ts_packet_cc(0x0000, &pat_v1, 1);
        demux.feed(&pat_v1_pkt).unwrap();

        assert!(
            !demux.timestamps.has_program(1),
            "the running unwrap reference for a program removed from the PAT \
             must be dropped with the program"
        );
        assert!(
            demux.unwrap_of(0x0101).is_none(),
            "the removed program's elementary PIDs must lose their unwrap \
             accumulators too"
        );
    }

    /// A fully populated `PidState` for `pid`. Written as an exhaustive
    /// struct literal on purpose: a field added to `PidState` stops this
    /// compiling until the retirement tests below cover it.
    fn fully_populated_state() -> super::super::pid_state::PidState {
        use crate::mpegts::stats::{CodecKind, StreamCodecCounters, StreamStats};
        super::super::pid_state::PidState {
            stream_kind: Some(StreamKind::Unknown(0xFC)),
            program: Some(1),
            cc: Some(3),
            dup: true,
            last_pkt_raw: Some([0x47; 188]),
            last_pts: Some(90_000),
            unwrap: Some(super::super::timestamp::UnwrapState {
                last_raw: 1,
                last_unwrapped: 1,
            }),
            stats: Some(StreamStats::default()),
            codec: Some(StreamCodecCounters::new(CodecKind::Video)),
            subtitle_missing_descriptor_emitted: true,
            av1_registration_malformed_emitted: true,
            subtitle_descriptor_ambiguous_emitted: true,
            subtitle_seen: true,
        }
    }

    // The invariant the four teardown fixes restored by hand: once a PMT
    // retires a PID, NOTHING about it survives — no `PidState` entry and no
    // reassembler buffer.
    #[test]
    fn pmt_retiring_a_pid_leaves_no_entry_anywhere() {
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &build_pat_section(1, 0, &[(1, 0x1000)]));
        let pmt_v0 = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101), (0x03, 0x0102)]);
        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux
            .feed(&wrap_section_in_ts_packet(0x1000, &pmt_v0))
            .unwrap();

        // An in-flight video PES gives the reassembler a buffer for 0x0101; the
        // literal fills every other field.
        let mut pes_pkt = [0xFFu8; 188];
        pes_pkt[..4].copy_from_slice(&[0x47, 0x41, 0x01, 0x10]);
        pes_pkt[4..10].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00]);
        demux.feed(&pes_pkt).unwrap();
        assert!(demux.pes.has_pid(0x0101), "test setup: PES buffered");
        demux.pid_state.insert(0x0101, fully_populated_state());

        // PMT v1 keeps 0x0102 and drops 0x0101.
        let pmt_v1 = build_pmt_section(1, 0x0102, 1, &[(0x03, 0x0102)]);
        demux
            .feed(&wrap_section_in_ts_packet_cc(0x1000, &pmt_v1, 1))
            .unwrap();

        assert!(
            demux.pid_state.get(&0x0101).is_none(),
            "a retired PID must have no PidState entry"
        );
        assert!(
            !demux.pes.has_pid(0x0101),
            "a retired PID must have no reassembler buffer"
        );
        assert!(
            demux
                .pid_state
                .get(&0x0102)
                .is_some_and(|s| s.stream_kind.is_some()),
            "the persisting PID keeps its kind"
        );
    }

    // Same invariant through the other door: the program leaves the PAT.
    #[test]
    fn pat_removing_a_program_leaves_no_entry_for_its_pids() {
        let pat_pkt = wrap_section_in_ts_packet(0x0000, &build_pat_section(1, 0, &[(1, 0x1000)]));
        let pmt_v0 = build_pmt_section(1, 0x0101, 0, &[(0x1B, 0x0101)]);
        let mut demux = Demuxer::new();
        demux.feed(&pat_pkt).unwrap();
        demux
            .feed(&wrap_section_in_ts_packet(0x1000, &pmt_v0))
            .unwrap();

        // An in-flight video PES gives the reassembler a buffer for 0x0101.
        let mut pes_pkt = [0xFFu8; 188];
        pes_pkt[..4].copy_from_slice(&[0x47, 0x41, 0x01, 0x10]);
        pes_pkt[4..10].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00]);
        demux.feed(&pes_pkt).unwrap();
        assert!(demux.pes.has_pid(0x0101), "test setup: PES buffered");
        demux.pid_state.insert(0x0101, fully_populated_state());
        demux.pid_state.insert(0x1000, fully_populated_state());

        let pat_v1 = build_pat_section(1, 1, &[(2, 0x1100)]);
        demux
            .feed(&wrap_section_in_ts_packet_cc(0x0000, &pat_v1, 1))
            .unwrap();

        assert!(demux.pid_state.get(&0x0101).is_none());
        assert!(!demux.pes.has_pid(0x0101));
        // The removed program's PMT PID keeps no continuity baseline.
        assert!(
            demux
                .pid_state
                .get(&0x1000)
                .is_none_or(|s| s.cc.is_none() && !s.dup && s.last_pkt_raw.is_none())
        );
    }

    /// Per ISO/IEC 13818-1 §2.4.4.4, PSI sections with `current_next_indicator=0`
    /// describe a future-staged table not yet in effect. ffmpeg drops these
    /// (mpegts.c:1759, 2610-2611, 2832, 2969-2970, 2974). If we instead process
    /// them and bump `pat_version`, the *real* current section that follows is
    /// then deduplicated as "same version, skip", silently dropping streams
    /// that are still active.
    ///
    /// We verify the staged PAT is not processed by feeding only the staged
    /// PAT (cn=0) followed by a PMT on the PID it announces. With the bug,
    /// the staged PAT installs PID 0x100 in `programs` and the PMT then fires
    /// a ProgramMap. With the fix, the PAT is dropped, PID 0x100 is unknown,
    /// and the PMT is silently ignored — no ProgramMap.
    #[test]
    fn pat_with_current_next_indicator_zero_is_dropped() {
        let mut demux = Demuxer::new();

        // Build a PAT with current_next_indicator=0 announcing program 1 → 0x100.
        // Byte 5 = 0xC0 → reserved(2)=11, version(5)=0, current_next(1)=0.
        let mut pat = vec![
            0x00, // table_id = PAT
            0xB0, 0x0D, // syntax=1, section_length=0x00D
            0x00, 0x01, // transport_stream_id=1
            0xC0, // reserved=11, version=0, current_next=0
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number=1
            0xE1, 0x00, // reserved=111, pmt_pid=0x100
        ];
        let pat_crc = crate::mpegts::common::crc32::crc32_mpeg2(&pat);
        pat.extend_from_slice(&pat_crc.to_be_bytes());

        let mut ts_pat = [0xFFu8; 188];
        ts_pat[0] = 0x47;
        ts_pat[1] = 0x40; // PUSI=1, pid_high=0
        ts_pat[2] = 0x00; // pid_low=0 → PID 0x0000
        ts_pat[3] = 0x10; // adaptation=01, CC=0
        ts_pat[4] = 0x00; // pointer_field
        ts_pat[5..5 + pat.len()].copy_from_slice(&pat);
        demux.feed(&ts_pat).expect("staged PAT feed");

        // Build a valid current PMT (cn=1) for program 1 announcing one
        // H.264 stream on PID 0x101. Body is 18 bytes (0x12) past byte 3.
        let mut pmt = vec![
            0x02, // table_id = PMT
            0xB0, 0x12, // syntax=1, section_length=0x012
            0x00, 0x01, // program_number=1
            0xC1, // reserved=11, version=0, current_next=1
            0x00, 0x00, // section_number, last_section_number
            0xE1, 0x01, // reserved=111, pcr_pid=0x101
            0xF0, 0x00, // reserved=1111, program_info_length=0
            0x1B, // stream_type = H.264
            0xE1, 0x01, // reserved=111, elementary_pid=0x101
            0xF0, 0x00, // reserved=1111, es_info_length=0
        ];
        let pmt_crc = crate::mpegts::common::crc32::crc32_mpeg2(&pmt);
        pmt.extend_from_slice(&pmt_crc.to_be_bytes());

        let mut ts_pmt = [0xFFu8; 188];
        ts_pmt[0] = 0x47;
        ts_pmt[1] = 0x41; // PUSI=1, pid_high=1
        ts_pmt[2] = 0x00; // pid_low=0 → PID 0x100
        ts_pmt[3] = 0x10; // adaptation=01, CC=0
        ts_pmt[4] = 0x00; // pointer_field
        ts_pmt[5..5 + pmt.len()].copy_from_slice(&pmt);
        demux.feed(&ts_pmt).expect("PMT feed");

        // With the fix, the staged PAT is dropped → PID 0x100 was never
        // installed as a PMT PID → the PMT packet on 0x100 is ignored → no
        // ProgramMap fires. With the bug, the staged PAT installed the
        // program and a ProgramMap fires from the PMT.
        let mut saw_program_map = false;
        while let Some(ev) = demux.next_event() {
            if matches!(ev, DemuxEvent::ProgramMap(_)) {
                saw_program_map = true;
            }
        }
        assert!(
            !saw_program_map,
            "PAT with current_next=0 must be dropped — its PID 0x100 must not \
             be registered, so a subsequent PMT on 0x100 is ignored and no \
             ProgramMap fires"
        );
    }

    /// Per ISO/IEC 13818-1 §2.4.4.4, the PMT case mirrors the PAT case: a PMT
    /// with `current_next_indicator=0` is the next staged table, not the
    /// current one. Processing it bumps `pmt_version`, which then dedupes the
    /// real current PMT — silently dropping streams.
    #[test]
    fn pmt_with_current_next_indicator_zero_is_dropped() {
        let mut demux = Demuxer::new();

        // Seed a normal PAT mapping program 1 → PMT PID 0x100, so the
        // demuxer routes packets on PID 0x100 to handle_pmt_section.
        let mut pat = vec![
            0x00, // table_id
            0xB0, 0x0D, // syntax=1, section_length=0x00D
            0x00, 0x01, // transport_stream_id=1
            0xC1, // reserved=11, version=0, current_next=1
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number=1
            0xE1, 0x00, // reserved=111, pmt_pid=0x100
        ];
        let pat_crc = crate::mpegts::common::crc32::crc32_mpeg2(&pat);
        pat.extend_from_slice(&pat_crc.to_be_bytes());

        let mut ts_pat = [0xFFu8; 188];
        ts_pat[0] = 0x47;
        ts_pat[1] = 0x40;
        ts_pat[2] = 0x00;
        ts_pat[3] = 0x10;
        ts_pat[4] = 0x00;
        ts_pat[5..5 + pat.len()].copy_from_slice(&pat);
        demux.feed(&ts_pat).expect("PAT feed");

        // Drain PAT-induced events (no ProgramMap yet — needs PMT).
        while demux.next_event().is_some() {}

        // Build a PMT with current_next_indicator=0 announcing one H.264
        // video stream on PID 0x101. Byte 5 = 0xC0.
        //   table_id=0x02, syntax=1, section_length, program_number=1,
        //   byte5=0xC0 (current_next=0), section_number=0, last=0,
        //   reserved+pcr_pid=0xE101, reserved+program_info_length=0xF000,
        //   stream_type=0x1B (H.264), reserved+elementary_pid=0xE101,
        //   reserved+es_info_length=0xF000, CRC32.
        // Body length from byte 3: 5 (header tail) + 4 (program_info hdr)
        //   + 5 (stream entry) + 4 (CRC) = 18 = 0x12.
        let mut pmt = vec![
            0x02, // table_id = PMT
            0xB0, 0x12, // syntax=1, section_length=0x012
            0x00, 0x01, // program_number=1
            0xC0, // reserved=11, version=0, current_next=0
            0x00, 0x00, // section_number, last_section_number
            0xE1, 0x01, // reserved=111, pcr_pid=0x101
            0xF0, 0x00, // reserved=1111, program_info_length=0
            0x1B, // stream_type = H.264
            0xE1, 0x01, // reserved=111, elementary_pid=0x101
            0xF0, 0x00, // reserved=1111, es_info_length=0
        ];
        let pmt_crc = crate::mpegts::common::crc32::crc32_mpeg2(&pmt);
        pmt.extend_from_slice(&pmt_crc.to_be_bytes());

        let mut ts_pmt = [0xFFu8; 188];
        ts_pmt[0] = 0x47;
        ts_pmt[1] = 0x41; // PUSI=1, pid_high=1
        ts_pmt[2] = 0x00; // pid_low=0 → PID = 0x100
        ts_pmt[3] = 0x10;
        ts_pmt[4] = 0x00; // pointer_field
        ts_pmt[5..5 + pmt.len()].copy_from_slice(&pmt);
        demux.feed(&ts_pmt).expect("staged PMT feed");

        // The staged PMT must NOT produce a ProgramMap event.
        let mut saw_program_map = false;
        while let Some(ev) = demux.next_event() {
            if matches!(ev, DemuxEvent::ProgramMap(_)) {
                saw_program_map = true;
            }
        }
        assert!(
            !saw_program_map,
            "PMT with current_next=0 must not produce ProgramMap events"
        );
    }

    #[test]
    fn first_pat_creates_program_trackers_for_all_entries() {
        let mut demuxer = Demuxer::new();
        let pat = pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0);
        demuxer.feed(&pat).unwrap();

        let progs = demuxer.programs_for_test();
        assert_eq!(
            progs.len(),
            2,
            "expected 2 program trackers, got {}",
            progs.len()
        );
        assert!(
            progs.contains_key(&0x1000),
            "missing tracker for pmt_pid=0x1000"
        );
        assert!(
            progs.contains_key(&0x1100),
            "missing tracker for pmt_pid=0x1100"
        );
    }

    #[test]
    fn pat_version_bump_adds_new_program() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        assert_eq!(demuxer.programs_for_test().len(), 1);

        demuxer
            .feed(&pat_packet_with_programs_cc(
                &[(1, 0x1000), (2, 0x1100)],
                1,
                1,
            ))
            .unwrap();
        let progs = demuxer.programs_for_test();
        assert_eq!(
            progs.len(),
            2,
            "expected 2 trackers after version bump, got {}",
            progs.len()
        );
        assert!(progs.contains_key(&0x1000));
        assert!(progs.contains_key(&0x1100));
    }

    #[test]
    fn pat_version_bump_removes_dropped_program() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
            .unwrap();
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        let progs = demuxer.programs_for_test();
        assert_eq!(
            progs.len(),
            1,
            "expected 1 tracker after program removal, got {}",
            progs.len()
        );
        assert!(
            progs.contains_key(&0x1000),
            "surviving program 1 tracker missing"
        );
        assert!(
            !progs.contains_key(&0x1100),
            "dropped program 2 tracker still present"
        );
    }

    #[test]
    fn each_program_has_independent_pmt_version() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011), (0x06, 0x1031)],
                3,
            ))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test(
                0x1100,
                2,
                0x1111,
                &[(0x24, 0x1111), (0x06, 0x1131)],
                5,
            ))
            .unwrap();

        let progs = demuxer.programs_for_test();
        assert_eq!(progs[&0x1000].pmt_version, Some(3));
        assert_eq!(progs[&0x1100].pmt_version, Some(5));
        assert_eq!(progs[&0x1000].streams.len(), 2);
        assert_eq!(progs[&0x1100].streams.len(), 2);
    }

    #[test]
    fn pid_collision_across_programs_emits_nonconformant() {
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
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();

        let mut nonconformant_seen = false;
        while let Some(ev) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue: NonConformantIssue::PidReusedAcrossPrograms { pid: 0x1011, .. },
                ..
            } = ev
            {
                nonconformant_seen = true;
            }
        }
        assert!(
            nonconformant_seen,
            "expected PidReusedAcrossPrograms event for PID 0x1011"
        );

        let progs = demuxer.programs_for_test();
        assert!(
            progs[&0x1000].streams.iter().any(|s| s.pid == 0x1011),
            "program 1 should retain ownership of PID 0x1011"
        );
        assert!(
            !progs[&0x1100].streams.iter().any(|s| s.pid == 0x1011),
            "program 2 must not own PID 0x1011 after collision"
        );
    }

    #[test]
    fn stream_kind_by_pid_tracks_across_pat_changes() {
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

        let progs = demuxer.programs_for_test();
        assert!(progs.contains_key(&0x1000), "program 1 tracker missing");
        assert_eq!(progs[&0x1000].streams.len(), 1);
        assert_eq!(progs[&0x1000].streams[0].pid, 0x1011);

        demuxer
            .feed(&pat_packet_with_programs_cc(
                &[(1, 0x1000), (2, 0x1100)],
                1,
                1,
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

        let progs = demuxer.programs_for_test();
        assert_eq!(progs.len(), 2, "expected 2 program trackers after add");
        assert!(
            progs.contains_key(&0x1000),
            "program 1 tracker must survive"
        );
        assert!(progs.contains_key(&0x1100), "program 2 tracker must appear");
        assert!(
            progs[&0x1000].streams.iter().any(|s| s.pid == 0x1011),
            "program 1 stream 0x1011 must survive after PAT bump"
        );
        assert!(
            progs[&0x1100].streams.iter().any(|s| s.pid == 0x1111),
            "program 2 stream 0x1111 must be tracked"
        );
    }

    #[test]
    fn program_removed_drops_streams_from_tracker() {
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

        assert_eq!(
            demuxer.programs_for_test().len(),
            2,
            "expected 2 trackers before removal"
        );

        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        let progs = demuxer.programs_for_test();
        assert_eq!(progs.len(), 1, "expected 1 tracker after removal");
        assert!(
            progs.contains_key(&0x1000),
            "surviving program 1 tracker missing"
        );
        assert!(
            !progs.contains_key(&0x1100),
            "dropped program 2 tracker still present"
        );
        assert!(
            progs[&0x1000].streams.iter().any(|s| s.pid == 0x1011),
            "program 1 stream 0x1011 must survive program 2 removal"
        );
    }

    /// A complete 2-section PAT must reassemble such that BOTH
    /// declared programs produce trackers end-to-end, and must NOT emit
    /// PsiMultiSectionUnsupported.
    #[test]
    fn demuxer_reassembles_complete_multi_section_pat() {
        // Section 0 carries program 1 on PMT PID 0x100;
        // section 1 carries program 2 on PMT PID 0x200.
        // Both share transport_stream_id=0x0001, version=0, current_next=1.
        let s0 = pat_packet_with_multi_section(0, 1, &[(1u16, 0x0100u16)]);
        // Section 1 arrives as a separate TS packet on the same PID, so its CC
        // must differ from section 0's (CC=0) to avoid duplicate suppression.
        let s1 = pat_packet_with_multi_section_cc(1, 1, &[(2u16, 0x0200u16)], 1);
        let mut demuxer = Demuxer::new();
        demuxer.feed(&s0).unwrap();
        demuxer.feed(&s1).unwrap();
        // Feed PMTs for BOTH reassembled PMT PIDs so each tracker emits a
        // ProgramMap. If apply_pat_programs silently dropped section 1's
        // program, no tracker would exist for PMT PID 0x0200 and the PMT
        // arriving on it would be dropped (no ProgramMap for program 2).
        let pmt1 = pmt_packet_for_test(0x0100, 1, 0x0101, &[(0x1B, 0x0101)], 0);
        let pmt2 = pmt_packet_for_test(0x0200, 2, 0x0201, &[(0x1B, 0x0201)], 0);
        demuxer.feed(&pmt1).unwrap();
        demuxer.feed(&pmt2).unwrap();
        demuxer.flush();
        let events = drain_all_events(&mut demuxer);

        let saw_multi_section_nonconformance = events.iter().any(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiMultiSectionUnsupported { .. },
                    ..
                }
            )
        });
        assert!(
            !saw_multi_section_nonconformance,
            "a VALID complete multi-section PAT must NOT emit PsiMultiSectionUnsupported"
        );
        // Both reassembled sections must have produced trackers: assert a
        // PMT-driven ProgramMap (one with streams) fired for program 1 AND
        // program 2.
        let saw_program_1 = events.iter().any(|e| {
            matches!(e, DemuxEvent::ProgramMap(pm) if pm.program_number == 1 && !pm.streams.is_empty())
        });
        let saw_program_2 = events.iter().any(|e| {
            matches!(e, DemuxEvent::ProgramMap(pm) if pm.program_number == 2 && !pm.streams.is_empty())
        });
        assert!(
            saw_program_1,
            "program 1 (PAT section 0) must produce a ProgramMap with streams"
        );
        assert!(
            saw_program_2,
            "program 2 (PAT section 1) must produce a ProgramMap with streams — proves section 1 was not dropped during reassembly"
        );
    }

    /// An INCOMPLETE multi-section PAT (only section 0 of 2) must
    /// stay pending — no ProgramMap AND no PsiMultiSectionUnsupported.
    #[test]
    fn demuxer_does_not_emit_program_map_for_incomplete_multi_section_pat() {
        // Only section 0 of a 2-section PAT, section 1 never arrives.
        let s0 = pat_packet_with_multi_section(0, 1, &[(1u16, 0x0100u16)]);
        let mut demuxer = Demuxer::new();
        demuxer.feed(&s0).unwrap();
        demuxer.flush();
        let events = drain_all_events(&mut demuxer);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DemuxEvent::ProgramMap(_))),
            "an INCOMPLETE multi-section PAT must NOT apply a partial program set"
        );
        assert!(
            !events.iter().any(|e| {
                matches!(
                    e,
                    DemuxEvent::NonConformant {
                        issue: NonConformantIssue::PsiMultiSectionUnsupported { .. },
                        ..
                    }
                )
            }),
            "an INCOMPLETE (not broken) multi-section PAT must NOT emit PsiMultiSectionUnsupported"
        );
    }

    #[test]
    fn demuxer_emits_non_conformance_on_multi_section_pmt() {
        // Drive a normal PAT first so the demuxer creates a tracker for
        // the PMT PID; then feed a PMT with last_section_number=1. PAT
        // lands; PMT triggers PsiMultiSectionUnsupported on its own PID.
        let pat = pat_packet_with_programs(&[(1, 0x100)], 0);
        let pmt = pmt_packet_with_multi_section(0x100, 1, 0x101, &[(0x1B /* H.264 */, 0x101)], 0);

        let mut demuxer = Demuxer::new();
        demuxer.feed(&pat).unwrap();
        demuxer.feed(&pmt).unwrap();
        let events = drain_all_events(&mut demuxer);

        let nc = events.iter().find_map(|e| match e {
            DemuxEvent::NonConformant { issue, .. } => Some(issue.clone()),
            _ => None,
        });
        match nc {
            Some(NonConformantIssue::PsiMultiSectionUnsupported {
                pid,
                table_id,
                last_section_number,
            }) => {
                assert_eq!(pid, 0x100);
                assert_eq!(table_id, 0x02);
                assert_eq!(last_section_number, 1);
            }
            other => panic!("expected PsiMultiSectionUnsupported, got {other:?}"),
        }
        // ProgramMap may fire from the PAT (announces the empty program),
        // but the PMT-driven version (which would populate streams) must
        // NOT have arrived — the rejection happens before stream emission.
        // The empty ProgramMap from the PAT carries no streams; assert no
        // ProgramMap event contains streams.
        for e in &events {
            if let DemuxEvent::ProgramMap(pm) = e {
                assert!(
                    pm.streams.is_empty(),
                    "no PMT-driven ProgramMap should have streams after rejection: {pm:?}"
                );
            }
        }
    }

    #[test]
    fn pat_removed_program_clears_cc_by_pid() {
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
        // Seed CC entries for both programs' video PIDs + the PMT PIDs.
        demuxer.feed(&payload_packet_for_test(0x1011, 5)).unwrap();
        demuxer.feed(&payload_packet_for_test(0x1111, 7)).unwrap();
        assert!(
            demuxer
                .pid_state
                .get(&0x1011)
                .is_some_and(|s| s.cc.is_some())
        );
        assert!(
            demuxer
                .pid_state
                .get(&0x1111)
                .is_some_and(|s| s.cc.is_some())
        );
        assert!(
            demuxer
                .pid_state
                .get(&0x1100)
                .is_some_and(|s| s.cc.is_some())
        ); // PMT PID

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        assert!(
            demuxer
                .pid_state
                .get(&0x1011)
                .is_some_and(|s| s.cc.is_some()),
            "program 1's PID 0x1011 cc must survive"
        );
        assert!(
            demuxer
                .pid_state
                .get(&0x1111)
                .is_none_or(|s| s.cc.is_none()),
            "program 2's PID 0x1111 cc must be cleared"
        );
        assert!(
            demuxer
                .pid_state
                .get(&0x1100)
                .is_none_or(|s| s.cc.is_none()),
            "program 2's PMT PID 0x1100 cc must be cleared"
        );
    }

    #[test]
    fn pat_removed_program_clears_stream_kind_and_pid_to_program() {
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
        assert!(demuxer.stream_kind(0x1111).is_some());
        assert!(demuxer.program_of(0x1111).is_some());

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        // Sanity: the removed program's PID also leaves the routing maps.
        assert!(
            demuxer.stream_kind(0x1111).is_none(),
            "stream kind must drop removed program's PID"
        );
        assert!(
            demuxer.program_of(0x1111).is_none(),
            "owning program must drop removed program's PID"
        );
    }

    /// A PMT version change that drops an elementary PID must clear that
    /// PID's per-PID routing state, exactly as PAT program removal does. Before
    /// the fix, `stream_kind_by_pid` retained the removed PID so later packets
    /// on it were still routed as PES (stale-sample emission).
    #[test]
    fn pmt_version_change_clears_removed_stream_pid_state() {
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        // PMT v0: video PID 0x1011.
        demuxer
            .feed(&pmt_packet_for_test(
                0x1000,
                1,
                0x1011,
                &[(0x1B, 0x1011)],
                0,
            ))
            .unwrap();
        assert!(demuxer.stream_kind(0x1011).is_some());
        // PMT v1 (same program 1): replaces 0x1011 with 0x1012.
        // CC=1 so it isn't swallowed as a duplicate of the v0 PMT on this PID.
        demuxer
            .feed(&pmt_packet_for_test_cc(
                0x1000,
                1,
                0x1012,
                &[(0x1B, 0x1012)],
                1,
                1,
            ))
            .unwrap();
        assert!(
            demuxer.stream_kind(0x1011).is_none(),
            "removed PID 0x1011 must no longer route as a stream"
        );
        assert!(
            demuxer.program_of(0x1011).is_none(),
            "removed PID 0x1011 must have no owning program"
        );
        assert!(
            demuxer.stream_kind(0x1012).is_some(),
            "the surviving new PID 0x1012 must route"
        );
    }

    /// Lenient: a valid multi-section PAT must NOT surface a false
    /// `PsiSyntax(SectionNumberNonZero)` for its section_number>0 sections.
    #[test]
    fn valid_multi_section_pat_emits_no_psi_syntax_event() {
        let s0 = pat_packet_with_multi_section(0, 1, &[(1u16, 0x0100u16)]);
        let s1 = pat_packet_with_multi_section(1, 1, &[(2u16, 0x0200u16)]);
        let mut demuxer = Demuxer::new();
        demuxer.feed(&s0).unwrap();
        demuxer.feed(&s1).unwrap();
        demuxer.flush();
        let events = drain_all_events(&mut demuxer);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiSyntax { .. },
                    ..
                }
            )),
            "a valid multi-section PAT must not emit PsiSyntax"
        );
    }

    /// Strict: `StrictMode::Full` must accept a valid multi-section PAT —
    /// the section_number>0 of section 1 is conformant, not a hard failure.
    #[test]
    fn strict_full_accepts_valid_multi_section_pat() {
        let s0 = pat_packet_with_multi_section(0, 1, &[(1u16, 0x0100u16)]);
        let s1 = pat_packet_with_multi_section(1, 1, &[(2u16, 0x0200u16)]);
        let mut demuxer = Demuxer::with_config(
            DemuxerConfig::builder()
                .strict(crate::mpegts::demux::StrictMode::Full)
                .build(),
        );
        demuxer.feed(&s0).unwrap();
        let r = demuxer.feed(&s1);
        assert!(
            r.is_ok(),
            "StrictMode::Full must not reject a valid multi-section PAT (err = {:?})",
            r.err()
        );
    }

    /// A PAT version that reassigns an existing PMT PID to a different
    /// program_number must adopt the new program — its PMT must be accepted,
    /// not rejected as `PmtProgramNumberMismatch` against the stale identity.
    #[test]
    fn pat_reuse_of_pmt_pid_for_new_program_accepts_new_pmt() {
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
        // PAT v1: program 2 reuses the same PMT PID 0x1000.
        // Both PAT and PMT need CC=1 — they arrive on PIDs already seen at CC=0.
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(2, 0x1000)], 1, 1))
            .unwrap();
        demuxer
            .feed(&pmt_packet_for_test_cc(
                0x1000,
                2,
                0x1011,
                &[(0x1B, 0x1011)],
                1,
                1,
            ))
            .unwrap();
        demuxer.flush();
        let events = drain_all_events(&mut demuxer);
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PmtProgramNumberMismatch { .. },
                    ..
                }
            )),
            "reused PMT PID with a new program must not raise PmtProgramNumberMismatch"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DemuxEvent::ProgramMap(pm) if pm.program_number == 2)),
            "the new program 2 must produce a ProgramMap"
        );
    }

    #[test]
    fn pat_removed_program_clears_pes_reassembler_state() {
        // Drive the Reassembler to buffer a partial PES on program 2's PID,
        // then drop program 2 via PAT and verify the PES partial buffer is
        // cleaned.
        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
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
        // Build a PUSI packet with PES start code so the Reassembler
        // initialises its partial-PES buffer on PID 0x1111.
        let mut pes_start = [0xFFu8; 188];
        pes_start[0] = 0x47;
        pes_start[1] = 0x40 | ((0x1111u16 >> 8) as u8 & 0x1F); // PUSI + pid hi
        pes_start[2] = (0x1111u16 & 0xFF) as u8;
        pes_start[3] = 0x10; // payload-only, CC=0
        // PES header at offset 4..
        pes_start[4] = 0x00;
        pes_start[5] = 0x00;
        pes_start[6] = 0x01;
        pes_start[7] = 0xE0; // stream_id = video
        pes_start[8] = 0x00;
        pes_start[9] = 0x00; // PES_packet_length = 0 (unbounded)
        pes_start[10] = 0x80;
        pes_start[11] = 0x00;
        pes_start[12] = 0x00; // PES_header_data_length=0
        demuxer.feed(&pes_start).unwrap();
        let buffered_before = demuxer.pes.buffered_bytes();
        assert!(
            buffered_before > 0,
            "PES reassembler must have buffered bytes for PID 0x1111"
        );

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        // The PID's partial-PES buffer must be cleaned; total_buffered drops
        // back to 0.
        assert_eq!(
            demuxer.pes.buffered_bytes(),
            0,
            "PES reassembler total_buffered must reflect dropped PID's bytes"
        );
    }
}
