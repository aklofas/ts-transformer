//! MuxSender-side MPEG-TS muxer.
//!
//! The public surface is `Muxer`, `MuxerConfig`, `VideoCodec`,
//! `KlvStreamType`. Internal helpers live in `ts`, `psi`, `pes` submodules.
//!
//! Re-export note: `Muxer`, `VideoCodec`, and `KlvStreamType` are re-exported
//! at the crate root (`tst_core::Muxer` etc.). `MuxerConfig` deliberately is
//! not — callers reach it via `mpegts::mux::MuxerConfig` so the construction
//! site is visually distinct from the SRT `SocketConfig` / `ListenerConfig`
//! already at the crate root. Don't "symmetry-fix" this.

pub(crate) mod pes;
pub(crate) mod psi;
pub(crate) mod ts;

mod state;
mod scheduling;
mod stats_accounting;
mod push_audio;
mod push_data;
mod push_klv;
mod push_subtitle;
mod push_video;

use crate::error::MuxError;
use alloc::vec::Vec;

mod types;
pub use types::*;

mod config;
pub use config::*;
pub use stats_accounting::MuxerStats;

// Re-exported through `super::` / `super::*` by sibling modules (scheduling.rs
// uses `super::StreamType`; the tests/ files use `super::*` glob). Keep here
// even though mod.rs itself no longer references these directly — the
// `Muxer::new` body that did was extracted into `state.rs`.
#[allow(unused_imports)]
use crate::mpegts::common::{StreamType, StreamTypeCode};
use alloc::collections::{BTreeMap, VecDeque};

use self::pes::MAX_PES_HEADER_SIZE;
use self::state::{
    AudioStreamState, DataStreamState, KlvStreamState, SubtitleStreamState, VideoStreamState,
};
use self::ts::ContinuityCounters;

/// MuxSender-side MPEG-TS muxer.
///
/// Construct with `Muxer::new(config)`, push encoded frames via `push_video`
/// and `push_klv`, then drain TS packets with `pull`. The muxer is
/// deterministic — output is a function of inputs only, not wall-clock time.
///
/// # Closing
///
/// `Muxer` is a passive aggregator — it owns no transport and no OS
/// handles. Drop is the only shutdown and is trivially synchronous.
/// Call [`Self::pull`] in a loop until it returns `0` before drop to
/// drain any TS packets sitting in the internal queue; bytes left in
/// the queue at drop are discarded.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `while muxer.pull(&mut buf) > 0 { /* ... */ } drop(muxer);` (or just let it fall out of scope) |
/// | Java | Drain via `pull()`, then let GC reclaim — no `AutoCloseable` needed |
/// | Kotlin | Drain via `pull()`, then let GC reclaim |
/// | Swift | `deinit` calls drop; explicit drain via `pull()` before exit |
/// | Python | Drain via `pull()` at end-of-stream; let GC reclaim |
/// | C | `tst_muxer_close(muxer)` — releases the muxer; caller is responsible for prior drain |
pub struct Muxer {
    config: MuxerConfig,

    /// Per-program pre-composed PMT descriptor bytes.
    /// `pmt_descriptor_caches[prog_idx][stream_idx]` = concatenated TLVs for
    /// that stream (KLVA auto-emit + caller-supplied). Indexed parallel to
    /// `config.programs[prog_idx].streams`. Borrowed at PMT emission time;
    /// never reallocated after construction.
    pmt_descriptor_caches: Vec<Vec<Vec<u8>>>,

    /// Per-program video stream state. `video_streams[prog_idx]` is the
    /// list of video streams for program `prog_idx`, in declaration order.
    /// `VideoStreamHandle::unpack()` → `(prog_idx, within_idx)` indexes here.
    video_streams: Vec<Vec<VideoStreamState>>,

    /// Per-program KLV stream state. Same indexing as `video_streams`.
    klv_streams: Vec<Vec<KlvStreamState>>,

    /// Per-program audio stream state. Same indexing as `video_streams`.
    /// `AudioStreamHandle::unpack()` → `(prog_idx, within_idx)` indexes here.
    audio_streams: Vec<Vec<AudioStreamState>>,

    /// Per-program subtitle stream state. Same indexing as `video_streams`.
    /// `SubtitleStreamHandle::unpack()` → `(prog_idx, within_idx)` indexes here.
    subtitle_streams: Vec<Vec<SubtitleStreamState>>,

