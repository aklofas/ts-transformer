//! In-place reconfigure: the plan (the diff between the live slot table and
//! a new `MuxerConfig`, and every refusal, computed without touching the
//! muxer so a refused call leaves it unchanged) and its infallible apply.

use super::slot::{ProgramSlot, pmt_identity_key};
use super::state::{self, pmt_stream_type_byte, resolve_pcr_pid};
use super::{MuxStreamKind, Muxer, MuxerConfig, MuxerProgramConfig, StreamSpec};
use crate::error::MuxError;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

/// One stream of a planned program, in the new program's declaration order.
pub(super) struct PlannedStream {
    pub(super) kind: MuxStreamKind,
    /// Index into the slot's stream vector of `kind`.
    pub(super) within: usize,
    /// Index into the new program's `streams`.
    pub(super) spec_index: usize,
    /// The stream keeps its live slot, handle and continuity counter.
    pub(super) survivor: bool,
}

/// One program of the new config and the slot it lands in.
pub(super) struct PlannedProgram {
    pub(super) slot: usize,
    /// Index into the new config's `programs`.
    pub(super) config_index: usize,
    /// Resolved PCR PID under the new config.
    pub(super) pcr_pid: u16,
    pub(super) streams: Vec<PlannedStream>,
    /// Live PIDs of a matched program that the new program drops.
    pub(super) removed_pids: Vec<u16>,
}

pub(super) struct ReconfigurePlan {
    /// In the new config's program order.
    pub(super) programs: Vec<PlannedProgram>,
    /// Live slots whose program is absent from the new config.
    pub(super) removed_slots: Vec<usize>,
}

fn kind_of(spec: &StreamSpec) -> MuxStreamKind {
    match spec {
        StreamSpec::Video { .. } => MuxStreamKind::Video,
        StreamSpec::Klv { .. } => MuxStreamKind::Klv,
        StreamSpec::Audio { .. } => MuxStreamKind::Audio,
        StreamSpec::Subtitle { .. } => MuxStreamKind::Subtitle,
        StreamSpec::Data { .. } => MuxStreamKind::Data,
    }
}

/// Same PMT stream_type byte and same codec / KLV stream type. Subtitles
/// compare the codec family; its fields are descriptor content.
fn same_stream_identity(old: &StreamSpec, new: &StreamSpec) -> bool {
    let same_codec = match (old, new) {
        (StreamSpec::Video { codec: a, .. }, StreamSpec::Video { codec: b, .. }) => a == b,
        (StreamSpec::Klv { stream_type: a, .. }, StreamSpec::Klv { stream_type: b, .. }) => a == b,
        (StreamSpec::Audio { codec: a, .. }, StreamSpec::Audio { codec: b, .. }) => a == b,
        (StreamSpec::Subtitle { codec: a, .. }, StreamSpec::Subtitle { codec: b, .. }) => {
            core::mem::discriminant(a) == core::mem::discriminant(b)
        }
        (StreamSpec::Data { .. }, StreamSpec::Data { .. }) => true,
        _ => false,
    };
    same_codec && pmt_stream_type_byte(old) == pmt_stream_type_byte(new)
}

/// Per-kind stream vector lengths of a slot.
fn kind_len(slot: &ProgramSlot, kind: MuxStreamKind) -> usize {
    match kind {
        MuxStreamKind::Video => slot.video.len(),
        MuxStreamKind::Klv => slot.klv.len(),
        MuxStreamKind::Audio => slot.audio.len(),
        MuxStreamKind::Subtitle => slot.subtitle.len(),
        MuxStreamKind::Data => slot.data.len(),
    }
}

/// Chooses stream slots for the new streams of one program slot. `free`
/// holds, per kind (in `KINDS` order), the indices that will be tombstoned
/// after the call.
struct StreamPlacer<'a> {
    slot: Option<&'a ProgramSlot>,
    free: [Vec<usize>; 5],
    appended: [usize; 5],
}

