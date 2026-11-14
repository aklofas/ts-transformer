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

mod slot;
mod state;
mod scheduling;
mod stats_accounting;
mod push_audio;
mod push_data;
mod push_klv;
mod push_subtitle;
mod push_video;
mod reconfigure;

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
use self::slot::ProgramSlot;
use self::ts::ContinuityCounters;

/// MuxSender-side MPEG-TS muxer.
///
/// Construct with `Muxer::new(config)`, push encoded frames via `push_video`
/// and `push_klv`, then drain TS packets with `pull`. The muxer is
/// deterministic — output is a function of inputs only, not wall-clock time.
/// [`Self::reconfigure`] replaces the program list of a running muxer in
/// place.
///
/// # Clock
///
/// Each program's PCR runs [`MuxerConfig::pcr_lead_ms`] behind the decode
/// time of the access units pushed on its PCR PID and never steps back. A
/// PCR is written when a push finds one due, so a PCR stream below ten
/// access units a second (or a faster one with `pcr_interval_ms` above 50)
/// needs [`Self::tick`] between pushes to keep the 100 ms PCR interval.
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
    /// Read only for the global fields (`pcr_interval_ms`, `pcr_lead_ms`,
    /// `psi_interval_ms`, `buffer_packets`, `av1_carriage`); `slots` is the source of truth for
    /// the program list, so nothing reads `config.programs`.
    config: MuxerConfig,

    /// Per-program state. `program_index` of a stream handle is the slot
    /// index; a tombstoned slot keeps its index so later handles stay valid.
    slots: Vec<ProgramSlot>,

    /// Slot index of every live program in the current configuration's
    /// declaration order. A slot index is a handle's program index; this is
    /// the order the handle accessors report.
    program_order: Vec<usize>,

    pcr_interval_90khz: i64,
    pcr_lead_90khz: u64,
    psi_interval_90khz: i64,

    queue: VecDeque<[u8; 188]>,
    counters: ContinuityCounters,

    /// Version of the PAT last rendered.
    pat_version: u8,
    /// `pat_entries()` as of the last PAT version, to detect a changed loop.
    pat_identity: Vec<(u16, u16)>,
    /// PMT PID -> last version used on it; survives a program's removal so
    /// a PID reused later continues the sequence.
    pmt_versions_by_pid: BTreeMap<u16, u8>,

    // ── Stats counters ────────────────────────────────────────────────────
    ts_packets_emitted: u64,
    ts_bytes_emitted: u64,
    late_metadata_pushes: u64,
    /// Keyed by PID. Populated eagerly at construction for every configured
    /// stream; only the `items` / `bytes` / `discontinuities` fields change
    /// at runtime, except that `reconfigure` adds entries for new PIDs and
    /// rewrites the identity fields of a PID placed again. Entries are never
    /// removed.
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

        let pcr_interval_90khz = (config.pcr_interval_ms as i64) * 90;
        let pcr_lead_90khz = (config.pcr_lead_ms as u64) * 90;
        let psi_interval_90khz = (config.psi_interval_ms as i64) * 90;

        // One slot per program, in declaration order. The heavy lifting
        // (stream-state collection, PCR PID resolution, descriptor cache
        // assembly, per-stream stats initialization) lives in `slot.rs` and
        // `state.rs` so this constructor stays a thin coordinator.
        let mut slots: Vec<ProgramSlot> = Vec::with_capacity(config.programs.len());
        let mut per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats> = BTreeMap::new();

        for prog in &config.programs {
            let slot = ProgramSlot::from_program(prog);
            state::initialize_stats(
                prog,
                &slot.video,
                &slot.klv,
                &slot.audio,
                &slot.subtitle,
                &slot.data,
                &mut per_stream,
            );
            slots.push(slot);
        }

        let mut pmt_versions_by_pid = BTreeMap::new();
        for slot in &slots {
            if let Some(p) = &slot.live {
                pmt_versions_by_pid.insert(p.pmt_pid, 0);
            }
        }
        let pat_identity = slots
            .iter()
            .filter_map(|s| s.live.as_ref())
            .map(|p| (p.program_number, p.pmt_pid))
            .collect();

        let program_order = (0..config.programs.len()).collect();
        Ok(Self {
            config,
            slots,
            program_order,
            pat_version: 0,
            pat_identity,
            pmt_versions_by_pid,
            pcr_interval_90khz,
            pcr_lead_90khz,
            psi_interval_90khz,
            queue: VecDeque::with_capacity(64),
            counters: ContinuityCounters::new(),
            ts_packets_emitted: 0,
            ts_bytes_emitted: 0,
            late_metadata_pushes: 0,
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
        for slot in &mut self.slots {
            slot.psi_last = None;
        }
    }

    /// Return the resolved PCR PID for program at `prog_idx` (0-based index).
    /// Returns `None` if `prog_idx` is out of range.
    #[cfg(test)]
    pub(crate) fn pcr_pid_for_program(&self, prog_idx: usize) -> Option<u16> {
        self.slots.get(prog_idx).map(|s| s.pcr_pid)
    }

    /// Live slots with their indices, in slot order.
    fn live_slots(&self) -> impl Iterator<Item = (usize, &ProgramSlot)> {
        self.slots.iter().enumerate().filter(|(_, s)| s.is_live())
    }

    /// `(program_number, pmt_pid)` of every live program, in slot order —
    /// the PAT's program loop.
    fn pat_entries(&self) -> Vec<(u16, u16)> {
        self.slots
            .iter()
            .filter_map(|s| s.live.as_ref())
            .map(|p| (p.program_number, p.pmt_pid))
            .collect()
    }

    // ── Shared push-path helpers ─────────────────────────────────────────────

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

/// Collect the handles of every live stream in every live program, in
/// `(program, within-program)` declaration order. The program order comes
/// from the current configuration (`program_order`), not the slot table: a
/// reconfigure can leave a program on any slot.
///
/// `kind` selects the stream kind. `pack` is the handle constructor (e.g.
/// `AudioStreamHandle::pack`).
///
/// All five `push_*.rs` files expose a `*_handles(&self)` method with an
/// identical body; routing them here eliminates the duplication.
fn all_handles<H>(
    slots: &[ProgramSlot],
    program_order: &[usize],
    kind: MuxStreamKind,
    pack: fn(usize, usize) -> H,
) -> Vec<H> {
    program_order
        .iter()
        .flat_map(|&p_idx| {
            declared_within_indices(&slots[p_idx], kind).map(move |w| pack(p_idx, w))
        })
        .collect()
}

/// Within-indices of a live slot's `kind` streams in the program's
/// declaration order; empty for a tombstoned slot.
///
/// After a reconfigure a stream's within-index need not follow declaration
/// order (a new stream may take a lower tombstoned index), so the order
/// comes from `live.streams` and each PID maps to its index through
/// `placed_pids`.
fn declared_within_indices(
    slot: &ProgramSlot,
    kind: MuxStreamKind,
) -> impl Iterator<Item = usize> + '_ {
    slot.live
        .iter()
        .flat_map(|prog| prog.streams.iter())
        .filter_map(move |spec| match slot.placed_pids.get(&spec.pid()) {
            Some(&(placed, within)) if placed == kind => Some(within),
            _ => None,
        })
}

