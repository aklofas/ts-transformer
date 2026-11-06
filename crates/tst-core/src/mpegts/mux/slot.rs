//! Per-program muxer state, one `ProgramSlot` per program.
//!
//! A stream handle's `program_index` is the slot's index in `Muxer::slots`
//! and its `within_index` indexes the slot's per-kind stream vector. A
//! removed program or stream stays in place as a `None` tombstone so the
//! handles of every other program and stream keep resolving.

use super::MuxerProgramConfig;
use super::state::{
    self, AudioStreamState, DataStreamState, KlvStreamState, SubtitleStreamState, VideoStreamState,
};
use alloc::vec::Vec;

pub(super) struct ProgramSlot {
    /// The program's configuration; `None` = tombstoned program.
    pub(super) live: Option<MuxerProgramConfig>,
    /// Video stream state in declaration order; `None` = tombstoned stream.
    /// The four vectors below have the same shape.
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
    /// Last PSI emission PTS, masked to 33 bits. None until first.
    pub(super) psi_last: Option<u64>,
    /// Last PCR emission value at 27 MHz. None until first.
    pub(super) pcr_last: Option<u64>,
}

impl ProgramSlot {
    pub(super) fn from_program(prog: &MuxerProgramConfig) -> Self {
        state::warn_on_descriptor_conflicts(prog);
        let (video, klv, audio, subtitle, data) = state::collect_stream_states(prog);
        Self {
            live: Some(prog.clone()),
            video: video.into_iter().map(Some).collect(),
            klv: klv.into_iter().map(Some).collect(),
            audio: audio.into_iter().map(Some).collect(),
            subtitle: subtitle.into_iter().map(Some).collect(),
            data: data.into_iter().map(Some).collect(),
            pcr_pid: state::resolve_pcr_pid(prog),
            pmt_descriptor_cache: state::build_pmt_descriptor_cache(prog),
            psi_last: None,
            pcr_last: None,
        }
    }

    pub(super) fn is_live(&self) -> bool {
        self.live.is_some()
    }

    pub(super) fn program_number(&self) -> Option<u16> {
        self.live.as_ref().map(|p| p.program_number)
    }
}