impl StreamPlacer<'_> {
    /// The free index that last carried `pid` as a `kind` stream, if any.
    fn claim_same_pid(&mut self, kind: MuxStreamKind, pid: u16) -> Option<usize> {
        let free = &mut self.free[kind_index(kind)];
        let i = self
            .slot
            .and_then(|s| s.placed_pids.get(&pid))
            .filter(|(placed, _)| *placed == kind)
            .and_then(|(_, w)| free.iter().position(|f| f == w))?;
        Some(free.remove(i))
    }

    /// The lowest free index, else a new index after the vector.
    fn claim_any(&mut self, kind: MuxStreamKind) -> usize {
        let k = kind_index(kind);
        if !self.free[k].is_empty() {
            return self.free[k].remove(0);
        }
        let base = self.slot.map_or(0, |s| kind_len(s, kind));
        self.appended[k] += 1;
        base + self.appended[k] - 1
    }
}

fn kind_index(kind: MuxStreamKind) -> usize {
    KINDS
        .iter()
        .position(|k| *k == kind)
        .expect("every kind is listed")
}

const KINDS: [MuxStreamKind; 5] = [
    MuxStreamKind::Video,
    MuxStreamKind::Klv,
    MuxStreamKind::Audio,
    MuxStreamKind::Subtitle,
    MuxStreamKind::Data,
];

/// Indices of `kind` in `slot` that are tombstoned now or whose PID is not
/// in `keep`.
fn free_indices(slot: &ProgramSlot, kind: MuxStreamKind, keep: &[u16]) -> Vec<usize> {
    fn collect<T>(v: &[Option<T>], pid: fn(&T) -> u16, keep: &[u16]) -> Vec<usize> {
        v.iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_none_or(|s| !keep.contains(&pid(s))))
            .map(|(w, _)| w)
            .collect()
    }
    match kind {
        MuxStreamKind::Video => collect(&slot.video, |s| s.pid, keep),
        MuxStreamKind::Klv => collect(&slot.klv, |s| s.pid, keep),
        MuxStreamKind::Audio => collect(&slot.audio, |s| s.pid, keep),
        MuxStreamKind::Subtitle => collect(&slot.subtitle, |s| s.pid, keep),
        MuxStreamKind::Data => collect(&slot.data, |s| s.pid, keep),
    }
}

fn refuse(reason: alloc::string::String) -> MuxError {
    MuxError::ConfigInvalid { reason }
}

/// Store `state` at `within`, growing `v` with tombstones as needed.
fn place<T>(v: &mut Vec<Option<T>>, within: usize, state: T) {
    if v.len() <= within {
        v.resize_with(within + 1, || None);
    }
    v[within] = Some(state);
}

/// Tombstone every stream of `slot`.
fn clear_streams(slot: &mut ProgramSlot) {
    slot.video.iter_mut().for_each(|s| *s = None);
    slot.klv.iter_mut().for_each(|s| *s = None);
    slot.audio.iter_mut().for_each(|s| *s = None);
    slot.subtitle.iter_mut().for_each(|s| *s = None);
    slot.data.iter_mut().for_each(|s| *s = None);
}

/// Tombstone the stream slot `slot` placed `pid` on.
fn clear_stream(slot: &mut ProgramSlot, pid: u16) {
    let (kind, w) = slot.placed_pids[&pid];
    match kind {
        MuxStreamKind::Video => slot.video[w] = None,
        MuxStreamKind::Klv => slot.klv[w] = None,
        MuxStreamKind::Audio => slot.audio[w] = None,
        MuxStreamKind::Subtitle => slot.subtitle[w] = None,
        MuxStreamKind::Data => slot.data[w] = None,
    }
}