    /// Per-program data stream state. Same indexing as `video_streams`.
    /// `DataStreamHandle::unpack()` → `(prog_idx, within_idx)` indexes here.
    data_streams: Vec<Vec<DataStreamState>>,

    /// Per-program resolved PCR PID. Indexed parallel to `config.programs`.
    pcr_pids: Vec<u16>,

    pcr_interval_27mhz: u64,
    psi_interval_90khz: i64,

    queue: VecDeque<[u8; 188]>,
    counters: ContinuityCounters,

    /// Per-program last PSI emission PTS, masked to 33 bits. None until first.
    /// Indexed parallel to `config.programs`.
    psi_last: Vec<Option<u64>>,

    /// Per-program last PCR emission value at 27 MHz. None until first.
    /// Indexed parallel to `config.programs`.
    pcr_last: Vec<Option<u64>>,

    // ── Stats counters ────────────────────────────────────────────────────
    ts_packets_emitted: u64,
    ts_bytes_emitted: u64,
    /// Keyed by PID. Populated eagerly at construction for every configured
    /// stream; only the `items` / `bytes` / `discontinuities` fields change
    /// at runtime. `pid` and `stream_type` are set at construction and
    /// never modified.
    per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats>,

    /// Per-PID codec-specific counters. Allocated lazily on first push
    /// for a PID whose StreamSpec falls into a counted family. Subtitle
    /// PIDs and audio PIDs with LATM/AC-3 codecs do NOT get an entry.
    stream_codec_counters: BTreeMap<u16, crate::mpegts::stats::StreamCodecCounters>,

    /// Scratch buffer reused across push_video / push_klv / push_audio /
    /// push_subtitle calls. Cleared and refilled each AU; avoids one
    /// per-AU heap allocation on the hot path. Grows to the largest AU seen
    /// and stays there — typical size is MAX_PES_HEADER_SIZE + payload.
    pes_scratch: Vec<u8>,
}

impl Muxer {
    /// Construct and validate.
    pub fn new(config: MuxerConfig) -> Result<Self, MuxError> {
        config.validate()?;

        let n_programs = config.programs.len();
        let pcr_interval_27mhz = (config.pcr_interval_ms as u64) * 27_000;
        let psi_interval_90khz = (config.psi_interval_ms as i64) * 90;

        // Per-program state vectors built in a single pass over programs.
        // The heavy lifting (stream-state collection, PCR PID resolution,
        // descriptor cache assembly, per-stream stats initialization) lives
        // in `state.rs` as `pub(super)` helpers so this constructor stays a
        // thin coordinator.
        let mut video_streams: Vec<Vec<VideoStreamState>> = Vec::with_capacity(n_programs);
        let mut klv_streams: Vec<Vec<KlvStreamState>> = Vec::with_capacity(n_programs);
        let mut audio_streams: Vec<Vec<AudioStreamState>> = Vec::with_capacity(n_programs);
        let mut subtitle_streams: Vec<Vec<SubtitleStreamState>> = Vec::with_capacity(n_programs);
        let mut data_streams: Vec<Vec<DataStreamState>> = Vec::with_capacity(n_programs);
        let mut pcr_pids: Vec<u16> = Vec::with_capacity(n_programs);
        let mut pmt_descriptor_caches: Vec<Vec<Vec<u8>>> = Vec::with_capacity(n_programs);
        let mut per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats> = BTreeMap::new();

        for prog in &config.programs {
            state::warn_on_descriptor_conflicts(prog);
            let (video, klv, audio, subtitle, data) = state::collect_stream_states(prog);
            let pcr_pid = state::resolve_pcr_pid(prog);
            let cache = state::build_pmt_descriptor_cache(prog);
            state::initialize_stats(
                prog,
                &video,
                &klv,
                &audio,
                &subtitle,
                &data,
                &mut per_stream,
            );

            video_streams.push(video);
            klv_streams.push(klv);
            audio_streams.push(audio);
            subtitle_streams.push(subtitle);
            data_streams.push(data);
            pcr_pids.push(pcr_pid);
            pmt_descriptor_caches.push(cache);
        }

        Ok(Self {
            config,
            pmt_descriptor_caches,
            video_streams,
            klv_streams,
            audio_streams,
            subtitle_streams,
            data_streams,
            pcr_pids,
            pcr_interval_27mhz,
            psi_interval_90khz,
            queue: VecDeque::with_capacity(64),
            counters: ContinuityCounters::new(),
            psi_last: vec![None; n_programs],
            pcr_last: vec![None; n_programs],
            ts_packets_emitted: 0,
            ts_bytes_emitted: 0,
            per_stream,
            stream_codec_counters: BTreeMap::new(),
            // 8 KiB heuristic: covers a typical small AU without reallocation;
            // grows automatically on first oversized frame and stays there.
            pes_scratch: Vec::with_capacity(MAX_PES_HEADER_SIZE + 8192),
        })
    }

