//! Planning an in-place reconfigure: the diff between the live slot table
//! and a new `MuxerConfig`, and every refusal, computed without touching the
//! muxer so a refused call leaves it unchanged.

use super::slot::ProgramSlot;
use super::state::{pmt_stream_type_byte, resolve_pcr_pid};
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
    /// Same PID's tombstone, else the lowest free index, else append.
    fn place(&mut self, kind: MuxStreamKind, pid: u16) -> usize {
        let k = kind_index(kind);
        let free = &mut self.free[k];
        let preferred = self
            .slot
            .and_then(|s| s.placed_pids.get(&pid))
            .filter(|(placed, _)| *placed == kind)
            .and_then(|(_, w)| free.iter().position(|f| f == w));
        if let Some(i) = preferred.or((!free.is_empty()).then_some(0)) {
            return free.remove(i);
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

impl Muxer {
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
                let (within, survivor) = match survivor_within {
                    Some(w) => (w, true),
                    None => (placer.place(kind, pid), false),
                };
                streams.push(PlannedStream {
                    kind,
                    within,
                    spec_index,
                    survivor,
                });
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
                    return Err(refuse(format!(
                        "reconfigure: program {} changes its PCR PID from 0x{:04X} to \
                         0x{pcr_pid:04X} while 0x{:04X} stays in the program; keep the PCR \
                         PID or remove its stream in the same call",
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