impl Muxer {
    /// Replace the program list of a running muxer in place.
    ///
    /// Programs match by `program_number`; within a matched program, streams
    /// match by PID. A matched stream **survives** when its PMT
    /// `stream_type` byte and its codec / KLV stream type are unchanged;
    /// the rest of its spec and its descriptors may change (that program's
    /// PMT then changes). A survivor keeps its handle, its continuity
    /// counter and its program's PCR and PSI cadence timers. Packets already
    /// queued, including those of a removed program, still drain.
    ///
    /// # Refusals
    ///
    /// `config` is first checked by [`MuxerConfig::validate`] exactly as in
    /// [`Muxer::new`] (including distinct `metadata_service_id`s across
    /// synchronous KLV streams). Then each of these is a
    /// [`MuxError::ConfigInvalid`] naming the program and PID:
    ///
    /// 1. A PID present in both configs that does not survive: it moved to
    ///    another program, or changed its stream type or codec.
    /// 2. A matched program whose `pmt_pid` changed.
    /// 3. A matched program whose resolved PCR PID changed while the old
    ///    PCR PID's stream stays in the program.
    /// 4. Any non-program field of `MuxerConfig` (`pcr_interval_ms`,
    ///    `psi_interval_ms`, `buffer_packets`, `av1_carriage`) differs from
    ///    the running value.
    ///
    /// A refused call leaves the muxer exactly as it was.
    ///
    /// # Handles
    ///
    /// Handles to surviving streams stay valid. Handles to the streams of a
    /// new program come from the `*_handles_for_program` entry points after
    /// the call. A removed stream or program keeps its slot as a tombstone:
    /// its handle fails every `push_*_to` with
    /// [`MuxError::InvalidStreamHandle`], and `*_handles_for_program` on a
    /// removed program number returns [`MuxError::ProgramNotFound`]. A new
    /// program takes the tombstoned slot that last carried its program
    /// number, else any tombstoned slot, else a new slot; a new stream
    /// likewise prefers the tombstone that carried its PID. So a handle to a
    /// removed stream fails until a later reconfigure reuses its slot;
    /// re-adding the same program with the same PIDs revives the same
    /// handles; re-obtain handles after each call otherwise.
    ///
    /// # Wire
    ///
    /// Every live program is marked PSI-due, so the next `push_*` emits the
    /// new PAT and every live PMT before its payload ([`Muxer::request_psi`]
    /// after this call changes nothing). The PAT version increments (mod 32)
    /// when the list of `(program_number, pmt_pid)` changed; an identical
    /// list keeps it. A live program's PMT version increments (mod 32) when
    /// its rendered PMT changed. PMT versions are kept per PMT PID for the
    /// muxer's lifetime, so a program placed on a previously used PMT PID
    /// continues from that PID's last version + 1. A removed ES PID's
    /// continuity counter restarts at 0, and so does the counter of every
    /// PID a new stream is placed on; PMT PID counters continue.
    ///
    /// Removing a stream and re-adding it on the same PID with no successful
    /// `push_*` between the two calls coalesces the two table changes into
    /// one PMT update: receivers never see the stream retired, and observe a
    /// single continuity-counter discontinuity on that PID, whose counter
    /// restarted at 0. Push at least once between the two calls when
    /// receivers must see the retirement.
    ///
    /// # Stats
    ///
    /// Lifetime counters continue. A removed PID's
    /// [`MuxerStats::per_stream`](super::MuxerStats::per_stream) entry stays
    /// frozen and resumes counting if the PID is placed again, with its
    /// `program_number`, `stream_type` and `label` rewritten; its codec
    /// counters are dropped. New PIDs get entries at this call.
    /// `programs_configured` and `subtitle_streams_configured` count live
    /// programs and streams.
    pub fn reconfigure(&mut self, config: MuxerConfig) -> Result<(), MuxError> {
        // The plan runs `config.validate()` first, then every refusal.
        let plan = self.plan_reconfigure(&config)?;
        self.apply_reconfigure(plan, config);
        Ok(())
    }