    /// Drain ready TS packets into `out`.
    ///
    /// Returns the number of bytes written: 0 or a positive multiple of 188.
    /// `0` indicates either an empty queue or `out.len() < 188`. Pull is
    /// infallible — there are no failure modes that don't already surface
    /// at `push_video` / `push_klv` time (buffer-full, validation).
    ///
    /// # C ABI
    ///
    /// `tst_muxer_pull` — see `bindings/c/include/tstrans.h`.
    pub fn pull(&mut self, out: &mut [u8]) -> usize {
        if out.len() < 188 {
            return 0;
        }
        let max_packets = (out.len() / 188).min(self.queue.len());
        for i in 0..max_packets {
            let pkt = self.queue.pop_front().expect("checked count");
            out[i * 188..(i + 1) * 188].copy_from_slice(&pkt);
        }
        let n = max_packets * 188;
        self.ts_packets_emitted += max_packets as u64;
        self.ts_bytes_emitted += n as u64;
        n
    }

    /// Number of 188-byte TS packets currently queued in the muxer's
    /// internal output buffer awaiting [`Muxer::pull`]. This is a live
    /// gauge — non-zero between a `push_*` call and the subsequent
    /// drain. Compared against [`Muxer::capacity_packets`] this gives
    /// the back-pressure ratio used by `tst_pipeline::MuxSender`'s
    /// observability hooks.
    pub fn pending_packets(&self) -> u64 {
        self.queue.len() as u64
    }

    /// The configured queue capacity in 188-byte TS packets — a snapshot
    /// of `MuxerConfig::buffer_packets`. A `push_*` that would push the
    /// queue past this cap returns [`crate::error::MuxError::BufferFull`].
    #[must_use]
    pub fn capacity_packets(&self) -> u64 {
        self.config.buffer_packets as u64
    }

    /// Force PAT + PMT re-emission on the next push (all programs).
    ///
    /// Segmenting shells (e.g. `MuxPublisher` over an HLS publisher) call this
    /// after cutting a segment so the next segment opens with PSI and is
    /// independently decodable (RFC 8216 §3: each Media Segment must be
    /// decodable without prior segments).
    pub fn request_psi(&mut self) {
        for slot in &mut self.psi_last {
            *slot = None;
        }
    }

    /// Return the resolved PCR PID for program at `prog_idx` (0-based index).
    /// Returns `None` if `prog_idx` is out of range.
    #[cfg(test)]
    pub(crate) fn pcr_pid_for_program(&self, prog_idx: usize) -> Option<u16> {
        self.pcr_pids.get(prog_idx).copied()
    }

    // ── Shared push-path helpers ─────────────────────────────────────────────

