//! Per-program muxer state, one `ProgramSlot` per program.
//!
//! A stream handle's `program_index` is the slot's index in `Muxer::slots`
//! and its `within_index` indexes the slot's per-kind stream vector. A
//! removed program or stream stays in place as a `None` tombstone so the
//! handles of every other program and stream keep resolving.

use super::state::{
    self, AudioStreamState, DataStreamState, KlvStreamState, SubtitleStreamState, VideoStreamState,
};
use super::{MuxStreamKind, MuxerProgramConfig};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

pub(super) struct ProgramSlot {
    /// The program's configuration; `None` = tombstoned program.
    pub(super) live: Option<MuxerProgramConfig>,
    /// Video stream state by stable within-index (a handle's low nibble);
    /// `None` = tombstoned stream. The current declaration order is
    /// `live.streams`; `placed_pids` maps a PID to its index. The four
    /// vectors below have the same shape.
    pub(super) video: Vec<Option<VideoStreamState>>,
    pub(super) klv: Vec<Option<KlvStreamState>>,
    pub(super) audio: Vec<Option<AudioStreamState>>,
    pub(super) subtitle: Vec<Option<SubtitleStreamState>>,
    pub(super) data: Vec<Option<DataStreamState>>,
    /// Resolved PCR PID.
    pub(super) pcr_pid: u16,
    /// Pre-composed PMT descriptor bytes per stream (KLVA auto-emit +
    /// caller-supplied), parallel to `live.streams`.
    pub(super) pmt_descriptor_cache: Vec<Vec<u8>>,
    /// Version of the PMT last rendered for this program.
    pub(super) pmt_version: u8,
    /// `pmt_identity_key` as of that version, to detect a changed PMT.
    pub(super) pmt_identity: Vec<u8>,
    /// Program clock at the last PSI emission, 33-bit 90 kHz. None until
    /// first, and after `request_psi`.
    pub(super) psi_last: Option<u64>,
    /// The program clock: the base of the last PCR sent, 33-bit 90 kHz.
    /// None until first.
    pub(super) clock: Option<u64>,
    /// Decode time of the last push on the PCR PID (or tick), 33-bit 90 kHz.
    /// None until first, and after the PCR PID changes.
    pub(super) pcr_stream_last: Option<u64>,
    /// PCRs sent on the current time base, saturating.
    pub(super) pcrs_on_time_base: u8,
    /// Program number this slot last carried; kept when the slot is
    /// tombstoned so a reconfigure re-adding that program prefers this slot.
    /// The in-place reconfigure overwrites it when it reuses the slot.
    pub(super) last_program_number: u16,
    /// Every PID this slot has placed -> the kind and within-index of the
    /// stream slot it occupied. Entries are never removed, so a tombstoned
    /// stream slot still names the PID it carried; they are overwritten on
    /// re-placement.
    pub(super) placed_pids: BTreeMap<u16, (MuxStreamKind, usize)>,
}

impl ProgramSlot {
    pub(super) fn from_program(prog: &MuxerProgramConfig) -> Self {
        state::warn_on_descriptor_conflicts(prog);
        let (video, klv, audio, subtitle, data) = state::collect_stream_states(prog);
        let pcr_pid = state::resolve_pcr_pid(prog);
        let pmt_descriptor_cache = state::build_pmt_descriptor_cache(prog);
        let mut placed_pids = BTreeMap::new();
        let kinds = [
            (
                MuxStreamKind::Video,
                video.iter().map(|s| s.pid).collect::<Vec<_>>(),
            ),
            (MuxStreamKind::Klv, klv.iter().map(|s| s.pid).collect()),
            (MuxStreamKind::Audio, audio.iter().map(|s| s.pid).collect()),
            (
                MuxStreamKind::Subtitle,
                subtitle.iter().map(|s| s.pid).collect(),
            ),
            (MuxStreamKind::Data, data.iter().map(|s| s.pid).collect()),
        ];
        for (kind, pids) in kinds {
            for (within, pid) in pids.into_iter().enumerate() {
                placed_pids.insert(pid, (kind, within));
            }
        }
        Self {
            pmt_version: 0,
            pmt_identity: pmt_identity_key(prog, pcr_pid, &pmt_descriptor_cache),
            live: Some(prog.clone()),
            video: video.into_iter().map(Some).collect(),
            klv: klv.into_iter().map(Some).collect(),
            audio: audio.into_iter().map(Some).collect(),
            subtitle: subtitle.into_iter().map(Some).collect(),
            data: data.into_iter().map(Some).collect(),
            pcr_pid,
            pmt_descriptor_cache,
            psi_last: None,
            clock: None,
            pcr_stream_last: None,
            pcrs_on_time_base: 0,
            last_program_number: prog.program_number,
            placed_pids,
        }
    }

    /// A tombstoned slot with no streams, appended when a reconfigure needs
    /// a new program slot.
    pub(super) fn empty() -> Self {
        Self {
            live: None,
            video: Vec::new(),
            klv: Vec::new(),
            audio: Vec::new(),
            subtitle: Vec::new(),
            data: Vec::new(),
            pcr_pid: 0,
            pmt_descriptor_cache: Vec::new(),
            pmt_version: 0,
            pmt_identity: Vec::new(),
            psi_last: None,
            clock: None,
            pcr_stream_last: None,
            pcrs_on_time_base: 0,
            last_program_number: 0,
            placed_pids: BTreeMap::new(),
        }
    }

    pub(super) fn is_live(&self) -> bool {
        self.live.is_some()
    }

    pub(super) fn program_number(&self) -> Option<u16> {
        self.live.as_ref().map(|p| p.program_number)
    }
}

/// Deterministic serialisation of exactly what the PMT writer renders for
/// `prog`: the PCR PID, the program descriptors, then each stream's
/// `(stream_type, pid, descriptor bytes)` in order. The same program renders
/// byte-identical PMT sections for equal keys; the key deliberately omits the
/// program number and version, which the PMT header carries.
pub(super) fn pmt_identity_key(
    prog: &MuxerProgramConfig,
    pcr_pid: u16,
    cache: &[Vec<u8>],
) -> Vec<u8> {
    let mut key = Vec::new();
    key.extend_from_slice(&pcr_pid.to_be_bytes());
    let program_descriptors: Vec<u8> = prog.program_descriptors.iter().flatten().copied().collect();
    key.extend_from_slice(&(program_descriptors.len() as u32).to_be_bytes());
    key.extend_from_slice(&program_descriptors);
    debug_assert_eq!(prog.streams.len(), cache.len());
    for (spec, descriptors) in prog.streams.iter().zip(cache) {
        key.push(state::pmt_stream_type_byte(spec));
        key.extend_from_slice(&spec.pid().to_be_bytes());
        key.extend_from_slice(&(descriptors.len() as u32).to_be_bytes());
        key.extend_from_slice(descriptors);
    }
    key
}