    /// Apply a plan `plan_reconfigure` accepted for `config`. Infallible.
    fn apply_reconfigure(&mut self, plan: ReconfigurePlan, config: MuxerConfig) {
        // A planned program continues a live one when its slot carries the
        // same program number now.
        let matched: Vec<bool> = plan
            .programs
            .iter()
            .map(|p| {
                self.slots.get(p.slot).and_then(ProgramSlot::program_number)
                    == Some(config.programs[p.config_index].program_number)
            })
            .collect();

        let mut removed: Vec<u16> = Vec::new();
        for &idx in &plan.removed_slots {
            let slot = &mut self.slots[idx];
            let prog = slot.live.take().expect("removed slot is live");
            removed.extend(prog.streams.iter().map(StreamSpec::pid));
            clear_streams(slot);
        }
        for planned in &plan.programs {
            for &pid in &planned.removed_pids {
                clear_stream(&mut self.slots[planned.slot], pid);
            }
            removed.extend_from_slice(&planned.removed_pids);
        }
        for pid in removed {
            self.counters.reset(pid);
            self.stream_codec_counters.remove(&pid);
        }

        self.program_order = plan.programs.iter().map(|p| p.slot).collect();
        for (planned, matched) in plan.programs.iter().zip(matched) {
            let prog = &config.programs[planned.config_index];
            state::warn_on_descriptor_conflicts(prog);
            while self.slots.len() <= planned.slot {
                self.slots.push(ProgramSlot::empty());
            }
            let slot = &mut self.slots[planned.slot];
            if !matched {
                clear_streams(slot);
                slot.pcr_last = None;
            }

            // Per-kind states in declaration order, as the plan's streams.
            let (video, klv, audio, subtitle, data) = state::collect_stream_states(prog);
            let (mut video, mut klv, mut audio, mut subtitle, mut data) = (
                video.into_iter(),
                klv.into_iter(),
                audio.into_iter(),
                subtitle.into_iter(),
                data.into_iter(),
            );
            const PLANNED: &str = "the plan lists every stream of the program";
            for s in &planned.streams {
                let w = s.within;
                match s.kind {
                    MuxStreamKind::Video => place(&mut slot.video, w, video.next().expect(PLANNED)),
                    MuxStreamKind::Klv => place(&mut slot.klv, w, klv.next().expect(PLANNED)),
                    MuxStreamKind::Audio => place(&mut slot.audio, w, audio.next().expect(PLANNED)),
                    MuxStreamKind::Subtitle => {
                        place(&mut slot.subtitle, w, subtitle.next().expect(PLANNED))
                    }
                    MuxStreamKind::Data => place(&mut slot.data, w, data.next().expect(PLANNED)),
                }
                let pid = prog.streams[s.spec_index].pid();
                slot.placed_pids.insert(pid, (s.kind, w));
                if !s.survivor {
                    self.counters.reset(pid);
                }
            }

            let mut fresh = BTreeMap::new();
            state::initialize_stats(
                prog,
                &slot.video,
                &slot.klv,
                &slot.audio,
                &slot.subtitle,
                &slot.data,
                &mut fresh,
            );
            for (pid, entry) in fresh {
                match self.per_stream.get_mut(&pid) {
                    Some(kept) => {
                        kept.program_number = entry.program_number;
                        kept.stream_type = entry.stream_type;
                        kept.label = entry.label;
                    }
                    None => {
                        self.per_stream.insert(pid, entry);
                    }
                }
            }

            slot.pcr_pid = planned.pcr_pid;
            slot.pmt_descriptor_cache = state::build_pmt_descriptor_cache(prog);
            let identity = pmt_identity_key(prog, slot.pcr_pid, &slot.pmt_descriptor_cache);
            if !matched || identity != slot.pmt_identity {
                let version = self
                    .pmt_versions_by_pid
                    .get(&prog.pmt_pid)
                    .map_or(0, |v| (v + 1) & 0x1F);
                self.pmt_versions_by_pid.insert(prog.pmt_pid, version);
                slot.pmt_version = version;
            }
            slot.pmt_identity = identity;
            slot.last_program_number = prog.program_number;
            slot.live = Some(prog.clone());
        }

        let pat_entries = self.pat_entries();
        if pat_entries != self.pat_identity {
            self.pat_version = (self.pat_version + 1) & 0x1F;
            self.pat_identity = pat_entries;
        }
        self.request_psi();
        self.config = config;
    }