    /// Check buffer headroom, then emit any pending PSI and PCR-only packets.
    ///
    /// All five `push_*` paths repeat the same three-step preamble:
    /// 1. Count packets needed for this push (PSI + PCR-only + payload).
    /// 2. Reject early with `BufferFull` if they don't fit.
    /// 3. Emit the PSI and PCR-only packets that are due.
    ///
    /// `pid` is the elementary-stream PID being pushed; `payload_pkts` is
    /// the TS-packet count for the PES payload (from `ts_packets_for`).
    pub(super) fn reserve_preamble(
        &mut self,
        prog_idx: usize,
        pts: crate::mpegts::common::Pts90khz,
        pid: u16,
        payload_pkts: usize,
    ) -> Result<(), MuxError> {
        // Packet counts mirror what maybe_emit_psi / maybe_emit_pcr_only
        // below actually emit: 1 PAT + N PMTs when the PSI tick is due
        // (the historical hardcoded `2` under-reserved with N >= 2
        // programs and let queue overflows slip past the check below),
        // plus one PCR-only adaptation-field packet when the PCR PID has
        // fallen behind and this push isn't already landing on it.
        let psi_packets = if self.psi_due(prog_idx, pts.as_ticks()) {
            1 + self.config.programs.len()
        } else {
            0
        };
        let pcr_only_packets = usize::from(self.pcr_only_due(prog_idx, pts.as_ticks(), pid));
        if self.queue.len() + psi_packets + pcr_only_packets + payload_pkts
            > self.config.buffer_packets
        {
            return Err(MuxError::BufferFull {
                capacity_packets: self.config.buffer_packets as u64,
            });
        }
        self.maybe_emit_psi(prog_idx, pts.as_ticks());
        self.maybe_emit_pcr_only(prog_idx, pts.as_ticks(), pid);
        Ok(())
    }

    /// Compute the `AdaptationField` for the first TS packet of a push:
    /// carries PCR (and updates `pcr_last[prog_idx]`) when `pid` is the
    /// program's PCR PID and a PCR tick is due; a default (no-PCR) field
    /// otherwise. Shared by `push_audio_to` / `push_klv_to`.
    ///
    /// `push_video_to` keeps its own copy — it also sets `random_access`
    /// from `key_frame` and force-emits PCR on a coincident key frame
    /// even when `pcr_due` would otherwise say no, a different rule than
    /// the plain due-check here.
    pub(super) fn pcr_first_af(
        &mut self,
        prog_idx: usize,
        pts: crate::mpegts::common::Pts90khz,
        pid: u16,
    ) -> self::ts::AdaptationField {
        if self.pcr_pids[prog_idx] == pid && self.pcr_due(prog_idx, pts.as_ticks()) {
            let pcr = crate::mpegts::common::Pcr27mhz::from_pts(pts);
            self.pcr_last[prog_idx] = Some(pcr.as_ticks());
            self::ts::AdaptationField {
                pcr: Some(pcr),
                random_access: false,
            }
        } else {
            self::ts::AdaptationField::default()
        }
    }

    /// Packetize `self.pes_scratch` into 188-byte TS packets on `pid`.
    ///
    /// `first_af` is the caller-computed `AdaptationField` for the FIRST
    /// packet — it may carry PCR, `random_access_indicator`, or both.
    /// All subsequent packets use `AdaptationField::default()`.
    ///
    /// The split borrows (`&self.pes_scratch[cursor..]` + `&mut self.counters`
    /// + `self.queue`) are on disjoint fields; NLL handles this correctly
    ///   within the method body.
    pub(super) fn drain_pes_scratch(&mut self, pid: u16, first_af: self::ts::AdaptationField) {
        let mut cursor = 0;
        let mut first = true;
        while cursor < self.pes_scratch.len() {
            let adaptation = if first {
                first_af
            } else {
                self::ts::AdaptationField::default()
            };
            let mut pkt = [0u8; 188];
            let result = self::ts::write_packet(
                &mut pkt,
                pid,
                first,
                adaptation,
                &self.pes_scratch[cursor..],
                &mut self.counters,
            );
            cursor += result.payload_consumed;
            self.queue.push_back(pkt);
            first = false;
        }
    }
}

// ── Free helpers shared by push_*.rs submodules ───────────────────────────

/// Collect all stream handles across every program, in `(program, within-program)`
/// declaration order.
///
/// `streams` is the per-program, per-stream-kind field (e.g. `self.audio_streams`).
/// `pack` is the handle constructor (e.g. `AudioStreamHandle::pack`).
///
/// All five `push_*.rs` files expose a `*_handles(&self)` method with an
/// identical flat_map body; routing them here eliminates the duplication.
fn all_handles<T, H>(streams: &[Vec<T>], pack: fn(usize, usize) -> H) -> Vec<H> {
    streams
        .iter()
        .enumerate()
        .flat_map(|(p_idx, prog)| (0..prog.len()).map(move |s_idx| pack(p_idx, s_idx)))
        .collect()
}