/// Live stream handles for the named live program, in declaration order.
///
/// `kind` and `pack` are as in [`all_handles`]. Shared body of the five
/// `*_handles_for_program` methods in the `push_*.rs` submodules.
///
/// Returns `Err(MuxError::ProgramNotFound)` if no live program with the given
/// number exists.
fn handles_for_program<H>(
    slots: &[ProgramSlot],
    kind: MuxStreamKind,
    program_number: u16,
    pack: fn(usize, usize) -> H,
) -> Result<Vec<H>, crate::error::MuxError> {
    let prog_idx = slots
        .iter()
        .position(|s| s.program_number() == Some(program_number))
        .ok_or(crate::error::MuxError::ProgramNotFound { program_number })?;
    Ok(declared_within_indices(&slots[prog_idx], kind)
        .map(|w| pack(prog_idx, w))
        .collect())
}

/// Handle for the i-th declared stream of `kind` in the first program of the
/// current configuration, or `None` if there is no live program or that
/// stream does not exist.
/// Shared body of `video_stream_handle` / `klv_stream_handle` /
/// `data_stream_handle` — convenience lookups for single-program callers.
fn first_program_handle<H>(
    slots: &[ProgramSlot],
    program_order: &[usize],
    kind: MuxStreamKind,
    index: usize,
    pack: fn(usize, usize) -> H,
) -> Option<H> {
    let p_idx = *program_order.first()?;
    declared_within_indices(&slots[p_idx], kind)
        .nth(index)
        .map(|w| pack(p_idx, w))
}

/// Number of live streams of one kind across all live programs.
fn live_stream_count<T>(slots: &[ProgramSlot], streams: fn(&ProgramSlot) -> &[Option<T>]) -> usize {
    slots
        .iter()
        .filter(|slot| slot.is_live())
        .map(|slot| streams(slot).iter().filter(|s| s.is_some()).count())
        .sum()
}

/// Return the `(program_index, within_index)` of the first live stream of
/// one kind.
///
/// Precondition: caller has verified that exactly one live stream of this
/// kind exists. The `expect` is safe under that invariant and will only
/// fire if the caller's precondition check was wrong.
///
/// Replaces identical lookup blocks in `single_video_handle` and
/// [`resolve_lone`].
fn locate_lone_stream<T>(
    slots: &[ProgramSlot],
    streams: fn(&ProgramSlot) -> &[Option<T>],
) -> (usize, usize) {
    slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_live())
        .find_map(|(p, slot)| {
            streams(slot)
                .iter()
                .position(Option::is_some)
                .map(|w| (p, w))
        })
        .expect("caller verified exactly one live stream of this kind")
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
    slots: &[ProgramSlot],
    streams: fn(&ProgramSlot) -> &[Option<T>],
    none_configured: MuxError,
    kind: MuxStreamKind,
    pack: fn(usize, usize) -> H,
) -> Result<H, MuxError> {
    let total = live_stream_count(slots, streams);
    if total == 0 {
        return Err(none_configured);
    }
    if total > 1 {
        return Err(MuxError::AmbiguousTarget { kind, count: total });
    }
    let (p, w) = locate_lone_stream(slots, streams);
    Ok(pack(p, w))
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

#[cfg(test)]
#[path = "tests/reconfigure.rs"]
mod tests_reconfigure;