    /// Diff the live slots against `new` and refuse what cannot change in
    /// place. Programs match by `program_number`, streams by PID within a
    /// matched program; a program whose number changed is therefore a
    /// removal plus an addition. Never mutates `self`.
    pub(super) fn plan_reconfigure(&self, new: &MuxerConfig) -> Result<ReconfigurePlan, MuxError> {
        new.validate()?;
        self.refuse_global_field_change(new)?;

        // PID -> (slot, program config) over every live stream.
        let mut live_pids: BTreeMap<u16, (usize, &MuxerProgramConfig, &StreamSpec)> =
            BTreeMap::new();
        let mut slot_by_number: BTreeMap<u16, usize> = BTreeMap::new();
        for (idx, slot) in self.live_slots() {
            let prog = slot.live.as_ref().expect("live slot");
            slot_by_number.insert(prog.program_number, idx);
            for spec in &prog.streams {
                live_pids.insert(spec.pid(), (idx, prog, spec));
            }
        }

        let removed_slots: Vec<usize> = self
            .live_slots()
            .filter(|(_, s)| {
                let n = s.program_number().expect("live slot");
                !new.programs.iter().any(|p| p.program_number == n)
            })
            .map(|(idx, _)| idx)
            .collect();

        let slots = self.choose_program_slots(new, &slot_by_number, &removed_slots);

        let mut programs = Vec::with_capacity(new.programs.len());
        for (config_index, (prog, &slot_idx)) in new.programs.iter().zip(&slots).enumerate() {
            let matched = slot_by_number.get(&prog.program_number) == Some(&slot_idx);
            let slot = self.slots.get(slot_idx);
            let new_pids: Vec<u16> = prog.streams.iter().map(StreamSpec::pid).collect();

            let mut streams = Vec::with_capacity(prog.streams.len());
            let mut placer = StreamPlacer {
                slot,
                free: Default::default(),
                appended: [0; 5],
            };
            if let Some(slot) = slot {
                // A new program reuses every stream slot of its tombstone.
                let keep: &[u16] = if matched { &new_pids } else { &[] };
                placer.free = KINDS.map(|kind| free_indices(slot, kind, keep));
            }
            for (spec_index, spec) in prog.streams.iter().enumerate() {
                let kind = kind_of(spec);
                let pid = spec.pid();
                let survivor_within = match live_pids.get(&pid) {
                    None => None,
                    Some(&(old_slot, old_prog, old_spec)) => {
                        if old_slot != slot_idx || !matched {
                            return Err(refuse(format!(
                                "reconfigure: pid 0x{pid:04X} moves from program {} to program \
                                 {}; a PID survives only in its own program, so remove it in \
                                 one call and declare it in the next",
                                old_prog.program_number, prog.program_number,
                            )));
                        }
                        if !same_stream_identity(old_spec, spec) {
                            return Err(refuse(format!(
                                "reconfigure: pid 0x{pid:04X} in program {} changes its \
                                 stream type or codec ({} 0x{:02X} to {kind} 0x{:02X}); a \
                                 surviving PID keeps its stream_type and codec, so remove it \
                                 in one call and declare it in the next",
                                prog.program_number,
                                kind_of(old_spec),
                                pmt_stream_type_byte(old_spec),
                                pmt_stream_type_byte(spec),
                            )));
                        }
                        let (_, within) = self.slots[slot_idx].placed_pids[&pid];
                        Some(within)
                    }
                };
                streams.push(PlannedStream {
                    kind,
                    within: survivor_within.unwrap_or(usize::MAX),
                    spec_index,
                    survivor: survivor_within.is_some(),
                });
            }
            // Place new streams: every same-PID tombstone first, so an earlier
            // new stream cannot take a later one's own slot; then the rest.
            let mut unplaced: Vec<usize> = Vec::new();
            for (i, s) in streams.iter_mut().enumerate() {
                if !s.survivor {
                    let pid = prog.streams[s.spec_index].pid();
                    match placer.claim_same_pid(s.kind, pid) {
                        Some(w) => s.within = w,
                        None => unplaced.push(i),
                    }
                }
            }
            for i in unplaced {
                streams[i].within = placer.claim_any(streams[i].kind);
            }

            let pcr_pid = resolve_pcr_pid(prog);
            let mut removed_pids = Vec::new();
            if matched {
                let slot = &self.slots[slot_idx];
                let old = slot.live.as_ref().expect("matched slot is live");
                if old.pmt_pid != prog.pmt_pid {
                    return Err(refuse(format!(
                        "reconfigure: program {} changes its PMT PID from 0x{:04X} to \
                         0x{:04X}; a program keeps its PMT PID across a reconfigure",
                        prog.program_number, old.pmt_pid, prog.pmt_pid,
                    )));
                }
                removed_pids = old
                    .streams
                    .iter()
                    .map(StreamSpec::pid)
                    .filter(|pid| !new_pids.contains(pid))
                    .collect();
                if pcr_pid != slot.pcr_pid && !removed_pids.contains(&slot.pcr_pid) {
                    let default_rule = if prog.pcr_pid.is_none() {
                        format!(
                            "; with no pcr_pid set the PCR PID follows the default rule (the \
                             first video stream), so setting pcr_pid to 0x{:04X} keeps it",
                            slot.pcr_pid,
                        )
                    } else {
                        alloc::string::String::new()
                    };
                    return Err(refuse(format!(
                        "reconfigure: program {} changes its PCR PID from 0x{:04X} to \
                         0x{pcr_pid:04X} while 0x{:04X} stays in the program; keep the PCR \
                         PID or remove its stream in the same call{default_rule}",
                        prog.program_number, slot.pcr_pid, slot.pcr_pid,
                    )));
                }
            }

            programs.push(PlannedProgram {
                slot: slot_idx,
                config_index,
                pcr_pid,
                streams,
                removed_pids,
            });
        }

        Ok(ReconfigurePlan {
            programs,
            removed_slots,
        })
    }