/// Stream handles for the named program, in declaration order.
///
/// `programs` is `self.config.programs` (to resolve `program_number` to an
/// index); `streams` and `pack` are as in [`all_handles`]. Shared body of
/// the five `*_handles_for_program` methods in the `push_*.rs` submodules.
///
/// Returns `Err(MuxError::ProgramNotFound)` if no program with the given
/// number exists.
fn handles_for_program<T, H>(
    programs: &[MuxerProgramConfig],
    streams: &[Vec<T>],
    program_number: u16,
    pack: fn(usize, usize) -> H,
) -> Result<Vec<H>, crate::error::MuxError> {
    let prog_idx = programs
        .iter()
        .position(|p| p.program_number == program_number)
        .ok_or(crate::error::MuxError::ProgramNotFound { program_number })?;
    Ok((0..streams[prog_idx].len())
        .map(|s_idx| pack(prog_idx, s_idx))
        .collect())
}

/// Handle for the i-th stream in `streams[0]` (the first program's
/// declaration order), or `None` if `streams` has no programs or `index`
/// is out of range. Shared body of `video_stream_handle` /
/// `klv_stream_handle` / `data_stream_handle` — convenience lookups for
/// single-program callers.
fn first_program_handle<T, H>(
    streams: &[Vec<T>],
    index: usize,
    pack: fn(usize, usize) -> H,
) -> Option<H> {
    if !streams.is_empty() && index < streams[0].len() {
        Some(pack(0, index))
    } else {
        None
    }
}

/// Return the program index of the sole non-empty stream list in `streams`.
///
/// Precondition: caller has verified that exactly one program has a non-empty
/// list for this stream kind. The `expect` is safe under that invariant and
/// will only fire if the caller's precondition check was wrong.
///
/// Replaces identical `.iter().enumerate().find(…).map(…).expect(…)` blocks
/// in `single_video_handle` and [`resolve_lone`].
fn locate_lone_program<T>(streams: &[Vec<T>]) -> usize {
    streams
        .iter()
        .enumerate()
        .find(|(_, s)| !s.is_empty())
        .map(|(p, _)| p)
        .expect("caller verified exactly one non-empty program for this stream kind")
}

/// Resolve the single-stream handle for a `push_*` shorthand entry point:
/// `none_configured` when no streams of this kind are configured,
/// `MuxError::AmbiguousTarget` when more than one is, otherwise the lone
/// stream's handle. Shared 3-step body of `push_audio` / `push_klv` /
/// `push_subtitle` / `push_data`'s shorthand methods.
///
/// `push_video`'s shorthand keeps its own copy — it maps BOTH the zero-
/// and ambiguous-count cases to `AmbiguousTarget` (no `NoVideoStreamsConfigured`
/// variant exists), which doesn't fit this shape.
fn resolve_lone<T, H>(
    streams: &[Vec<T>],
    none_configured: MuxError,
    kind: StreamKind,
    pack: fn(usize, usize) -> H,
) -> Result<H, MuxError> {
    let total: usize = streams.iter().map(|s| s.len()).sum();
    if total == 0 {
        return Err(none_configured);
    }
    if total > 1 {
        return Err(MuxError::AmbiguousTarget { kind, count: total });
    }
    Ok(pack(locate_lone_program(streams), 0))
}

// ── Tests ─────────────────────────────────────────────────────────────────
//
// The 2500+ LoC of inline test code is split into focused files under
// `tests/`. Each file is declared as a direct child of this module via
// `#[path]` so that `use super::*` inside each file resolves against
// `mpegts::mux` — the same scope the original inline blocks had.
//
// DO NOT add a `mod tests { ... }` wrapper here. The `#[path]` attribute
// makes each file a DIRECT child of `mux`, preserving the same
// `use super::*` scope that the inline tests relied on (including
// `pub(super)` helpers like `validate_language_code` from `config.rs`).

#[cfg(test)]
#[path = "tests/config.rs"]
mod tests_config;

#[cfg(test)]
#[path = "tests/handles.rs"]
mod tests_handles;

#[cfg(test)]
#[path = "tests/push.rs"]
mod tests_push;

#[cfg(test)]
#[path = "tests/subtitle.rs"]
mod tests_subtitle;

#[cfg(test)]
#[path = "tests/validation.rs"]
mod tests_validation;

#[cfg(test)]
#[path = "tests/stats.rs"]
mod tests_stats;