    fn refuse_global_field_change(&self, new: &MuxerConfig) -> Result<(), MuxError> {
        let old = &self.config;
        let changed = if old.pcr_interval_ms != new.pcr_interval_ms {
            Some((
                "pcr_interval_ms",
                format!("{} to {}", old.pcr_interval_ms, new.pcr_interval_ms),
            ))
        } else if old.psi_interval_ms != new.psi_interval_ms {
            Some((
                "psi_interval_ms",
                format!("{} to {}", old.psi_interval_ms, new.psi_interval_ms),
            ))
        } else if old.buffer_packets != new.buffer_packets {
            Some((
                "buffer_packets",
                format!("{} to {}", old.buffer_packets, new.buffer_packets),
            ))
        } else if old.av1_carriage != new.av1_carriage {
            Some((
                "av1_carriage",
                format!("{:?} to {:?}", old.av1_carriage, new.av1_carriage),
            ))
        } else {
            None
        };
        match changed {
            None => Ok(()),
            Some((field, change)) => Err(refuse(format!(
                "reconfigure: {field} changes from {change}; only the program list can change \
                 in place, so build a new muxer to change {field}",
            ))),
        }
    }

    /// Slot index per new program, in config order: a matched program keeps
    /// its slot; a new one takes the free slot that last carried its number,
    /// else the lowest free slot, else a new slot at the end. Free = already
    /// tombstoned or removed by this call.
    fn choose_program_slots(
        &self,
        new: &MuxerConfig,
        slot_by_number: &BTreeMap<u16, usize>,
        removed_slots: &[usize],
    ) -> Vec<usize> {
        let mut free: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(idx, s)| !s.is_live() || removed_slots.contains(idx))
            .map(|(idx, _)| idx)
            .collect();
        let mut chosen: Vec<Option<usize>> = new
            .programs
            .iter()
            .map(|p| slot_by_number.get(&p.program_number).copied())
            .collect();
        // Same-number tombstones first, so an earlier new program cannot take
        // a later one's own slot.
        for (prog, choice) in new.programs.iter().zip(chosen.iter_mut()) {
            if choice.is_none()
                && let Some(i) = free
                    .iter()
                    .position(|&f| self.slots[f].last_program_number == prog.program_number)
            {
                *choice = Some(free.remove(i));
            }
        }
        let mut next_append = self.slots.len();
        chosen
            .into_iter()
            .map(|choice| {
                choice.unwrap_or_else(|| {
                    if free.is_empty() {
                        next_append += 1;
                        next_append - 1
                    } else {
                        free.remove(0)
                    }
                })
            })
            .collect()
    }
}
