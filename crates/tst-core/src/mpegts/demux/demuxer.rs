//! Top-level `Demuxer` state machine — the coordinator that wires
//! together the sibling-submodule helpers extracted during Wave 6.B:
//!
//! - `sync_ingress` — byte-aligned 188-byte packet detection + PCR / CC
//!   anomaly checks.
//! - `psi_topology` — PSI section dispatch + PAT/PMT topology tracking.
//! - `pmt_classify` — PMT stream classification + descriptor recognition.
//! - `pes_emit` — PES reassembly dispatch + event construction.
//! - `stats_recorder` — counter bumping + nonconformant event queueing.
//! - `strict` (unchanged from Phase 5) — `StrictMode` policy enum.
//!
//! Public API (`new`, `with_config`, `feed`, `feed_aligned`,
//! `next_event`, `flush`, `stats`, `reset_stats`, `stream_codec_stats`)
//! lives here in the coordinator. Implementation helpers are
//! `pub(super)` and live in the sibling submodules per Decision DB2/DB3.

use crate::error::DemuxError;
use crate::mpegts::demux::event::{
    AdaptationFieldKind, DemuxEvent, NonConformantIssue, StreamId, StreamKind,
};
use crate::mpegts::demux::pes::Reassembler;
use crate::mpegts::demux::psi_assembler::PsiSectionAssembler;
use crate::mpegts::demux::ts::{TsParseError, parse_ts_packet};
use crate::mpegts::demux::types::{
    DEFAULT_AU_CELL_CAP_PER_PID, DEFAULT_AU_CELL_CAP_TOTAL, DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS,
    DEFAULT_PES_CAP_PER_PID, DEFAULT_PES_CAP_TOTAL, DemuxerConfig, DemuxerStats, ProgramTracker,
};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

/// Per-PID PTS/DTS unwrap accumulator state, in raw 90 kHz ticks.
///
/// Holds the previous sample's raw wire value alongside the unwrapped
/// value that was emitted for it. Each new sample accumulates its signed
/// wrap-aware delta (via [`crate::mpegts::common::pts_diff_33bit`]) onto
/// `last_unwrapped`, so the epoch a sample lands in is decided by how far
/// it actually is from its predecessor — never by a latched offset that a
/// later out-of-order arrival would inherit.
///
/// The "offset" a secondary timestamp needs is derived on demand as
/// `last_unwrapped - last_raw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UnwrapState {
    /// The previous sample's raw 33-bit wire value.
    pub(super) last_raw: i64,
    /// The unwrapped value emitted for that same sample.
    pub(super) last_unwrapped: i64,
}

/// MPEG-TS demuxer.
///
/// Caller-driven: call [`Self::feed`] with bytes (any size; sync recovery
/// is internal), then drain [`DemuxEvent`]s with [`Self::next_event`].
/// Holds bounded reassembly state per PID with caps from
/// [`DemuxerConfig`].
///
/// # Closing
///
/// `Demuxer` is a passive parser — it owns no transport and no OS
/// handles. Drop is the only shutdown and is trivially synchronous.
/// Call [`Self::flush`] at end-of-stream to surface any partial PES
/// still buffered (e.g. the final video AU whose PES length is 0 and
/// is only finalised on the next PUSI), then drain remaining events
/// via [`Self::next_event`] before drop.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `demuxer.flush(); while let Some(e) = demuxer.next_event() { /* ... */ } drop(demuxer);` |
/// | Java | Drain via `flush()` + `nextEvent()`, then let GC reclaim |
/// | Kotlin | Drain via `flush()` + `nextEvent()`, then let GC reclaim |
/// | Swift | `deinit` calls drop; explicit `flush()` + drain before exit |
/// | Python | `demuxer.flush()` + drain at end-of-stream; let GC reclaim |
/// | C (transport) | `tst_demux_receiver_recv_event(p, &out_event)` drains into an arena-lifetime `tst_event_t`; `tst_demux_receiver_close(p)` releases the handle |
/// | C (offline) | `tst_demuxer_feed(d, buf, n)` + `tst_demuxer_flush(d)`, then drain with `tst_demuxer_next_event` until `TST_E_NOT_AVAILABLE`, then `tst_demuxer_close(d)` |
#[derive(Debug)]
pub struct Demuxer {
    pub(super) options: DemuxerConfig,
    /// Bytes that haven't yet been sync-aligned into 188-byte packets.
    /// `sync_consumed` is the cursor into this buffer; the live region is
    /// `sync_buf[sync_consumed..]`. Avoiding `drain(..n)` per packet is
    /// what keeps `feed` amortized-linear on whole-file inputs (a naive
    /// drain is O(remaining) per call → O(N²) total).
    pub(super) sync_buf: Vec<u8>,
    /// Cursor into `sync_buf`; bytes before this index are consumed and
    /// will be reclaimed on the next compaction.
    pub(super) sync_consumed: usize,
    /// Per-PID PSI assembly state (PAT + any active PMT PIDs). Each
    /// assembler enforces the 4 KiB `MAX_SECTION_SIZE` cap and yields a
    /// complete section once `section_length + 3` bytes have been
    /// accumulated for that PID. See `psi_assembler.rs`.
    pub(super) psi_assemblers: HashMap<u16, PsiSectionAssembler>,
    /// Programs found in the current PAT, keyed by `pmt_pid`.
    /// O(1) lookup when routing PMT-bound packets.
    pub(super) programs: HashMap<u16, ProgramTracker>,
    /// Latest PAT version. Bump triggers PAT diff (programs added/removed).
    pub(super) pat_version: Option<u8>,
    /// Per-PID stream kind cache for PES dispatch. Flat across all programs
    /// (PIDs must be unique cross-program per ISO 13818-1).
    pub(super) stream_kind_by_pid: HashMap<u16, StreamKind>,
    pub(super) cc_by_pid: HashMap<u16, u8>,
    /// Per-PID duplicate-packet tracking for the spec-legal one-extra rule
    /// (H.222.0 §2.4.3.3: a packet may be transmitted exactly twice with the
    /// same continuity_counter). A PID in this set had its immediately
    /// preceding payload-bearing packet classified as a spec-legal duplicate
    /// (same CC, no discontinuity_indicator). If the SAME CC appears a
    /// THIRD time on that PID, that IS a discontinuity. The set is cleared
    /// per-PID whenever the PID advances its CC normally, or when per-PID
    /// state is dropped (PAT/PMT topology teardown, `reset_sync`).
    pub(super) dup_by_pid: HashSet<u16>,
    /// Raw bytes of the last routed payload-bearing packet per PID, retained
    /// so duplicate detection can byte-compare a same-CC packet against its
    /// predecessor (H.222.0 §2.4.3.3 requires duplicates to be bit-identical
    /// apart from a refreshed PCR — see `pcr_masked_identical`). Lifecycle
    /// mirrors `cc_by_pid` exactly: inserted on every routed payload packet,
    /// removed/cleared wherever `cc_by_pid` is. Bounded like the sibling
    /// per-PID maps (worst case 8191 PIDs × 188 B ≈ 1.5 MB on hostile input).
    pub(super) last_pkt_raw_by_pid: HashMap<u16, [u8; 188]>,
    /// Captured (expected, observed) CC pair when `check_continuity`
    /// flagged a real jump on the packet currently being routed. Drained
    /// by `handle_psi` when it consumes the strict-mode drop arm; cleared
    /// at the top of every `check_continuity` call so PSI packets without
    /// a jump don't carry stale state.
    pub(super) last_psi_cc_jump: Option<(u8, u8)>,
    /// Per-PCR-PID last-seen 27 MHz PCR. Keyed by the PID of the packet
    /// carrying the PCR (each program advertises its own PCR PID in its
    /// PMT; per ITU-T H.222.0 §2.4.3.5 each program has its own time base,
    /// so PCR comparison MUST stay within a single PID's timeline).
    pub(super) last_pcr_by_pid: HashMap<u16, u64>,
    pub(super) last_pts_by_pid: HashMap<u16, i64>,
    /// Per-PID PTS/DTS unwrap accumulator — see [`UnwrapState`].
    /// Populated and consulted only when
    /// [`DemuxerConfig::unwrap_timestamps`] is `true`; stays empty (and
    /// inert) otherwise. See `pes_emit.rs::unwrap_pts` /
    /// `unwrap_dts_with_pts` / `unwrap_secondary_ts`. Cleared on
    /// [`Self::reset_sync`] alongside `last_pts_by_pid`.
    pub(super) unwrap_state: HashMap<u16, UnwrapState>,
    pub(super) pes: Reassembler,
    pub(super) queue: VecDeque<DemuxEvent>,
    pub(super) bytes_since_sync: usize,
    /// `true` once the demuxer has acquired a confirmed packet boundary
    /// (a candidate 0x47 that satisfies the N-of-M stride check, OR a
    /// successful steady-state parse continuation). Cleared whenever
    /// sync is lost (`live[0] != 0x47`) so the next candidate is
    /// re-validated. See `SYNC_REACQ_N` / `SYNC_REACQ_M` constants for
    /// the rationale (ffmpeg `mpegts.c::mpegts_resync` semantics).
    pub(super) is_synced: bool,
    /// First strict-mode-rejected issue captured this `feed` call. Drained
    /// at the end of each packet's processing and converted into a
    /// `DemuxError::StrictRejection` return. The `NonConformant` event
    /// itself is still pushed onto `queue` so a caller that already
    /// drained events sees the rejection narrative if they wish.
    pub(super) fatal: Option<NonConformantIssue>,
    // ── stats counters ──────────────────────────────────────────────────
    pub(super) program_maps_seen: u64,
    pub(super) pmt_versions_seen: u64,
    pub(super) discontinuities_count: u64,
    pub(super) nonconformant_count: u64,
    pub(super) subtitle_streams_seen_count: u32,
    /// Per-PID counters; entries created lazily on first event per PID.
    pub(super) stats_per_stream: BTreeMap<u16, crate::mpegts::stats::StreamStats>,
    /// Per-PID codec-specific counters. Allocated lazily on first event
    /// for a PID whose stream_type falls into a counted family. PSI /
    /// subtitle / LATM / AC-3 PIDs do NOT get an entry — they live only
    /// in `stats_per_stream`.
    pub(super) stream_codec_counters: BTreeMap<u16, crate::mpegts::stats::StreamCodecCounters>,
    /// PIDs that have already emitted `SubtitleMissingDescriptor` for the
    /// current PMT version. Cleared at the top of each PMT-version bump so
    /// a fresh PMT re-fires if the descriptor is still missing.
    pub(super) subtitle_missing_descriptor_emitted: HashSet<u16>,
    /// PIDs the demuxer has emitted at least one `SamplePayload::Subtitle`
    /// event for. Used to dedupe `subtitle_streams_seen` increments so
    /// repeat samples on the same PID don't double-count. Cleared on
    /// `reset_stats`.
    pub(super) subtitle_pids_seen: HashSet<u16>,
    /// PIDs that have already emitted `Av1RegistrationMalformed` for the
    /// current PMT version. Cleared at the top of each PMT-version bump so
    /// a fresh PMT re-fires if the malformed registration is still present.
    pub(super) av1_registration_malformed_emitted: HashSet<u16>,
    /// PIDs that have already emitted `SubtitleDescriptorAmbiguous` for the
    /// current PMT version. Cleared at the top of each PMT-version bump so
    /// a fresh PMT re-fires if the ambiguity is still present.
    pub(super) subtitle_descriptor_ambiguous_emitted: HashSet<u16>,
    /// PID → program_number lookup. Populated when a PMT is parsed;
    /// entries are removed when the PAT drops a program. Replaces the
    /// O(programs × streams) linear scan in `program_number_for_pid` that
    /// ran at every event-emitting callsite.
    pub(super) pid_to_program: HashMap<u16, u16>,
    /// Per-PID Metadata AU cell reassembler. Accumulates fragmented sync-
    /// metadata cells (H.222.0 §2.12.4.2 First/Middle/Last) into complete
    /// AUs. Single-cell (`Complete`) AUs pass through unchanged. Cleared
    /// wholesale on [`Self::reset_sync`] and on PMT version change.
    pub(super) au_reassembler: crate::mpegts::demux::au_reassemble::AuCellReassembler,
    /// Multi-section PAT reassembler (REF-PSI-02). Buffers sections of one
    /// PAT table by `(tsid, version, current_next)` key and fires atomically
    /// on a complete `0..=last_section_number` set. Cleared on
    /// [`Self::reset_sync`]. `pub(super)` — invisible outside `mpegts::demux`.
    pub(super) pat_reassembler: crate::mpegts::demux::pat_reassemble::PatReassembler,
}

impl Demuxer {
    /// Create a demuxer with the default [`DemuxerConfig`].
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_open` — see `bindings/c/include/tstrans.h`.
    pub fn new() -> Self {
        Self::with_config(DemuxerConfig::default())
    }

    /// Create a demuxer with an explicit [`DemuxerConfig`].
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_open_with_config` — see `bindings/c/include/tstrans.h`.
    pub fn with_config(config: DemuxerConfig) -> Self {
        let cap_per_pid = config.pes_cap_per_pid.unwrap_or(DEFAULT_PES_CAP_PER_PID);
        let cap_total = config.pes_cap_total.unwrap_or(DEFAULT_PES_CAP_TOTAL);
        let au_cap = config
            .au_cell_cap_per_pid
            .unwrap_or(DEFAULT_AU_CELL_CAP_PER_PID);
        let au_cap_total = config
            .au_cell_cap_total
            .unwrap_or(DEFAULT_AU_CELL_CAP_TOTAL);
        let au_max_pids = config
            .au_cell_max_in_flight_pids
            .unwrap_or(DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS);
        // Seed the PAT PID (0x0000) so the PSI assembler is ready without a
        // separate "first packet" initialisation step.
        let mut psi_assemblers: HashMap<u16, PsiSectionAssembler> = HashMap::new();
        psi_assemblers.insert(0x0000, PsiSectionAssembler::new());
        Self {
            options: config,
            sync_buf: Vec::new(),
            sync_consumed: 0,
            psi_assemblers,
            programs: HashMap::new(),
            pat_version: None,
            stream_kind_by_pid: HashMap::new(),
            cc_by_pid: HashMap::new(),
            dup_by_pid: HashSet::new(),
            last_pkt_raw_by_pid: HashMap::new(),
            last_psi_cc_jump: None,
            last_pcr_by_pid: HashMap::new(),
            last_pts_by_pid: HashMap::new(),
            unwrap_state: HashMap::new(),
            pes: Reassembler::new(cap_per_pid, cap_total),
            queue: VecDeque::new(),
            bytes_since_sync: 0,
            is_synced: false,
            fatal: None,
            program_maps_seen: 0,
            pmt_versions_seen: 0,
            discontinuities_count: 0,
            nonconformant_count: 0,
            subtitle_streams_seen_count: 0,
            stats_per_stream: BTreeMap::new(),
            stream_codec_counters: BTreeMap::new(),
            subtitle_missing_descriptor_emitted: HashSet::new(),
            subtitle_pids_seen: HashSet::new(),
            av1_registration_malformed_emitted: HashSet::new(),
            subtitle_descriptor_ambiguous_emitted: HashSet::new(),
            pid_to_program: HashMap::new(),
            au_reassembler: crate::mpegts::demux::au_reassemble::AuCellReassembler::with_limits(
                au_cap,
                au_cap_total,
                au_max_pids,
            ),
            pat_reassembler: crate::mpegts::demux::pat_reassemble::PatReassembler::default(),
        }
    }

    /// Feed bytes into the demuxer. Bytes need not be 188-aligned; the
    /// demuxer handles TS sync recovery internally.
    ///
    /// When `feed` returns `Err(DemuxError::StrictRejection(_))`, the
    /// corresponding `NonConformant` event has already been pushed onto the
    /// internal queue. Drain `next_event()` after the error to retrieve the
    /// structured issue alongside the human-readable error string.
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_feed` — see `bindings/c/include/tstrans.h`.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), DemuxError> {
        // Enforce the hard ceiling BEFORE copying the caller's bytes. The
        // inner sync-search-window check below only fires per loop
        // iteration; if we extended first, a single oversized adversarial
        // feed would already have allocated (and copied) the entire input
        // — potentially OOMing the host — before any check could bail.
        // Compare the projected post-extend length against the ceiling
        // using checked arithmetic so a pathological `bytes.len()` near
        // `usize::MAX` can't wrap.
        let cap = self
            .options
            .sync_buf_cap
            .unwrap_or(super::sync_ingress::MAX_SYNC_BUF_BYTES);
        let projected = self.sync_buf.len().checked_add(bytes.len());
        if projected.is_none_or(|n| n > cap) {
            // Report what the total would have been (saturating, so the
            // checked-add-overflow case still yields a meaningful figure).
            let observed = self.sync_buf.len().saturating_add(bytes.len());
            // Defensive: once the cap is exceeded, the parser is in a known-bad
            // state and we should release the buffered bytes. Subsequent
            // feed calls will start from an empty buffer; if the peer is still
            // hostile, they'll trip the cap again. The caller's only sane
            // response is to teardown the demuxer. The incoming `bytes` are
            // dropped without ever being copied in.
            self.sync_buf.clear();
            self.sync_consumed = 0;
            self.is_synced = false;
            return Err(DemuxError::SyncBufExhausted { observed, max: cap });
        }
        // Reserve up front so a partial copy can't leave the buffer in a
        // half-grown state, and surface an allocation failure as a clean
        // error rather than aborting on a panic.
        if self.sync_buf.try_reserve(bytes.len()).is_err() {
            let observed = self.sync_buf.len().saturating_add(bytes.len());
            self.sync_buf.clear();
            self.sync_consumed = 0;
            self.is_synced = false;
            return Err(DemuxError::SyncBufExhausted { observed, max: cap });
        }
        self.sync_buf.extend_from_slice(bytes);
        // `resyncing` tracks whether the next 0x47 we find arrived via
        // a SCAN (loss-of-sync recovery path) — in which case it must
        // pass N-of-M validation before acceptance. Initial acquisition
        // (`live[0] == 0x47` on the first call, no scanning required)
        // and steady-state continuation (next-packet boundary aligned)
        // bypass N-of-M, matching ffmpeg `mpegts_resync` semantics
        // (resync logic fires only after a packet boundary turned out
        // not to carry 0x47).
        //
        // Cross-feed resume: if a previous `feed()` call ran out of
        // bytes mid-scan (live.len() < 188 with `bytes_since_sync > 0`
        // already accumulated), we re-enter here with `is_synced=false`
        // and a non-zero `bytes_since_sync`. The `> 0` predicate keeps
        // us in resync mode across the call boundary so the next 0x47
        // we find still has to pass N-of-M.
        let mut resyncing = !self.is_synced && self.bytes_since_sync > 0;
        loop {
            let live = &self.sync_buf[self.sync_consumed..];
            if live.len() < crate::mpegts::common::TS_PACKET_SIZE {
                self.compact_sync_buf();
                return Ok(());
            }
            // Sync to next 0x47.
            if live[0] != crate::mpegts::common::TS_SYNC_BYTE {
                // Lost sync (or never had it). Set the resync flag so
                // the next 0x47 we find must pass N-of-M validation.
                self.is_synced = false;
                resyncing = true;
                let mut i = 1;
                while i < live.len() && live[i] != crate::mpegts::common::TS_SYNC_BYTE {
                    i += 1;
                }
                self.bytes_since_sync += i;
                if self.bytes_since_sync > super::sync_ingress::SYNC_SEARCH_WINDOW {
                    return Err(DemuxError::Unrecoverable {
                        after_bytes: self.bytes_since_sync,
                    });
                }
                self.sync_consumed += i;
                self.compact_sync_buf();
                continue;
            }
            // Candidate sync byte. Only validate via N-of-M stride check
            // if we got here through the scan path (`resyncing`). Per
            // H.222.0 §2.4.3.2 and ffmpeg `mpegts.c::mpegts_resync`,
            // without this a stray 0x47 inside PES payload causes false
            // sync after a packet loss.
            if resyncing {
                use super::sync_ingress::NofMResult;
                match super::sync_ingress::sync_n_of_m_check(live) {
                    NofMResult::Accept => {
                        // Fall through to packet parse below.
                    }
                    NofMResult::Reject => {
                        // Stray 0x47 — advance past it and keep
                        // searching. Charge 1 byte against the
                        // sync-search window so adversarial input
                        // still hits Unrecoverable.
                        self.bytes_since_sync += 1;
                        if self.bytes_since_sync > super::sync_ingress::SYNC_SEARCH_WINDOW {
                            return Err(DemuxError::Unrecoverable {
                                after_bytes: self.bytes_since_sync,
                            });
                        }
                        self.sync_consumed += 1;
                        self.compact_sync_buf();
                        continue;
                    }
                    NofMResult::NeedMoreBytes => {
                        // Not enough strides buffered to confirm or
                        // reject. Keep the candidate in the buffer and
                        // return — the next `feed` call will deliver
                        // more bytes and re-evaluate. Preserve the
                        // `bytes_since_sync` charge so adversarial
                        // inputs that keep us in this state still hit
                        // Unrecoverable eventually.
                        self.compact_sync_buf();
                        return Ok(());
                    }
                }
            }
            // Have confirmed sync. Mark locked + reset the search counter.
            self.is_synced = true;
            resyncing = false;
            self.bytes_since_sync = 0;
            // Need to read 188 bytes; if the next byte after isn't 0x47 (or
            // we don't have enough buffer to check), we'll re-sync next loop.
            let pkt_buf: [u8; 188] = live[..crate::mpegts::common::TS_PACKET_SIZE]
                .try_into()
                .unwrap();
            self.sync_consumed += crate::mpegts::common::TS_PACKET_SIZE;
            self.compact_sync_buf();
            // Lenient mode catches `MalformedPes` and surfaces it as a
            // `NonConformant` event so the receive loop survives a single
            // corrupt PES on one PID. Strict modes still escalate. See
            // `handle_process_packet_result` for the per-error policy.
            let result = self.process_packet(&pkt_buf);
            self.handle_process_packet_result(result)?;
            // Strict-mode hatch: if the packet just processed produced a
            // `NonConformant` event whose issue category is rejected by the
            // configured `StrictMode`, surface it as a fatal error here. The
            // event itself is still in the queue; the caller can drain it
            // alongside the error if they want the narrative.
            if let Some(fatal) = self.fatal.take() {
                return Err(DemuxError::StrictRejection(format!("{fatal:?}")));
            }
        }
    }

    /// Fast-path ingress for callers that already hold a single 188-byte
    /// aligned TS packet (e.g. `pipeline::Receiver`, which produces `[u8; 188]`
    /// packets directly from the transport layer).
    ///
    /// Unlike [`feed`](Self::feed), this method skips the internal sync buffer
    /// and the 0x47 hunt entirely: the packet is dispatched inline with no
    /// heap allocation and no memmove. This eliminates the
    /// `extend_from_slice` + `compact_sync_buf` double-copy that `feed`
    /// performs on the already-aligned `Receiver` hot path.
    ///
    /// **The first byte of `pkt` MUST be `0x47`** (the MPEG-TS sync byte).
    /// Callers that cannot guarantee 188-byte alignment must use [`feed`](Self::feed).
    ///
    /// # Errors
    ///
    /// Returns `Err(DemuxError::Unrecoverable { after_bytes: 0 })` if
    /// `pkt[0] != 0x47` — the caller violated the alignment contract.
    /// All other errors mirror those of [`feed`](Self::feed): `MalformedPsi`
    /// and (in strict mode) `StrictRejection` or `MalformedPes`. In lenient
    /// mode (the default) `MalformedPes` is converted to a `NonConformant`
    /// event so a single corrupt PES doesn't tear down the receive loop.
    /// The `SyncBufExhausted` and `Unrecoverable { after_bytes > 0 }` variants
    /// cannot be returned by this method (no sync buffer is involved).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tst_core::mpegts::demux::Demuxer;
    /// # let pkt: [u8; 188] = {
    /// #     let mut p = [0u8; 188];
    /// #     p[0] = 0x47;
    /// #     p
    /// # };
    /// let mut d = Demuxer::new();
    /// // pkt must start with 0x47; obtained e.g. from pipeline::Receiver.
    /// d.feed_aligned(&pkt).expect("packet was aligned");
    /// while let Some(_event) = d.next_event() { /* handle */ }
    /// ```
    pub fn feed_aligned(&mut self, pkt: &[u8; 188]) -> Result<(), DemuxError> {
        if pkt[0] != crate::mpegts::common::TS_SYNC_BYTE {
            return Err(DemuxError::Unrecoverable { after_bytes: 0 });
        }
        // Caller guarantees alignment — lock sync state so the next
        // `feed` (if any) doesn't re-acquire via N-of-M.
        self.is_synced = true;
        self.bytes_since_sync = 0;
        let result = self.process_packet(pkt);
        self.handle_process_packet_result(result)?;
        if let Some(fatal) = self.fatal.take() {
            return Err(DemuxError::StrictRejection(format!("{fatal:?}")));
        }
        Ok(())
    }

    /// Pull the next available event. Returns `None` if no event is
    /// currently queued — feed more bytes and try again.
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_next_event` — see `bindings/c/include/tstrans.h`.
    /// The `None` case maps to the `TST_E_NOT_AVAILABLE` sentinel (-13).
    ///
    /// **Single-consumer contract (C ABI):** `tst_event_t` pointer fields
    /// are valid until the next `tst_demuxer_next_event` or
    /// `tst_demuxer_close` call on the same handle **from any thread**.
    /// Concurrent pulls on one handle silently invalidate the first
    /// caller's borrowed pointers; use one consumer thread per handle.
    pub fn next_event(&mut self) -> Option<DemuxEvent> {
        self.queue.pop_front()
    }

    /// Drain any partial PES still buffered in the reassembler — emit any
    /// complete events from them. Use on stream end (e.g. SRT receive loop
    /// reaching `TransportError::Closed`) to flush the last in-flight video AU
    /// or any other unbounded-PES payload that hadn't yet been finalized
    /// by a subsequent PUSI.
    ///
    /// Idempotent: calling twice with no further `feed` between them is safe
    /// and a no-op the second time.
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_flush` — see `bindings/c/include/tstrans.h`.
    pub fn flush(&mut self) {
        let partials = self.pes.drain_partial();
        for pes in partials {
            self.handle_complete_pes(pes);
        }
    }

    fn process_packet(&mut self, buf: &[u8; 188]) -> Result<(), DemuxError> {
        let pkt = match parse_ts_packet(buf) {
            Ok(p) => p,
            Err(TsParseError::BadAdaptationLength) => {
                let pid = u16::from_be_bytes([buf[1] & 0x1F, buf[2]]);
                let stream = self
                    .lookup_stream(pid)
                    .unwrap_or_else(|| StreamId::anonymous(pid, 0));
                self.queue_nonconformant(
                    stream,
                    NonConformantIssue::AdaptationFieldMalformed {
                        pid,
                        kind: AdaptationFieldKind::BadLengthForControl,
                    },
                );
                return Ok(());
            }
            Err(TsParseError::NoSyncByte) | Err(TsParseError::Truncated) => return Ok(()),
        };
        // ISO/IEC 13818-1 §2.4.3.2: `transport_error_indicator=1` means an
        // upstream link-layer flagged the packet as known-corrupt. ffmpeg
        // drops these and flags AV_PKT_FLAG_CORRUPT (mpegts.c:3091-3097);
        // feeding the payload to PES/PSI reassembly would corrupt downstream
        // parse state. Drop entirely and surface the drop as non-conformant
        // so consumers can correlate with downstream parse failures.
        if pkt.transport_error_indicator {
            let stream = self
                .lookup_stream(pkt.pid)
                .unwrap_or_else(|| StreamId::anonymous(pkt.pid, 0));
            self.queue_nonconformant(
                stream,
                NonConformantIssue::TransportErrorPacket { pid: pkt.pid },
            );
            return Ok(());
        }
        // ISO/IEC 13818-1 §2.4.3.2: a non-zero `transport_scrambling_control`
        // marks the payload as scrambled. The library does not descramble;
        // feeding scrambled bytes to PSI/PES reassembly would corrupt parse
        // state and surface as random malformation. Drop the packet (no
        // payload routed) and surface UnsupportedScrambling so consumers can
        // distinguish "unsupported scrambling" from "random corruption".
        // REF-TS-01.
        if pkt.transport_scrambling_control != 0 {
            let stream = self
                .lookup_stream(pkt.pid)
                .unwrap_or_else(|| StreamId::anonymous(pkt.pid, 0));
            self.queue_nonconformant(
                stream,
                NonConformantIssue::UnsupportedScrambling {
                    pid: pkt.pid,
                    control: pkt.transport_scrambling_control,
                },
            );
            return Ok(());
        }
        // REF-TS-02: surface adaptation-field control/length violations.
        // ReservedControl (00) routes neither adaptation nor payload by
        // construction; BadLengthForControl / ShortPcr may still carry a
        // routable payload — continue best-effort (lenient).
        if let Some(kind) = pkt.adaptation_malformed {
            let stream = self
                .lookup_stream(pkt.pid)
                .unwrap_or_else(|| StreamId::anonymous(pkt.pid, 0));
            self.queue_nonconformant(
                stream,
                NonConformantIssue::AdaptationFieldMalformed { pid: pkt.pid, kind },
            );
        }
        self.check_pcr(&pkt);
        let (cc_jumped, is_duplicate) = self.check_continuity(&pkt);
        // H.222.0 §2.4.3.3: a spec-legal duplicate (same CC, no
        // discontinuity_indicator) carries no new payload data. Suppress all
        // routing to avoid replaying bytes into PSI or PES reassemblers.
        if is_duplicate {
            return Ok(());
        }
        if pkt.pid == 0x0000 {
            self.handle_psi(
                pkt.pid,
                pkt.payload,
                pkt.payload_unit_start,
                true,
                cc_jumped,
            )?;
        } else if self.programs.contains_key(&pkt.pid) {
            self.handle_psi(
                pkt.pid,
                pkt.payload,
                pkt.payload_unit_start,
                false,
                cc_jumped,
            )?;
        } else if pkt.has_payload && self.stream_kind_by_pid.contains_key(&pkt.pid) {
            self.handle_pes_packet(&pkt)?;
        }
        Ok(())
    }

    pub(super) fn lookup_stream(&self, pid: u16) -> Option<StreamId> {
        self.stream_kind_by_pid.get(&pid).copied().map(|kind| {
            let program_number = self.program_number_for_pid(pid);
            StreamId {
                pid,
                kind,
                program_number,
            }
        })
    }

    /// Look up the program_number for a PID via the `pid_to_program` map.
    /// Returns 0 if the PID is not owned by any known program (e.g. PSI PIDs).
    pub(super) fn program_number_for_pid(&self, pid: u16) -> u16 {
        self.pid_to_program.get(&pid).copied().unwrap_or(0)
    }

    /// Unwrap a raw 33-bit PTS (or a KLV Metadata PTS — same clock, same
    /// accumulator) for `pid` into a monotonic `i64` timeline, advancing
    /// the per-PID accumulator. Only called when
    /// `options.unwrap_timestamps` is `true` and a genuine (non-
    /// synthesized) PTS was observed.
    ///
    /// The first observed value for a PID anchors the timeline (emitted
    /// value == the raw value). Each subsequent value accumulates its
    /// signed wrap-aware delta from the previous RAW value onto the
    /// previous UNWRAPPED value:
    /// `unwrapped = last_unwrapped + pts_diff_33bit(raw, last_raw)`.
    /// The timeline is never rebased to zero — see
    /// [`DemuxerConfig::unwrap_timestamps`](crate::mpegts::demux::DemuxerConfig::unwrap_timestamps)
    /// for why that matters for cross-PID (video vs. KLV) comparability.
    ///
    /// Accumulating a *signed* delta is what makes reordering safe. A
    /// genuine small backward step (an out-of-order arrival) yields a
    /// small negative delta, so the emitted value simply steps back —
    /// non-monotonic, correctly reflecting the reorder, and still in the
    /// right epoch. In particular a pre-wrap PTS delivered AFTER the wrap
    /// is numerically LARGER than its predecessor, so a running-offset
    /// scheme (bump on `raw < last_raw`) would leave the already-bumped
    /// offset applied to it and strand it a full `1 << 33` too high;
    /// measuring from the predecessor instead places it back below the
    /// boundary where it belongs.
    pub(super) fn unwrap_pts(
        &mut self,
        pid: u16,
        raw: crate::mpegts::common::Pts90khz,
    ) -> crate::mpegts::common::Pts90khz {
        let raw_ticks = raw.as_ticks();
        let Some(state) = self.unwrap_state.get(&pid).copied() else {
            self.unwrap_state.insert(
                pid,
                UnwrapState {
                    last_raw: raw_ticks,
                    last_unwrapped: raw_ticks,
                },
            );
            return crate::mpegts::common::Pts90khz::new(raw_ticks);
        };
        let delta = crate::mpegts::common::pts_diff_33bit(raw_ticks as u64, state.last_raw as u64);
        let unwrapped = state.last_unwrapped.saturating_add(delta);
        self.unwrap_state.insert(
            pid,
            UnwrapState {
                last_raw: raw_ticks,
                last_unwrapped: unwrapped,
            },
        );
        crate::mpegts::common::Pts90khz::new(unwrapped)
    }

    /// Unwrap a DTS that has a PTS on the same PES, relative to that
    /// PES's OWN raw PTS — NOT the bare per-PID offset.
    ///
    /// This distinction matters exactly at a wrap: the 33-bit boundary
    /// can fall BETWEEN one AU's DTS and PTS (DTS ≤ PTS always, so DTS
    /// can still be in the pre-wrap epoch while PTS has already
    /// wrapped). Applying [`Self::unwrap_pts`]'s just-advanced offset
    /// directly to the DTS would then put it a full `1 << 33` epoch too
    /// high — this method instead measures the DTS's wrap-aware
    /// distance from its own PES's raw PTS (via
    /// [`crate::mpegts::common::pts_diff_33bit`]) and adds that to the
    /// *unwrapped* PTS, so the result lands in the correct epoch
    /// whether or not this particular PES straddled the boundary.
    ///
    /// Pure — does not read or mutate the accumulator. `unwrapped_pts`
    /// is this PES's already-unwrapped PTS (the return of
    /// [`Self::unwrap_pts`]); `pts_raw` / `dts_raw` are this PES's raw
    /// 33-bit wire values.
    pub(super) fn unwrap_dts_with_pts(
        unwrapped_pts: crate::mpegts::common::Pts90khz,
        pts_raw: crate::mpegts::common::Pts90khz,
        dts_raw: crate::mpegts::common::Pts90khz,
    ) -> crate::mpegts::common::Pts90khz {
        let delta = crate::mpegts::common::pts_diff_33bit(
            dts_raw.as_ticks() as u64,
            pts_raw.as_ticks() as u64,
        );
        crate::mpegts::common::Pts90khz::new(unwrapped_pts.as_ticks() + delta)
    }

    /// Unwrap a DTS that arrived with NO PTS on the same PES — not
    /// spec-legal (§2.4.3.6 forbids `PTS_DTS_flags = '01'`), but
    /// tolerated defensively. Falls back to the *current* per-PID
    /// accumulator offset — derived as `last_unwrapped - last_raw`, i.e.
    /// how far the last sample's emitted value sat above its raw wire
    /// value (no independent wrap detection, no state mutation).
    /// Defaults to offset 0 if `pid` has no accumulator entry yet.
    pub(super) fn unwrap_secondary_ts(
        &self,
        pid: u16,
        raw: crate::mpegts::common::Pts90khz,
    ) -> crate::mpegts::common::Pts90khz {
        let offset = self
            .unwrap_state
            .get(&pid)
            .map_or(0, |s| s.last_unwrapped.saturating_sub(s.last_raw));
        crate::mpegts::common::Pts90khz::new(offset.saturating_add(raw.as_ticks()))
    }

    /// Convert a `process_packet` result into lenient/strict policy.
    ///
    /// Lenient mode (`StrictMode::Off`): `DemuxError::MalformedPes` becomes
    /// a `NonConformant` event with `NonConformantIssue::MalformedPes` so
    /// the receive loop survives a single corrupt PES on one PID. Strict
    /// modes that reject `NonConformantIssue::MalformedPes` (today only
    /// `StrictMode::Full`) propagate the original error so callers see the
    /// failure rather than a silently-buried event.
    ///
    /// All other `DemuxError` variants (`MalformedPsi`, `Unrecoverable`,
    /// `StrictRejection`, `SyncBufExhausted`) are pass-through — those
    /// represent unrecoverable byte-stream conditions or strict-mode
    /// rejections already shaped for caller handling.
    fn handle_process_packet_result(
        &mut self,
        result: Result<(), DemuxError>,
    ) -> Result<(), DemuxError> {
        match result {
            Ok(()) => Ok(()),
            Err(DemuxError::MalformedPes { pid, reason }) => {
                let issue = NonConformantIssue::MalformedPes { pid, reason };
                if self.options.strict.rejects(&issue) {
                    return Err(DemuxError::MalformedPes { pid, reason });
                }
                let stream = self
                    .lookup_stream(pid)
                    .unwrap_or_else(|| StreamId::anonymous(pid, self.program_number_for_pid(pid)));
                self.queue_nonconformant(stream, issue);
                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    /// Return a reference to the programs map for white-box unit tests.
    ///
    /// Keyed by `pmt_pid`. Crate-internal test accessor for PAT/PMT diffing
    /// logic. Not part of the stable API.
    #[cfg(test)]
    pub(crate) fn programs_for_test(&self) -> &HashMap<u16, ProgramTracker> {
        &self.programs
    }

    /// Per-PID codec-specific counters. See
    /// [`crate::mpegts::stats::StreamCodecStats`] for the semantics of
    /// the return value (`None` vs `Some(Unknown)` vs typed variant).
    pub fn stream_codec_stats(&self, pid: u16) -> Option<crate::mpegts::stats::StreamCodecStats> {
        if let Some(c) = self.stream_codec_counters.get(&pid) {
            return Some(c.to_public());
        }
        if self.stats_per_stream.contains_key(&pid) {
            return Some(crate::mpegts::stats::StreamCodecStats::Unknown);
        }
        None
    }

    /// Return a snapshot of current demuxer statistics.
    pub fn stats(&self) -> DemuxerStats {
        DemuxerStats {
            program_maps_seen: self.program_maps_seen,
            pmt_versions_seen: self.pmt_versions_seen,
            discontinuities: self.discontinuities_count,
            nonconformant: self.nonconformant_count,
            programs_seen: self.programs.len() as u32,
            subtitle_streams_seen: self.subtitle_streams_seen_count,
            per_stream: self.stats_per_stream.clone(),
        }
    }

    /// Drop all per-PID parse state — sync buffer, PSI assemblers,
    /// program/PMT topology, PES reassembly, CC/PCR/PTS tracking, and
    /// the pending event queue. Does NOT reset stats counters (those
    /// reflect cumulative observations across the receiver's lifetime
    /// and are reset only via [`Self::reset_stats`]).
    ///
    /// Intended for transport-reconnect scenarios at a higher
    /// composition layer (`ManagedDemuxReceiver`): when the underlying
    /// transport has been re-established, partial PSI sections, half-
    /// assembled PES samples, and continuity-counter state from the
    /// dead connection MUST NOT splice into the new connection's parse
    /// state — otherwise the next PMT, the next PUSI, and the next CC
    /// jump all carry stale predecessors and produce corrupted
    /// downstream events.
    ///
    /// Idempotent: safe to call repeatedly with no intervening feed.
    ///
    /// The PAT PID (0x0000) assembler is re-seeded so the next PAT
    /// section arrives ready to parse. Configuration (`DemuxerConfig`,
    /// strict mode, PES caps) is preserved across reset.
    pub fn reset_sync(&mut self) {
        self.sync_buf.clear();
        self.sync_consumed = 0;
        self.psi_assemblers.clear();
        self.psi_assemblers
            .insert(0x0000, PsiSectionAssembler::new());
        self.programs.clear();
        self.pat_version = None;
        self.stream_kind_by_pid.clear();
        self.cc_by_pid.clear();
        self.dup_by_pid.clear();
        self.last_pkt_raw_by_pid.clear();
        self.last_psi_cc_jump = None;
        self.last_pcr_by_pid.clear();
        self.last_pts_by_pid.clear();
        // A reconnect restarts the opt-in PTS/DTS unwrap timeline (see
        // `DemuxerConfig::unwrap_timestamps`) — stale offsets from the
        // dead connection must not splice into the new one's epoch.
        self.unwrap_state.clear();
        // Drop all in-flight PES reassembly state. A new reassembler
        // with the same caps replaces it (Reassembler exposes no
        // public reset method; constructing fresh is the canonical
        // way to drop all `by_pid` partials + the total_buffered counter).
        let cap_per_pid = self
            .options
            .pes_cap_per_pid
            .unwrap_or(DEFAULT_PES_CAP_PER_PID);
        let cap_total = self.options.pes_cap_total.unwrap_or(DEFAULT_PES_CAP_TOTAL);
        self.pes = Reassembler::new(cap_per_pid, cap_total);
        self.queue.clear();
        self.bytes_since_sync = 0;
        self.is_synced = false;
        self.fatal = None;
        // Per-PMT-version PID dedupe sets clear so a fresh PMT post-
        // reconnect re-fires `SubtitleMissingDescriptor` /
        // `Av1RegistrationMalformed` / `SubtitleDescriptorAmbiguous` if
        // those still apply on the new connection.
        self.subtitle_missing_descriptor_emitted.clear();
        self.av1_registration_malformed_emitted.clear();
        self.subtitle_descriptor_ambiguous_emitted.clear();
        self.pid_to_program.clear();
        // Drop all in-flight AU cell reassembly buffers. Operational reset
        // — no NonConformant emitted; pre-reconnect cells are simply gone.
        self.au_reassembler.reset_all();
        // Drop any partially-assembled multi-section PAT (REF-PSI-02).
        self.pat_reassembler.clear();
    }

    /// Reset all stats counters to zero and clear per-stream entries.
    ///
    /// Per-PID entries are dropped (both the unified `stats_per_stream`
    /// and the codec-counter side table); the next event for a
    /// previously-seen PID re-materializes both entries with the kind
    /// discriminator derived from the current `stream_type`. This keeps
    /// the 3-state [`Self::stream_codec_stats`] accessor symmetric across
    /// reset — an unseen PID returns `None` both before AND after.
    ///
    /// Also drops the cached PMT version so the next incoming PMT will
    /// increment `pmt_versions_seen` even if the version_number hasn't
    /// changed.
    pub fn reset_stats(&mut self) {
        self.program_maps_seen = 0;
        self.pmt_versions_seen = 0;
        self.discontinuities_count = 0;
        self.nonconformant_count = 0;
        self.subtitle_streams_seen_count = 0;
        self.subtitle_pids_seen.clear();
        self.stats_per_stream.clear();
        self.stream_codec_counters.clear();
        // Drop cached PMT versions on each ProgramTracker so the next PMT
        // triggers pmt_versions_seen += 1 even if the version_number hasn't changed.
        for tracker in self.programs.values_mut() {
            tracker.pmt_version = None;
        }
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpegts::common::Pts90khz;
    use crate::mpegts::demux::StrictMode;
    use crate::mpegts::demux::event::{
        AudioCodec, DiscontinuityKind, SamplePayload, SubtitleCodec, VideoCodec,
    };
    use crate::mpegts::demux::pmt_classify::{
        classify_0x06, classify_0x06_with_ambiguity, is_malformed_av1_registration,
        stream_type_from_kind,
    };
    use crate::mpegts::demux::types::{
        DemuxerConfig, default_pes_cap_per_pid, default_pes_cap_total,
    };

    #[test]
    fn builder_carries_defaults() {
        let d = Demuxer::with_config(DemuxerConfig::builder().build());
        assert_eq!(d.options.strict, StrictMode::Off);
        assert_eq!(d.options.pes_cap_per_pid, None);
    }

    /// A `feed` that overflows `MAX_SYNC_BUF_BYTES` must reject WITHOUT
    /// first copying the caller's bytes into `sync_buf`. The pre-fix code
    /// did `extend_from_slice` (a full copy + realloc of the entire input)
    /// and only then checked the ceiling, so an adversarial multi-GiB feed
    /// could OOM the host before the check fired. This guards the
    /// check-before-extend contract by asserting the buffer's *capacity*
    /// does not grow across a rejected feed. White-box (reads the private
    /// `sync_buf.capacity()`), so it lives here rather than in the
    /// integration `demux_caps.rs`.
    #[test]
    fn oversized_feed_rejects_without_growing_capacity() {
        let mut dx = Demuxer::new();
        let cap_before = dx.sync_buf.capacity();
        let len_before = dx.sync_buf.len();

        // One byte past the ceiling — the smallest oversized feed.
        let garbage = vec![0xFFu8; crate::mpegts::demux::sync_ingress::MAX_SYNC_BUF_BYTES + 1];
        let result = dx.feed(&garbage);
        assert!(
            matches!(
                result,
                Err(crate::error::DemuxError::SyncBufExhausted { .. })
            ),
            "expected SyncBufExhausted, got {result:?}"
        );

        // The rejected bytes were never copied: capacity (and len) unchanged.
        assert_eq!(
            dx.sync_buf.capacity(),
            cap_before,
            "rejected oversized feed grew sync_buf capacity (check-before-extend violated)"
        );
        assert_eq!(dx.sync_buf.len(), len_before);
    }

    // A null TS packet (PID 0x1FFF) — syntactically valid 188-byte packet with
    // a 0x47 sync byte. The null PID carries no payload; the demuxer ignores it
    // without emitting any event, making it safe as a volume-fill packet in
    // cap tests that care only about accepting/rejecting the feed, not events.
    fn null_ts_packet() -> [u8; 188] {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47; // sync byte
        pkt[1] = 0x1F; // pid_high = 0x1F (null PID 0x1FFF)
        pkt[2] = 0xFF; // pid_low = 0xFF
        pkt[3] = 0x10; // adaptation_field_control=01, cc=0
        pkt
    }

    #[test]
    fn sync_buf_cap_default_rejects_whole_file_feed_over_4mib() {
        // The field-report case: a valid ~5 MiB TS fed in ONE call must still
        // exhaust the DEFAULT ceiling (the check is on the feed-call size).
        let pkt = null_ts_packet();
        let n = (5 * 1024 * 1024) / 188 + 1;
        let mut data = Vec::with_capacity(n * 188);
        for _ in 0..n {
            data.extend_from_slice(&pkt);
        }
        let mut d = Demuxer::new();
        let err = d.feed(&data).unwrap_err();
        assert!(matches!(err, DemuxError::SyncBufExhausted { .. }));
    }

    #[test]
    fn sync_buf_cap_raised_accepts_whole_file_feed() {
        let pkt = null_ts_packet();
        let n = (5 * 1024 * 1024) / 188 + 1;
        let mut data = Vec::with_capacity(n * 188);
        for _ in 0..n {
            data.extend_from_slice(&pkt);
        }
        let cfg = DemuxerConfig::builder()
            .sync_buf_cap(16 * 1024 * 1024)
            .build();
        let mut d = Demuxer::with_config(cfg);
        d.feed(&data)
            .expect("raised ceiling must accept a whole-file feed");
    }

    #[test]
    fn sync_buf_exhausted_message_names_the_knob() {
        // The 0.2.0 wording sent the integrator hunting through pes_cap_* (a
        // dead end). The message must now name the actual knob and the pattern.
        let mut d = Demuxer::new();
        let garbage = vec![0xFFu8; 5 * 1024 * 1024];
        let err = d.feed(&garbage).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sync_buf_cap"), "got: {msg}");
        assert!(msg.contains("smaller chunks"), "got: {msg}");
    }

    #[test]
    fn demuxer_options_default_strict_psi_reassembly() {
        let opts = DemuxerConfig::default();
        assert!(
            !opts.lenient_psi_reassembly,
            "default is strict (per ffmpeg parity); opt-in lenient via lenient_psi_reassembly=true"
        );
    }

    #[test]
    fn builder_overrides_apply() {
        let d = Demuxer::with_config(
            DemuxerConfig::builder()
                .strict(StrictMode::TimingOnly)
                .pes_cap_per_pid(1 << 20)
                .pes_cap_total(8 << 20)
                .link_klv(0x100, 0x101)
                .build(),
        );
        assert_eq!(d.options.strict, StrictMode::TimingOnly);
        assert_eq!(d.options.pes_cap_per_pid, Some(1 << 20));
        assert_eq!(d.options.pes_cap_total, Some(8 << 20));
        assert_eq!(d.options.klv_link_overrides, vec![(0x100, 0x101)]);
    }

    #[test]
    fn builder_treat_as_override_applies() {
        let d = Demuxer::with_config(
            DemuxerConfig::builder()
                .treat_as(0x100, StreamKind::Video(VideoCodec::H265))
                .build(),
        );
        assert_eq!(
            d.options.stream_kind_overrides.get(&0x100),
            Some(&StreamKind::Video(VideoCodec::H265))
        );
    }

    /// Per-codec dispatch in stats `stream_type` byte. Prior code returned
    /// 0x0F (ADTS AAC) for every audio kind; MP2/LATM/AC-3 streams misreported
    /// in StreamStats.
    #[test]
    fn stream_type_from_kind_per_audio_codec() {
        assert_eq!(
            stream_type_from_kind(&StreamKind::Audio(AudioCodec::Mp2)),
            0x03
        );
        assert_eq!(
            stream_type_from_kind(&StreamKind::Audio(AudioCodec::Aac)),
            0x0F
        );
        assert_eq!(
            stream_type_from_kind(&StreamKind::Audio(AudioCodec::AacLatm)),
            0x11
        );
        assert_eq!(
            stream_type_from_kind(&StreamKind::Audio(AudioCodec::Ac3)),
            0x81
        );
    }

    #[test]
    fn default_caps_match_plan_decision() {
        // Spec §11.2 closure: 4 MiB / 64 MiB.
        assert_eq!(default_pes_cap_per_pid(), 4 * 1024 * 1024);
        assert_eq!(default_pes_cap_total(), 64 * 1024 * 1024);
    }

    #[test]
    fn empty_input_produces_no_events() {
        let mut d = Demuxer::new();
        d.feed(&[]).unwrap();
        assert!(d.next_event().is_none());
    }

    #[test]
    fn unrecoverable_after_bytes() {
        let mut d = Demuxer::new();
        let big = vec![0xAA; crate::mpegts::demux::sync_ingress::SYNC_SEARCH_WINDOW * 2];
        let err = d.feed(&big).unwrap_err();
        assert!(matches!(err, DemuxError::Unrecoverable { .. }));
    }

    #[test]
    fn flush_is_idempotent_and_safe_with_no_state() {
        let mut d = Demuxer::new();
        // Empty — no events queued by flush.
        d.flush();
        assert!(d.next_event().is_none());
        // Second call also a no-op.
        d.flush();
        assert!(d.next_event().is_none());
    }

    #[test]
    fn stats_lazy_creates_per_stream_on_first_event() {
        let d = Demuxer::new();
        let st = d.stats();
        assert_eq!(st.program_maps_seen, 0);
        assert_eq!(st.pmt_versions_seen, 0);
        assert_eq!(st.per_stream.len(), 0);
    }

    #[test]
    fn stats_reset_clears_per_stream_and_zeroes_counters() {
        let mut d = Demuxer::new();
        d.reset_stats();
        let st = d.stats();
        assert_eq!(st.program_maps_seen, 0);
        assert_eq!(st.pmt_versions_seen, 0);
        assert_eq!(st.discontinuities, 0);
        assert_eq!(st.nonconformant, 0);
        assert!(st.per_stream.is_empty());
    }

    #[test]
    fn stream_codec_stats_returns_none_for_never_seen_pid() {
        let demux = Demuxer::new();
        assert_eq!(demux.stream_codec_stats(0x1234), None);
    }

    #[test]
    fn stream_codec_stats_returns_none_for_unbumped_psi_pid_before_any_feed() {
        // Without any feed, PSI PIDs haven't been observed yet — returns None.
        // Once a PMT arrives the demuxer's stats_per_stream map populates
        // entries for PAT/PMT PIDs; full integration coverage of the
        // "seen but uncounted → Some(Unknown)" path lives in
        // crates/tst-core/tests/codec_stats.rs (Task 5).
        let demux = Demuxer::new();
        assert_eq!(demux.stream_codec_stats(0x0000), None);
    }

    #[test]
    fn reset_stats_drops_codec_counter_entries() {
        // Counter for an unseen PID should be None both before AND after reset.
        let mut demux = Demuxer::new();
        assert_eq!(demux.stream_codec_stats(0x1234), None);
        demux.reset_stats();
        assert_eq!(demux.stream_codec_stats(0x1234), None);
    }

    #[test]
    fn bump_video_counters_increments_existing_entry() {
        let mut demux = Demuxer::new();
        crate::mpegts::stats::bump_video_counters(&mut demux.stream_codec_counters, 0x100, 2, 1);
        match demux.stream_codec_stats(0x100) {
            Some(crate::mpegts::stats::StreamCodecStats::Video {
                nals_or_obus: 2,
                random_access_aus: 1,
                ..
            }) => {}
            other => panic!("expected Video {{2,1}}, got {:?}", other),
        }
        crate::mpegts::stats::bump_video_counters(&mut demux.stream_codec_counters, 0x100, 3, 0);
        match demux.stream_codec_stats(0x100) {
            Some(crate::mpegts::stats::StreamCodecStats::Video {
                nals_or_obus: 5,
                random_access_aus: 1,
                ..
            }) => {}
            other => panic!("expected Video {{5,1}}, got {:?}", other),
        }
    }

    #[test]
    fn pmt_program_map_event_carries_raw_descriptors() {
        use crate::mpegts::demux::event::DemuxEvent;
        use crate::mpegts::descriptors;
        use crate::mpegts::mux::{
            MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
        };

        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, MuxVideoCodec::H264);
            prog.stream_descriptors_for_video(
                0,
                vec![descriptors::user_private(b"EO 1080p").expect("label within 255-byte cap")],
            )
            .unwrap();
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut mux = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        // Push a minimal H.264 AU to trigger PSI + PES emission.
        mux.push_video(&[0, 0, 0, 1, 0x09, 0x10], Pts90khz::new(9000), true)
            .unwrap();
        let mut buf = vec![0u8; 188 * 32];
        let n = mux.pull(&mut buf);

        let mut demuxer = Demuxer::new();
        demuxer.feed(&buf[..n]).unwrap();

        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }

        let pm = events
            .iter()
            .find_map(|e| match e {
                DemuxEvent::ProgramMap(pm) => Some(pm),
                _ => None,
            })
            .expect("ProgramMap event emitted");
        let stream = pm
            .streams
            .iter()
            .find(|s| s.pid == 0x100)
            .expect("video PID 0x100 in ProgramMap");
        assert_eq!(stream.raw_descriptors.len(), 1);
        assert_eq!(stream.raw_descriptors[0].tag, 0xFF);
        assert_eq!(stream.raw_descriptors[0].data, b"EO 1080p".to_vec());
    }

    #[test]
    fn demuxer_emits_audio_sample_for_aac_pes() {
        use crate::mpegts::demux::event::AudioCodec;
        use crate::mpegts::mux::{
            AudioCodec as MuxAudioCodec, MuxerConfig, MuxerProgramConfigBuilder,
        };

        // Mux: single-program with one AAC audio stream.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x300, MuxAudioCodec::Aac);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let audio_payload: Vec<u8> = vec![
            0xFF, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC, 0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02,
            0x03, 0x04,
        ];
        muxer
            .push_audio(&audio_payload, Pts90khz::new(90_000))
            .unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        // Demux: feed the muxed bytes, capture events.
        let mut demuxer = Demuxer::new();
        demuxer.feed(&buf).unwrap();
        demuxer.flush();
        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }

        let sample = events
            .iter()
            .find(|e| {
                matches!(
                    e,
                    DemuxEvent::Sample {
                        payload: SamplePayload::Audio { .. },
                        ..
                    }
                )
            })
            .expect("audio Sample event present");

        if let DemuxEvent::Sample {
            stream,
            pts,
            dts,
            payload: event_payload,
        } = sample
        {
            assert_eq!(stream.pid, 0x300);
            assert!(matches!(stream.kind, StreamKind::Audio(AudioCodec::Aac)));
            assert_eq!(pts.as_ticks(), 90_000);
            assert_eq!(*dts, None, "audio has no DTS");
            if let SamplePayload::Audio { codec, frames } = event_payload {
                assert_eq!(*codec, AudioCodec::Aac);
                assert_eq!(
                    frames.as_slice(),
                    audio_payload.as_slice(),
                    "all payload bytes recovered"
                );
            }
        }
    }

    #[test]
    fn treat_as_overrides_pmt_classification_to_typed_audio() {
        use crate::mpegts::demux::event::AudioCodec;
        use crate::mpegts::mux::{
            AudioCodec as MuxAudioCodec, MuxerConfig, MuxerProgramConfigBuilder,
        };

        // Mux an AAC audio stream (PMT stream_type = 0x0F, default classifies
        // as Aac).
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x300, MuxAudioCodec::Aac);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let audio_payload: Vec<u8> = vec![
            0xFF, 0xF1, 0x4C, 0x80, 0x00, 0x1F, 0xFC, 0xDE, 0xAD, 0xBE, 0xEF,
        ];
        muxer
            .push_audio(&audio_payload, Pts90khz::new(90_000))
            .unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        // Demux WITHOUT treat_as: classification follows PMT → Audio(Aac).
        let mut demuxer = Demuxer::new();
        demuxer.feed(&buf).unwrap();
        demuxer.flush();
        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }
        let aac_classified = events.iter().any(|e| {
            matches!(
                e,
                DemuxEvent::Sample {
                    payload: SamplePayload::Audio {
                        codec: AudioCodec::Aac,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(aac_classified, "default: PMT classifies as Aac");

        // Demux WITH treat_as override: classifies as Mp2 (override wins).
        let mut options = DemuxerConfig::default();
        options
            .stream_kind_overrides
            .insert(0x300, StreamKind::Audio(AudioCodec::Mp2));
        let mut demuxer = Demuxer::with_config(options);
        demuxer.feed(&buf).unwrap();
        demuxer.flush();
        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }
        let mp2_overridden = events.iter().any(|e| {
            matches!(
                e,
                DemuxEvent::Sample {
                    payload: SamplePayload::Audio {
                        codec: AudioCodec::Mp2,
                        ..
                    },
                    ..
                }
            )
        });
        assert!(mp2_overridden, "treat_as override: classifies as Mp2");
    }

    #[test]
    fn treat_as_routes_arbitrary_pid_to_subtitle_codec() {
        use crate::mpegts::demux::event::SubtitleCodec as DemuxSubtitleCodec;
        use crate::mpegts::mux::{
            AudioCodec as MuxAudioCodec, MuxerConfig, MuxerProgramConfigBuilder,
        };

        // Mux an audio stream on PID 0x200 (PMT stream_type = 0x04 for MP2).
        // The PMT entry will have no subtitle descriptor — but the
        // `stream_kind_overrides` map will remap the PID to
        // `StreamKind::Subtitle(WebVttInTs)`. The demuxer should dispatch
        // through the subtitle arm of `handle_complete_pes` and produce a
        // `SamplePayload::Subtitle` event.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x200, MuxAudioCodec::Mp2);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        // Body content irrelevant to dispatch — just needs to traverse
        // PES reassembly cleanly. Use a WEBVTT-like header for clarity.
        let payload = b"WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n".to_vec();
        muxer.push_audio(&payload, Pts90khz::new(90_000)).unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        let mut options = DemuxerConfig::default();
        options
            .stream_kind_overrides
            .insert(0x200, StreamKind::Subtitle(DemuxSubtitleCodec::WebVttInTs));
        let mut demuxer = Demuxer::with_config(options);
        demuxer.feed(&buf).unwrap();
        demuxer.flush();

        let mut got_subtitle = false;
        while let Some(ev) = demuxer.next_event() {
            if let DemuxEvent::Sample {
                stream,
                payload: SamplePayload::Subtitle { codec, .. },
                ..
            } = ev
            {
                if stream.pid == 0x200 && codec == DemuxSubtitleCodec::WebVttInTs {
                    got_subtitle = true;
                }
            }
        }
        assert!(
            got_subtitle,
            "treat_as remap produced subtitle Sample event"
        );
    }

    #[test]
    fn treat_as_routes_subtitle_without_descriptor_emits_non_conformant() {
        use crate::mpegts::demux::event::SubtitleCodec as DemuxSubtitleCodec;
        use crate::mpegts::mux::{
            AudioCodec as MuxAudioCodec, MuxerConfig, MuxerProgramConfigBuilder,
        };

        // PMT entry for 0x200 carries no subtitle descriptor. `treat_as`
        // remaps it to a subtitle codec — classifier should surface
        // `NonConformantIssue::SubtitleMissingDescriptor` once for that PID.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x200, MuxAudioCodec::Mp2);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        muxer
            .push_audio(b"WEBVTT\n", Pts90khz::new(90_000))
            .unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        let mut options = DemuxerConfig::default();
        options
            .stream_kind_overrides
            .insert(0x200, StreamKind::Subtitle(DemuxSubtitleCodec::WebVttInTs));
        let mut demuxer = Demuxer::with_config(options);
        demuxer.feed(&buf).unwrap();
        demuxer.flush();

        let mut got_missing_descriptor = false;
        let mut count = 0;
        while let Some(ev) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue: NonConformantIssue::SubtitleMissingDescriptor { pid },
                ..
            } = ev
            {
                if pid == 0x200 {
                    got_missing_descriptor = true;
                    count += 1;
                }
            }
        }
        assert!(
            got_missing_descriptor,
            "expected SubtitleMissingDescriptor for PID 0x200"
        );
        assert_eq!(count, 1, "deduped: one event per PID per PMT version");
    }

    #[test]
    fn multi_descriptor_0x06_emits_subtitle_ambiguous() {
        use crate::mpegts::mux::{
            MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSubtitleCodec,
            VideoCodec as MuxVideoCodec,
        };

        // Caller supplies BOTH a subtitling_descriptor (0x59) and a
        // VTTC registration_descriptor on the same WebVTT subtitle PID.
        // The mux auto-emit suppresses on either marker, so caller has
        // to bypass that path by stuffing both into the descriptor list
        // directly. Two recognized subtitle codec markers on the same
        // 0x06 PID — the classifier still picks subtitling per first-match
        // priority, and the demuxer surfaces the ambiguity for diagnostics.
        // subtitling_descriptor body: ISO 639 lang (3) + subtitling_type
        // (1) + composition_page_id (2) + ancillary_page_id (2).
        let subtitling_tlv = vec![0x59u8, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01];
        // registration_descriptor body: 4-byte format_identifier "VTTC".
        let vttc_tlv = vec![0x05u8, 0x04, b'V', b'T', b'T', b'C'];
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x101, MuxVideoCodec::H264);
            prog.add_subtitle(0x200, MuxSubtitleCodec::WebVttInTs);
            prog.stream_descriptors_for_subtitle(0, vec![subtitling_tlv, vttc_tlv])
                .unwrap();
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        // Push something to force PSI emission.
        let h = muxer.subtitle_handles()[0];
        muxer
            .push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n\nx\n")
            .unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        let mut demuxer = Demuxer::new();
        demuxer.feed(&buf).unwrap();
        demuxer.flush();

        let mut got_ambiguous = false;
        let mut count = 0;
        while let Some(ev) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue: NonConformantIssue::SubtitleDescriptorAmbiguous { pid, tags },
                ..
            } = ev
            {
                if pid == 0x200 {
                    got_ambiguous = true;
                    count += 1;
                    // 0x59 (subtitling) priority before 0xF0 (synthetic VTTC).
                    assert_eq!(tags, vec![0x59, 0xF0]);
                }
            }
        }
        assert!(
            got_ambiguous,
            "expected SubtitleDescriptorAmbiguous for PID 0x200"
        );
        assert_eq!(count, 1, "deduped: one event per PID per PMT version");
    }

    // -- classify_0x06: PSI cascade for stream_type 0x06 ----------------------

    fn raw_desc(tag: u8, data: Vec<u8>) -> crate::mpegts::descriptors::RawDescriptor {
        crate::mpegts::descriptors::RawDescriptor { tag, data }
    }

    #[test]
    fn classify_0x06_subtitling_descriptor_wins() {
        // subtitling_descriptor (tag 0x59) — body shape per ETSI EN 300 468:
        // ISO 639 lang (3) + subtitling_type (1) + composition_page_id (2) +
        // ancillary_page_id (2). Content irrelevant to classification.
        let descs = vec![raw_desc(
            0x59,
            vec![b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01],
        )];
        assert_eq!(
            classify_0x06(&descs),
            StreamKind::Subtitle(SubtitleCodec::DvbSubtitling)
        );
    }

    #[test]
    fn classify_0x06_teletext_descriptor_wins() {
        // teletext_descriptor (tag 0x56) — body: ISO 639 lang (3) +
        // (teletext_type<<3 | teletext_magazine_number) + teletext_page_number.
        let descs = vec![raw_desc(
            0x56,
            vec![b'e', b'n', b'g', (0x02 << 3) | 1, 0x88],
        )];
        assert_eq!(
            classify_0x06(&descs),
            StreamKind::Subtitle(SubtitleCodec::DvbTeletext)
        );
    }

    #[test]
    fn classify_0x06_vbi_teletext_descriptor_also_classifies_teletext() {
        // VBI_teletext_descriptor (tag 0x46) — same outcome as 0x56.
        let descs = vec![raw_desc(0x46, vec![])];
        assert_eq!(
            classify_0x06(&descs),
            StreamKind::Subtitle(SubtitleCodec::DvbTeletext)
        );
    }

    #[test]
    fn classify_0x06_vttc_format_identifier_classifies_webvtt() {
        let descs = vec![raw_desc(0x05, b"VTTC".to_vec())];
        assert_eq!(
            classify_0x06(&descs),
            StreamKind::Subtitle(SubtitleCodec::WebVttInTs)
        );
    }

    #[test]
    fn classify_0x06_ga94_format_identifier_classifies_cea708_standalone() {
        let descs = vec![raw_desc(0x05, b"GA94".to_vec())];
        assert_eq!(
            classify_0x06(&descs),
            StreamKind::Subtitle(SubtitleCodec::Cea708Standalone)
        );
    }

    #[test]
    fn classify_0x06_klva_still_klv_async_regression_guard() {
        let descs = vec![raw_desc(0x05, b"KLVA".to_vec())];
        assert_eq!(classify_0x06(&descs), StreamKind::KlvAsync);
    }

    #[test]
    fn classify_0x06_av1_registration_takes_priority() {
        let descs = vec![raw_desc(0x05, b"AV01".to_vec())];
        assert_eq!(classify_0x06(&descs), StreamKind::Video(VideoCodec::Av1));
    }

    #[test]
    fn classify_0x06_av01_wins_over_klva_and_subtitle_arms() {
        // AV01 registration alongside (hypothetical, spec-violating) KLVA
        // registration — AV01 wins per descriptor exclusivity (binding §2.1).
        let descs = vec![
            raw_desc(0x05, b"AV01".to_vec()),
            raw_desc(0x05, b"KLVA".to_vec()),
        ];
        assert_eq!(classify_0x06(&descs), StreamKind::Video(VideoCodec::Av1));
    }

    #[test]
    fn classify_0x06_emits_ambiguous_on_subtitling_plus_vttc() {
        // PMT entry with both subtitling_descriptor (0x59) AND VTTC
        // registration — ambiguous which subtitle codec actually rides on
        // the PID. Classifier still picks subtitling per first-match
        // priority; ambiguity helper surfaces both markers.
        let descs = vec![
            raw_desc(0x59, vec![b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01]),
            raw_desc(0x05, b"VTTC".to_vec()),
        ];
        let (kind, ambiguous_tags) = classify_0x06_with_ambiguity(&descs);
        assert_eq!(kind, StreamKind::Subtitle(SubtitleCodec::DvbSubtitling));
        assert_eq!(
            ambiguous_tags,
            vec![0x59, 0xF0],
            "ambiguity reports both 0x59 and synthetic 0xF0 (VTTC)"
        );
    }

    #[test]
    fn classify_0x06_no_ambiguity_when_single_marker() {
        let descs = vec![raw_desc(0x05, b"VTTC".to_vec())];
        let (kind, ambiguous_tags) = classify_0x06_with_ambiguity(&descs);
        assert_eq!(kind, StreamKind::Subtitle(SubtitleCodec::WebVttInTs));
        assert!(ambiguous_tags.is_empty());
    }

    #[test]
    fn classify_0x06_no_ambiguity_when_no_markers() {
        // No recognized markers — empty tag list, falls through to Unknown.
        let descs: Vec<crate::mpegts::descriptors::RawDescriptor> = vec![];
        let (kind, ambiguous_tags) = classify_0x06_with_ambiguity(&descs);
        assert_eq!(kind, StreamKind::Unknown(0x06));
        assert!(ambiguous_tags.is_empty());
    }

    #[test]
    fn classify_0x06_ambiguous_teletext_synonyms_count_once() {
        // 0x56 and 0x46 are teletext synonyms — one marker total even when
        // both are present. Combined with VTTC, that's two markers.
        let descs = vec![
            raw_desc(0x56, vec![b'e', b'n', b'g', (0x02 << 3) | 1, 0x88]),
            raw_desc(0x46, vec![]),
            raw_desc(0x05, b"VTTC".to_vec()),
        ];
        let (kind, ambiguous_tags) = classify_0x06_with_ambiguity(&descs);
        assert_eq!(kind, StreamKind::Subtitle(SubtitleCodec::DvbTeletext));
        assert_eq!(ambiguous_tags, vec![0x56, 0xF0]);
    }

    // -- is_malformed_av1_registration -----------------------------------------

    #[test]
    fn malformed_av01_registration_truncated_to_three_bytes() {
        // Tag 0x05, length 3 (well-formed at walk time), contents "AV0".
        // This is the "tried to be AV01 but truncated" case.
        let descs = vec![raw_desc(0x05, b"AV0".to_vec())];
        assert!(is_malformed_av1_registration(&descs));
    }

    #[test]
    fn well_formed_av01_registration_not_flagged() {
        let descs = vec![raw_desc(0x05, b"AV01".to_vec())];
        assert!(!is_malformed_av1_registration(&descs));
    }

    #[test]
    fn other_short_registration_not_flagged() {
        // KLVA truncated — doesn't start with "AV" so not flagged.
        let descs = vec![raw_desc(0x05, b"KL".to_vec())];
        assert!(!is_malformed_av1_registration(&descs));
    }

    #[test]
    fn empty_descriptors_not_flagged() {
        assert!(!is_malformed_av1_registration(&[]));
    }

    #[test]
    fn demuxer_emits_subtitle_sample_for_webvtt_pes() {
        use crate::mpegts::demux::event::SubtitleCodec as DemuxSubtitleCodec;
        use crate::mpegts::mux::{
            MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSubtitleCodec, VideoCodec,
        };

        // Mux: single-program with one video stream and one WebVTT subtitle
        // stream. Video is required because MuxerConfig::validate enforces at least
        // one video or KLV per program; subtitle alone wouldn't be a valid
        // program shape.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_subtitle(0x200, MuxSubtitleCodec::WebVttInTs);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut muxer = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let h = muxer.subtitle_handles()[0];
        let cue = b"WEBVTT\n\nx-cue\n";
        muxer
            .push_subtitle_to(h, Pts90khz::new(90_000), cue)
            .unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        buf.truncate(n);

        // Demux: feed the muxed bytes, capture events.
        let mut demuxer = Demuxer::new();
        demuxer.feed(&buf).unwrap();
        demuxer.flush();
        let mut events = Vec::new();
        while let Some(e) = demuxer.next_event() {
            events.push(e);
        }

        let sample = events
            .iter()
            .find(|e| {
                matches!(
                    e,
                    DemuxEvent::Sample {
                        payload: SamplePayload::Subtitle { .. },
                        ..
                    }
                )
            })
            .expect("subtitle Sample event present");

        if let DemuxEvent::Sample {
            stream,
            pts,
            dts,
            payload: event_payload,
        } = sample
        {
            assert_eq!(stream.pid, 0x200);
            assert!(matches!(
                stream.kind,
                StreamKind::Subtitle(DemuxSubtitleCodec::WebVttInTs)
            ));
            assert_eq!(pts.as_ticks(), 90_000);
            assert_eq!(*dts, None, "subtitles have no DTS (no B-frame reorder)");
            if let SamplePayload::Subtitle { codec, payload } = event_payload {
                assert_eq!(*codec, DemuxSubtitleCodec::WebVttInTs);
                assert!(
                    payload.windows(6).any(|w| w == b"WEBVTT"),
                    "WEBVTT signature recovered from payload"
                );
            }
        }
    }

    #[test]
    fn demuxer_stats_increments_subtitle_streams_seen() {
        use crate::mpegts::mux::{
            MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec, VideoCodec as MuxVideoCodec,
        };

        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
            prog.add_video(0x101, MuxVideoCodec::H264);
            prog.add_subtitle(0x200, SubtitleCodec::WebVttInTs);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut mux = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let h = mux.subtitle_handles()[0];
        // Push twice on the same PID — the dedupe HashSet should keep
        // subtitle_streams_seen at 1 (one distinct PID seen).
        mux.push_subtitle_to(h, Pts90khz::new(90_000), b"WEBVTT\n")
            .unwrap();
        mux.push_subtitle_to(h, Pts90khz::new(180_000), b"WEBVTT\n\n")
            .unwrap();
        let mut bytes = vec![0u8; 188 * 64];
        let n = mux.pull(&mut bytes);
        bytes.truncate(n);

        let mut demux = Demuxer::new();
        demux.feed(&bytes).unwrap();
        demux.flush();
        while demux.next_event().is_some() {}
        let s = demux.stats();
        assert_eq!(s.subtitle_streams_seen, 1);
        let stream_stat = s.per_stream.get(&0x200).unwrap();
        assert_eq!(stream_stat.label.as_deref(), Some("WebVTT-in-TS"));
        assert!(stream_stat.items >= 1);

        // reset_stats clears the dedup set + counter so a fresh sample
        // would re-bump.
        demux.reset_stats();
        let s = demux.stats();
        assert_eq!(s.subtitle_streams_seen, 0);
    }

    // last_seen is a std-only field (wall clock unavailable under no_std) —
    // gate this test to match, same convention as MuxSender::into_inner's
    // tests.
    #[cfg(feature = "std")]
    #[test]
    fn stats_last_seen_stamped_on_emit() {
        use crate::mpegts::mux::{
            KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
        };

        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x101, MuxVideoCodec::H264);
            prog.add_klv(0x102, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut mux = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let mut demux = Demuxer::new();

        // Feed stream A (video, 0x101) and drain its event in its own
        // feed/drain cycle, then do the same for stream B (KLV, 0x102) —
        // two strictly-sequential batches, so PES emission order is
        // pinned to feed-call order regardless of how the muxer
        // interleaves TS packets from concurrently-pushed streams within
        // a single pull().
        let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        mux.push_video(nal, Pts90khz::new(0), true).unwrap();
        let mut bytes_a = vec![0u8; 188 * 64];
        let n = mux.pull(&mut bytes_a);
        bytes_a.truncate(n);
        demux.feed(&bytes_a).unwrap();
        demux.flush();
        while demux.next_event().is_some() {}
        let a = demux.stats().per_stream[&0x101]
            .last_seen
            .expect("video PES was emitted — last_seen must be Some");

        let klv: &[u8] = &[
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x00,
        ];
        mux.push_klv(klv, Pts90khz::new(0), 0x00).unwrap();
        let mut bytes_b = vec![0u8; 188 * 64];
        let n = mux.pull(&mut bytes_b);
        bytes_b.truncate(n);
        demux.feed(&bytes_b).unwrap();
        demux.flush();
        while demux.next_event().is_some() {}
        let b = demux.stats().per_stream[&0x102]
            .last_seen
            .expect("KLV PES was emitted — last_seen must be Some");

        assert!(
            a <= b,
            "stream B was emitted after stream A: last_seen must be non-decreasing"
        );
    }

    /// A malicious PMT PUSI claiming `section_length=0xFFF` (4095 bytes,
    /// total = 4098 > 4096 cap) and never closing must NOT cause unbounded
    /// buffer growth — ffmpeg caps at 4096 (`MAX_SECTION_SIZE` in
    /// `mpegts.h:34`). On overflow we discard the partial section and emit
    /// `NonConformantIssue::PsiOverlongSection`.
    ///
    /// We pre-seed the demuxer with a PAT that maps program 1 → PMT PID
    /// 0x100 so that `process_packet` routes the malicious packet to the
    /// PMT-PID branch of `handle_psi`. Without this, packets on PID 0x100
    /// would be silently ignored (PID not in PAT, not in stream_kind_by_pid).
    #[test]
    fn psi_buffer_caps_at_4kib_and_emits_psi_overlong_section() {
        let mut demux = Demuxer::new();

        // ── Step 1: feed a PAT that announces program 1 on PMT PID 0x100. ──
        // PAT section bytes (per ISO 13818-1 §2.4.4.3):
        //   table_id=0x00,
        //   syntax=1, '0', reserved=11, section_length (12 bits),
        //   transport_stream_id (16 bits),
        //   reserved=11, version=0, current_next=1,
        //   section_number=0, last_section_number=0,
        //   { program_number=1, reserved=111, pid=0x100 } * 1,
        //   CRC32 (4 bytes).
        // section_length spans from after section_length itself through CRC,
        // so = 5 (header tail) + 4 (program loop) + 4 (CRC) = 13 (0x0D).
        let mut pat = vec![
            0x00, // table_id
            0xB0, 0x0D, // syntax=1, section_length=0x00D
            0x00, 0x01, // transport_stream_id=1
            0xC1, // reserved=11, version=0, current_next=1
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number=1
            0xE1, 0x00, // reserved=111, pid=0x100
        ];
        let crc = crate::mpegts::common::crc32::crc32_mpeg2(&pat);
        pat.extend_from_slice(&crc.to_be_bytes());

        // Wrap PAT into a single TS packet on PID 0x0000.
        let mut ts_pat = [0xFFu8; 188];
        ts_pat[0] = 0x47;
        ts_pat[1] = 0x40; // PUSI=1, pid_high=0
        ts_pat[2] = 0x00; // pid_low=0
        ts_pat[3] = 0x10; // adaptation=01, cc=0
        ts_pat[4] = 0x00; // pointer_field=0
        ts_pat[5..5 + pat.len()].copy_from_slice(&pat);
        demux.feed(&ts_pat).expect("PAT feed");

        // Drain any events emitted by the PAT (no PMT yet → no ProgramMap).
        while demux.next_event().is_some() {}

        // ── Step 2: feed N packets on PMT PID 0x100 — first PUSI claims a
        // section_length=0xFFF (4095 bytes), total declared length 4098 >
        // 4096 cap. The assembler should reject this on the FIRST packet
        // (declared_too_long path).
        let mut ts_pmt = [0xFFu8; 188];
        ts_pmt[0] = 0x47;
        ts_pmt[1] = 0x41; // PUSI=1, pid_high=1
        ts_pmt[2] = 0x00; // pid_low=0 → PID = 0x100
        ts_pmt[3] = 0x10; // adaptation=01, cc=0
        ts_pmt[4] = 0x00; // pointer_field=0
        // table_id=0x02 (PMT), syntax=1 + section_length=0xFFF (4095).
        ts_pmt[5] = 0x02;
        ts_pmt[6] = 0xBF; // 1011 1111 → syntax=1, '0'=0, reserved=11, sl_hi=0xF
        ts_pmt[7] = 0xFF; // sl_lo=0xFF → section_length=0xFFF=4095
        // Remaining 181 bytes are arbitrary attacker payload.

        demux.feed(&ts_pmt).expect("malicious PMT feed");

        // ── Step 3: assert PsiOverlongSection emitted. ──
        let mut saw_overlong = false;
        while let Some(ev) = demux.next_event() {
            if let DemuxEvent::NonConformant {
                issue: NonConformantIssue::PsiOverlongSection { pid, observed_len },
                ..
            } = ev
            {
                assert_eq!(pid, 0x100);
                assert_eq!(observed_len, 4098); // 3 + 0xFFF
                saw_overlong = true;
            }
        }
        assert!(
            saw_overlong,
            "expected PsiOverlongSection on PID 0x100 from malicious PMT"
        );
    }

    /// Per ISO/IEC 13818-1 §2.4.3.2, bit 0x80 of byte 1 (transport_error_indicator)
    /// marks a packet as link-layer-corrupt. ffmpeg drops these
    /// (mpegts.c:3091-3097); we must too — silently feeding the payload to
    /// PES/PSI reassembly produces garbage parse output downstream.
    ///
    /// We pre-seed the demuxer with a PAT that maps program 1 → PMT PID 0x100
    /// so that without the TEI drop, the packet would route to the PMT-PID
    /// branch of `handle_psi` and PSI parsing would be observable.
    #[test]
    fn tei_packets_are_dropped_with_non_conformant_event() {
        let mut demux = Demuxer::new();

        // Seed a PAT that announces program 1 on PMT PID 0x100. (Same
        // construction as in `psi_buffer_caps_at_4kib_and_emits_psi_overlong_section`.)
        let mut pat = vec![
            0x00, // table_id
            0xB0, 0x0D, // syntax=1, section_length=0x00D
            0x00, 0x01, // transport_stream_id=1
            0xC1, // reserved=11, version=0, current_next=1
            0x00, 0x00, // section_number, last_section_number
            0x00, 0x01, // program_number=1
            0xE1, 0x00, // reserved=111, pid=0x100
        ];
        let crc = crate::mpegts::common::crc32::crc32_mpeg2(&pat);
        pat.extend_from_slice(&crc.to_be_bytes());

        let mut ts_pat = [0xFFu8; 188];
        ts_pat[0] = 0x47;
        ts_pat[1] = 0x40; // PUSI=1, pid_high=0
        ts_pat[2] = 0x00; // pid_low=0
        ts_pat[3] = 0x10; // adaptation=01, cc=0
        ts_pat[4] = 0x00; // pointer_field=0
        ts_pat[5..5 + pat.len()].copy_from_slice(&pat);
        demux.feed(&ts_pat).expect("PAT feed");

        // Drain PAT-induced events.
        while demux.next_event().is_some() {}

        // Build a TS packet with PUSI=1, CC=0, PID=0x100, AND TEI=1.
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0xC1; // TEI=1, PUSI=1, transport_priority=0, pid_high=1
        pkt[2] = 0x00; // pid_low=0 → PID=0x100
        pkt[3] = 0x10; // adaptation=01, cc=0
        pkt[4] = 0x00; // pointer_field=0
        // Put a valid-looking PMT prefix in the payload so we'd ASSUME PSI
        // parsing would happen if the packet weren't dropped.
        pkt[5] = 0x02; // table_id (PMT)
        pkt[6] = 0xB0; // section_syntax_indicator=1, reserved=11, sl_hi=0
        pkt[7] = 0x05; // sl_lo=5

        demux.feed(&pkt).expect("TEI feed");

        // Expect TransportErrorPacket on PID 0x100, NO PSI parse.
        let mut saw_tei = false;
        let mut saw_psi = false;
        while let Some(ev) = demux.next_event() {
            match ev {
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::TransportErrorPacket { pid: 0x100 },
                    ..
                } => {
                    saw_tei = true;
                }
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PsiOverlongSection { .. },
                    ..
                }
                | DemuxEvent::ProgramMap(_) => {
                    saw_psi = true;
                }
                _ => {}
            }
        }
        assert!(saw_tei, "expected TransportErrorPacket on PID 0x100");
        assert!(!saw_psi, "TEI packet should be dropped before PSI parsing");
    }

    #[test]
    fn scrambled_packet_emits_nonconformant_and_is_not_routed() {
        // A PAT packet (PID 0x0000) with transport_scrambling_control != 0 must
        // NOT be parsed into program topology; it surfaces UnsupportedScrambling.
        let mut demux = Demuxer::new();
        let mut pat = pat_packet_with_programs(&[(1, 0x1000)], 0);
        pat[3] |= 0b0100_0000; // set TSC=01 (byte 3 bits 7-6) on the PAT packet
        demux.feed(&pat).unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demux.next_event()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::UnsupportedScrambling { control: 1, .. },
                    ..
                }
            )),
            "expected UnsupportedScrambling, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DemuxEvent::ProgramMap(_))),
            "scrambled PAT must not be adopted"
        );
    }

    /// Per ISO/IEC 13818-1 §2.4.3.5, when adaptation_field.discontinuity_indicator=1
    /// the CC is *allowed* to be discontinuous on that packet. ffmpeg
    /// suppresses the CC error in that case (mpegts.c:3075-3078). We must
    /// too — emitting both `DiscontinuityKind::AdaptationFieldFlag` AND
    /// `DiscontinuityKind::ContinuityJump` double-counts the same event in
    /// stats and confuses strict-mode consumers.
    #[test]
    fn discontinuity_indicator_suppresses_continuity_jump_event() {
        use crate::mpegts::mux::{
            MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
        };
        // Build a real PAT+PMT+video PES through the muxer so the demuxer's
        // PSI tables get populated for PID 0x100 and `cc_by_pid` is primed.
        // This is the same pattern already used by other unit tests in this
        // module (e.g. `demuxer_emits_audio_sample_for_aac_pes`) — we need
        // PSI parsed for `lookup_stream(0x100)` to resolve and for the CC
        // tracker to have a baseline against which to detect a jump.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, MuxVideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut mux = crate::mpegts::mux::Muxer::new(cfg).unwrap();
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        au.extend(core::iter::repeat(0xAB).take(64));
        mux.push_video(&au, Pts90khz::new(9_000), true).unwrap();
        let mut buf = vec![0u8; 188 * 64];
        let n = mux.pull(&mut buf);

        let mut demux = Demuxer::new();
        demux.feed(&buf[..n]).expect("seed feed");

        // Find the last CC value emitted by the muxer for PID 0x100 so we
        // can construct a guaranteed-jumping CC on our synthetic packet.
        // Walk the seeded bytes packet-by-packet; CC is the low nibble of
        // byte 3.
        let mut last_cc = 0u8;
        for i in (0..n).step_by(188) {
            if buf[i] != 0x47 {
                continue;
            }
            let pid = ((u16::from(buf[i + 1] & 0x1F)) << 8) | u16::from(buf[i + 2]);
            if pid == 0x100 {
                last_cc = buf[i + 3] & 0x0F;
            }
        }
        // Pick a CC that is NOT (last_cc + 1) & 0x0F — bump by 5.
        let bad_cc = (last_cc.wrapping_add(5)) & 0x0F;

        // Drain events queued from the seed feed so we only observe events
        // produced by our synthetic packet.
        while demux.next_event().is_some() {}

        // Build a TS packet on PID 0x100 with adaptation_field present,
        // discontinuity_indicator=1, and a CC that has jumped relative to
        // the last muxer-emitted CC for this PID.
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        // PUSI=0, PID high bits = 1 (PID 0x100).
        pkt[1] = 0x01;
        pkt[2] = 0x00;
        // adaptation_field_control = '11' (both AF + payload).
        pkt[3] = 0x30 | (bad_cc & 0x0F);
        // adaptation_field_length = 1 (just the flags byte).
        pkt[4] = 1;
        // Flags byte: bit 7 = discontinuity_indicator.
        pkt[5] = 0x80;
        // Bytes 6..188 are payload — left as 0xFF (the buffer init).

        demux.feed(&pkt).expect("DI packet feed");

        let mut cc_jumps = 0usize;
        let mut di_events = 0usize;
        while let Some(ev) = demux.next_event() {
            if let DemuxEvent::Discontinuity {
                stream: StreamId { pid: 0x100, .. },
                kind,
            } = ev
            {
                match kind {
                    DiscontinuityKind::ContinuityJump { .. } => cc_jumps += 1,
                    DiscontinuityKind::AdaptationFieldFlag => di_events += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(
            cc_jumps, 0,
            "ContinuityJump must be suppressed when discontinuity_indicator=1"
        );
        assert!(
            di_events >= 1,
            "AdaptationFieldFlag event should still fire (got {di_events})"
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

    /// Audit finding (Demux-C): the muxer wraps DVB-sub PES payloads in the
    /// EN 300 743 §6.2 envelope (`0x20 + 0x00 + segments + 0xFF`), so the
    /// demuxer must strip that envelope before surfacing to callers. Without
    /// the strip, libavcodec's `dvbsubdec` rejects the buffer at
    /// `buf_size <= 6 || *buf != 0x0f`.
    #[test]
    fn dvb_sub_demux_strips_pes_data_field_envelope() {
        use crate::mpegts::demux::event::SamplePayload;
        use crate::mpegts::mux::{
            Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec as MuxSubtitleCodec,
            VideoCodec as MuxVideoCodec,
        };

        // Configure: one program with one H.264 video stream (PCR carrier)
        // plus one DVB-sub stream. Subtitles can't be the PCR PID, so the
        // video stream is required for a valid program.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x101, MuxVideoCodec::H264);
            prog.add_subtitle(
                0x200,
                MuxSubtitleCodec::DvbSubtitling {
                    language: *b"eng",
                    subtitling_type: 0x10,
                    composition_page_id: 0x0001,
                    ancillary_page_id: 0x0002,
                },
            );
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let mut mux = Muxer::new(cfg).unwrap();

        // Push a video AU first so PCR fires and PSI emits.
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        au.extend(core::iter::repeat_n(0xAB, 64));
        mux.push_video(&au, Pts90khz::new(9_000), true).unwrap();

        // Push raw DVB-sub segment bytes; muxer auto-prepends §6.2 envelope.
        let segment_bytes = [0x0Fu8, 0x10, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10];
        mux.push_subtitle(Pts90khz::new(9_000), &segment_bytes)
            .unwrap();

        // Drain all queued packets.
        let mut all = Vec::new();
        let mut buf = vec![0u8; 188 * 256];
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }

        let mut demux = Demuxer::new();
        demux.feed(&all).expect("feed");
        demux.flush();

        let mut subtitle_payload: Option<Vec<u8>> = None;
        while let Some(ev) = demux.next_event() {
            if let DemuxEvent::Sample {
                payload:
                    SamplePayload::Subtitle {
                        codec: SubtitleCodec::DvbSubtitling,
                        payload,
                    },
                ..
            } = ev
            {
                subtitle_payload = Some(payload.to_vec());
                break;
            }
        }
        let payload = subtitle_payload.expect("DVB-sub Sample event not found");

        // Surfaced payload is exactly the segment bytes — no leading
        // 0x20 + 0x00 envelope, no trailing 0xFF marker.
        assert_eq!(
            payload,
            segment_bytes.to_vec(),
            "EN 300 743 §6.2 envelope should be stripped"
        );
    }

    // -------------------------------------------------------------------------
    // White-box PAT/PMT diffing tests — use programs_for_test()
    // -------------------------------------------------------------------------
    //
    // Moved here from crates/tst-core/tests/mpegts_demux_multi_program.rs so
    // that programs_for_test can be pub(crate) + #[cfg(test)] rather than pub.

    /// Synthesise a well-formed PAT TS packet for tests.
    fn pat_packet_with_programs(programs: &[(u16, u16)], version: u8) -> Vec<u8> {
        let section_length = 5 + 4 * programs.len() + 4;
        let mut sec: Vec<u8> = Vec::with_capacity(3 + section_length);
        sec.push(0x00); // table_id = PAT
        sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        sec.push((section_length & 0xFF) as u8);
        sec.push(0x00); // transport_stream_id hi
        sec.push(0x01); // transport_stream_id lo
        sec.push(0xC1 | ((version & 0x1F) << 1)); // reserved | version | current_next
        sec.push(0x00); // section_number
        sec.push(0x00); // last_section_number
        for &(pn, pmt_pid) in programs {
            sec.push((pn >> 8) as u8);
            sec.push((pn & 0xFF) as u8);
            sec.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
            sec.push((pmt_pid & 0xFF) as u8);
        }
        let crc = crc32_mpeg2_test(&sec);
        sec.push((crc >> 24) as u8);
        sec.push((crc >> 16) as u8);
        sec.push((crc >> 8) as u8);
        sec.push(crc as u8);
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40; // PUSI | PAT PID hi = 0
        pkt[2] = 0x00; // PAT PID lo
        pkt[3] = 0x10; // payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        let sec_end = 5 + sec.len();
        assert!(sec_end <= 188);
        pkt[5..sec_end].copy_from_slice(&sec);
        pkt
    }

    /// CRC-32/MPEG-2 helper for test packet builders.
    fn crc32_mpeg2_test(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                if crc & 0x8000_0000 != 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc
    }

    /// Synthesise a well-formed PMT TS packet for tests.
    fn pmt_packet_for_test(
        pmt_pid: u16,
        program_number: u16,
        pcr_pid: u16,
        streams: &[(u8, u16)],
        version: u8,
    ) -> Vec<u8> {
        let stream_loop_len = 5 * streams.len();
        let section_length = 9 + stream_loop_len + 4;
        let mut sec: Vec<u8> = Vec::with_capacity(3 + section_length);
        sec.push(0x02); // table_id = PMT
        sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        sec.push((section_length & 0xFF) as u8);
        sec.push((program_number >> 8) as u8);
        sec.push((program_number & 0xFF) as u8);
        sec.push(0xC0 | ((version & 0x1F) << 1) | 1); // reserved | version | cni
        sec.push(0x00); // section_number
        sec.push(0x00); // last_section_number
        sec.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
        sec.push((pcr_pid & 0xFF) as u8);
        sec.push(0xF0); // program_info_length hi
        sec.push(0x00); // program_info_length lo (no descriptors)
        for &(stream_type, pid) in streams {
            sec.push(stream_type);
            sec.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            sec.push((pid & 0xFF) as u8);
            sec.push(0xF0); // es_info_length hi
            sec.push(0x00); // es_info_length lo
        }
        let crc = crc32_mpeg2_test(&sec);
        sec.push((crc >> 24) as u8);
        sec.push((crc >> 16) as u8);
        sec.push((crc >> 8) as u8);
        sec.push(crc as u8);
        let mut pkt = vec![0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F); // PUSI + PID hi
        pkt[2] = (pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10; // payload-only, CC=0
        pkt[4] = 0x00; // pointer_field
        let sec_end = 5 + sec.len();
        assert!(sec_end <= 188);
        pkt[5..sec_end].copy_from_slice(&sec);
        pkt
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

    // --- DEMUX-01 regression tests (multi-section PSI handling) ---

    /// Helper: build a PAT TS packet for one section of a multi-section table.
    ///
    /// `section_number` and `last_section_number` are written into bytes [6]
    /// and [7] of the section respectively; the CRC is recomputed so the
    /// section is otherwise well-formed.
    fn pat_packet_with_multi_section(
        section_number: u8,
        last_section_number: u8,
        programs: &[(u16, u16)],
    ) -> Vec<u8> {
        // Version 0, same transport_stream_id as pat_packet_with_programs (0x0001).
        let mut pkt = pat_packet_with_programs(programs, 0);
        // Section bytes start at offset 5 (sync + 3 TS header bytes + pointer_field).
        // Byte [6] = section_number, byte [7] = last_section_number.
        pkt[5 + 6] = section_number;
        pkt[5 + 7] = last_section_number;
        // Recompute CRC over the section sans its 4-byte trailer.
        let section_len = 3 + (((pkt[5 + 1] as usize & 0x0F) << 8) | pkt[5 + 2] as usize);
        let section_end = 5 + section_len;
        let crc = crc32_mpeg2_test(&pkt[5..section_end - 4]);
        pkt[section_end - 4..section_end].copy_from_slice(&crc.to_be_bytes());
        pkt
    }

    /// Helper: build a PMT TS packet whose section has `last_section_number=1`.
    fn pmt_packet_with_multi_section(
        pmt_pid: u16,
        program_number: u16,
        pcr_pid: u16,
        streams: &[(u8, u16)],
        version: u8,
    ) -> Vec<u8> {
        let mut pkt = pmt_packet_for_test(pmt_pid, program_number, pcr_pid, streams, version);
        pkt[5 + 7] = 0x01;
        let section_len = 3 + (((pkt[5 + 1] as usize & 0x0F) << 8) | pkt[5 + 2] as usize);
        let section_end = 5 + section_len;
        let crc = crc32_mpeg2_test(&pkt[5..section_end - 4]);
        pkt[section_end - 4..section_end].copy_from_slice(&crc.to_be_bytes());
        pkt
    }

    /// Like `pat_packet_with_programs` but sets the 4-bit CC field to `cc`.
    ///
    /// Use this in tests that feed two PAT packets on the same PID: the second
    /// packet must use `cc=1` (or any value != the first) so that DA-DEMUX-1's
    /// spec-legal duplicate suppression does not swallow it.
    fn pat_packet_with_programs_cc(programs: &[(u16, u16)], version: u8, cc: u8) -> Vec<u8> {
        let mut pkt = pat_packet_with_programs(programs, version);
        pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
        pkt
    }

    /// Like `pmt_packet_for_test` but sets the 4-bit CC field to `cc`.
    ///
    /// Use this in tests that feed two PMT packets on the same PID.
    fn pmt_packet_for_test_cc(
        pmt_pid: u16,
        program_number: u16,
        pcr_pid: u16,
        streams: &[(u8, u16)],
        version: u8,
        cc: u8,
    ) -> Vec<u8> {
        let mut pkt = pmt_packet_for_test(pmt_pid, program_number, pcr_pid, streams, version);
        pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
        pkt
    }

    /// Like `pat_packet_with_multi_section` but sets the 4-bit CC field to `cc`.
    ///
    /// Use this when feeding multiple sections on PID 0x0000 in the same test.
    fn pat_packet_with_multi_section_cc(
        section_number: u8,
        last_section_number: u8,
        programs: &[(u16, u16)],
        cc: u8,
    ) -> Vec<u8> {
        let mut pkt = pat_packet_with_multi_section(section_number, last_section_number, programs);
        pkt[3] = 0x10 | (cc & 0x0F); // payload-only, CC=cc
        pkt
    }

    fn drain_all_events(d: &mut Demuxer) -> Vec<DemuxEvent> {
        let mut events = Vec::new();
        while let Some(e) = d.next_event() {
            events.push(e);
        }
        events
    }

    /// REF-PSI-02: a complete 2-section PAT must reassemble such that BOTH
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

    /// REF-PSI-02: an INCOMPLETE multi-section PAT (only section 0 of 2) must
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

    // -------------------------------------------------------------------------
    // PAT cleanup on program removal (validate-1 B8)
    // -------------------------------------------------------------------------
    //
    // When PAT removes a program, per-PID state for that program's PIDs is
    // unreachable and must be cleaned. White-box tests inspect the private
    // per-PID maps directly via `pub(super)` field access.

    /// Build a 188-byte TS packet carrying a PCR via the adaptation field.
    fn pcr_packet_for_test(pid: u16, pcr_27mhz: u64) -> [u8; 188] {
        let base: u64 = pcr_27mhz / 300;
        let ext: u64 = pcr_27mhz % 300;
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (pid >> 8) as u8 & 0x1F;
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x20; // adaptation_field_control = 0b10 (af only), CC=0
        buf[4] = 183; // af_length fills the rest
        buf[5] = 0x10; // PCR_flag=1
        buf[6] = (base >> 25) as u8;
        buf[7] = (base >> 17) as u8;
        buf[8] = (base >> 9) as u8;
        buf[9] = (base >> 1) as u8;
        buf[10] = (((base & 0x01) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
        buf[11] = (ext & 0xFF) as u8;
        buf
    }

    /// Build a payload-only TS packet on `pid` with the given CC. Drives
    /// `cc_by_pid` registration without needing a PES header.
    fn payload_packet_for_test(pid: u16, cc: u8) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (pid >> 8) as u8 & 0x1F;
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x10 | (cc & 0x0F); // payload-only + CC
        buf
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
        assert!(demuxer.cc_by_pid.contains_key(&0x1011));
        assert!(demuxer.cc_by_pid.contains_key(&0x1111));
        assert!(demuxer.cc_by_pid.contains_key(&0x1100)); // PMT PID

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        assert!(
            demuxer.cc_by_pid.contains_key(&0x1011),
            "program 1's PID 0x1011 cc must survive"
        );
        assert!(
            !demuxer.cc_by_pid.contains_key(&0x1111),
            "program 2's PID 0x1111 cc must be cleared"
        );
        assert!(
            !demuxer.cc_by_pid.contains_key(&0x1100),
            "program 2's PMT PID 0x1100 cc must be cleared"
        );
    }

    #[test]
    fn pat_removed_program_clears_last_pcr_by_pid() {
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
        demuxer
            .feed(&pcr_packet_for_test(0x1011, 90_000_000))
            .unwrap();
        demuxer
            .feed(&pcr_packet_for_test(0x1111, 90_000_000))
            .unwrap();
        assert!(demuxer.last_pcr_by_pid.contains_key(&0x1011));
        assert!(demuxer.last_pcr_by_pid.contains_key(&0x1111));

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        assert!(
            demuxer.last_pcr_by_pid.contains_key(&0x1011),
            "program 1's PCR must survive"
        );
        assert!(
            !demuxer.last_pcr_by_pid.contains_key(&0x1111),
            "program 2's PCR must be cleared"
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
        assert!(demuxer.stream_kind_by_pid.contains_key(&0x1111));
        assert!(demuxer.pid_to_program.contains_key(&0x1111));

        // PAT v1 drops program 2 (CC=1 so it isn't swallowed as a duplicate of v0).
        demuxer
            .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
            .unwrap();

        // Pre-existing behavior (already in place before B8) — sanity that
        // we haven't broken what was working.
        assert!(
            !demuxer.stream_kind_by_pid.contains_key(&0x1111),
            "stream_kind_by_pid must drop removed program's PID"
        );
        assert!(
            !demuxer.pid_to_program.contains_key(&0x1111),
            "pid_to_program must drop removed program's PID"
        );
    }

    /// F-01: a PMT version change that drops an elementary PID must clear that
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
        assert!(demuxer.stream_kind_by_pid.contains_key(&0x1011));
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
            !demuxer.stream_kind_by_pid.contains_key(&0x1011),
            "removed PID 0x1011 must no longer route as a stream"
        );
        assert!(
            !demuxer.pid_to_program.contains_key(&0x1011),
            "removed PID 0x1011 must be gone from pid_to_program"
        );
        assert!(
            demuxer.stream_kind_by_pid.contains_key(&0x1012),
            "the surviving new PID 0x1012 must route"
        );
    }

    /// F-02 (lenient): a valid multi-section PAT must NOT surface a false
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

    /// F-02 (strict): `StrictMode::Full` must accept a valid multi-section PAT —
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

    /// F-03: a PAT version that reassigns an existing PMT PID to a different
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

    // -------------------------------------------------------------------------
    // PCR field validation (validate-1 B12)
    // -------------------------------------------------------------------------
    //
    // Parser-level validation lives in `ts.rs` unit tests; these tests cover
    // the demuxer's lenient-mode emission of NonConformantIssue::PcrMalformed.

    #[test]
    fn pcr_malformed_reserved_bits_emits_nonconformant() {
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

        // Drain PAT/PMT events.
        while demuxer.next_event().is_some() {}

        // PCR packet with one reserved bit cleared.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20; // af-only
        buf[4] = 183;
        buf[5] = 0x10; // PCR_flag
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        // byte 10: bit 7 = base lsb (0); reserved mask 0x7E but flip bit 1 to 0 → 0x7C
        buf[10] = 0x7C;
        buf[11] = 0;
        demuxer.feed(&buf).unwrap();

        let mut found = false;
        while let Some(e) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PcrMalformed {
                        kind: crate::mpegts::demux::PcrMalformedKind::InvalidReservedBits,
                    },
                ..
            } = e
            {
                found = true;
            }
        }
        assert!(
            found,
            "expected PcrMalformed::InvalidReservedBits event from corrupt PCR field"
        );
    }

    #[test]
    fn pcr_malformed_extension_out_of_range_emits_nonconformant() {
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
        while demuxer.next_event().is_some() {}

        // PCR with extension = 300 (max valid = 299).
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        // base lsb 0; reserved 0x7E; ext bit 8 = 1 (300 = 0x12C)
        buf[10] = 0x7E | 0x01;
        buf[11] = 0x2C;
        demuxer.feed(&buf).unwrap();

        let mut found = false;
        while let Some(e) = demuxer.next_event() {
            if let DemuxEvent::NonConformant {
                issue:
                    NonConformantIssue::PcrMalformed {
                        kind: crate::mpegts::demux::PcrMalformedKind::ExtensionOutOfRange,
                    },
                ..
            } = e
            {
                found = true;
            }
        }
        assert!(
            found,
            "expected PcrMalformed::ExtensionOutOfRange event from ext=300 PCR field"
        );
    }

    #[test]
    fn pcr_malformed_does_not_seed_last_pcr_by_pid() {
        // A malformed PCR must NOT populate last_pcr_by_pid — otherwise the
        // next valid PCR could fire a spurious PcrAnomaly using the corrupt
        // value as the comparison baseline.
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
        while demuxer.next_event().is_some() {}

        // Feed a malformed PCR.
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0x7C; // reserved bits malformed
        buf[11] = 0;
        demuxer.feed(&buf).unwrap();

        assert!(
            !demuxer.last_pcr_by_pid.contains_key(&0x1011),
            "malformed PCR must not seed last_pcr_by_pid"
        );
    }

    #[test]
    fn reserved_adaptation_control_emits_nonconformant() {
        let mut demux = Demuxer::new();
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x01;
        buf[2] = 0x00;
        buf[3] = 0x00; // control=00
        demux.feed(&buf).unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demux.next_event()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::AdaptationFieldMalformed {
                        kind: crate::mpegts::demux::AdaptationFieldKind::ReservedControl,
                        ..
                    },
                    ..
                }
            )),
            "got {events:?}"
        );
    }

    #[test]
    fn over_long_adaptation_length_emits_nonconformant() {
        let mut demux = Demuxer::new();
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x01;
        buf[2] = 0x00;
        buf[3] = 0x30; // control=11
        buf[4] = 200; // 5 + 200 > 188 -> BadAdaptationLength (previously swallowed)
        demux.feed(&buf).unwrap();
        let events: Vec<_> = core::iter::from_fn(|| demux.next_event()).collect();
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::AdaptationFieldMalformed {
                        kind: crate::mpegts::demux::AdaptationFieldKind::BadLengthForControl,
                        ..
                    },
                    ..
                }
            )),
            "got {events:?}"
        );
    }

    #[test]
    fn pcr_malformed_strict_timing_rejects() {
        // StrictMode::TimingOnly must escalate PcrMalformed to StrictRejection.
        let mut demuxer = Demuxer::with_config(
            DemuxerConfig::builder()
                .strict(StrictMode::TimingOnly)
                .build(),
        );
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
        while demuxer.next_event().is_some() {}

        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (0x1011u16 >> 8) as u8 & 0x1F;
        buf[2] = (0x1011u16 & 0xFF) as u8;
        buf[3] = 0x20;
        buf[4] = 183;
        buf[5] = 0x10;
        buf[6] = 0;
        buf[7] = 0;
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0x7C; // malformed reserved bits
        buf[11] = 0;
        let result = demuxer.feed(&buf);
        assert!(
            matches!(result, Err(DemuxError::StrictRejection(_))),
            "TimingOnly strict mode must reject malformed PCR, got {result:?}"
        );
    }

    // -------------------------------------------------------------------------
    // REF-PES-01: zero PES_packet_length on a non-video stream (WP-D Task 3)
    // -------------------------------------------------------------------------

    #[test]
    fn zero_length_non_video_pes_emits_nonconformant_no_sample() {
        // REF-PES-01: a PES with zero PES_packet_length on an audio PID must
        // surface NonConformantIssue::ZeroLengthPesNonVideo and must NOT emit
        // an audio Sample. stream_type 0x04 = MPEG-1 Audio, stream_id 0xC0.
        const AUDIO_PID: u16 = 0x0101;
        const PMT_PID: u16 = 0x0100;
        const PCR_PID: u16 = 0x0200;

        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, PMT_PID)], 0))
            .unwrap();
        // stream_type 0x04 = MPEG-1 Audio; AUDIO_PID is the audio elementary PID.
        demuxer
            .feed(&pmt_packet_for_test(
                PMT_PID,
                1,
                PCR_PID,
                &[(0x04, AUDIO_PID)],
                0,
            ))
            .unwrap();
        // Drain PAT/PMT events so they don't contaminate the assertion below.
        while demuxer.next_event().is_some() {}

        // Build a PUSI TS packet carrying a PES with stream_id=0xC0 (audio)
        // and PES_packet_length=0 (unbounded — illegal for non-video).
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40 | ((AUDIO_PID >> 8) as u8 & 0x1F); // PUSI + PID hi
        buf[2] = (AUDIO_PID & 0xFF) as u8;
        buf[3] = 0x10; // payload-only, CC=0
        // PES start code + stream_id + zero PES_packet_length
        buf[4] = 0x00; // PES start code prefix byte 1
        buf[5] = 0x00; // PES start code prefix byte 2
        buf[6] = 0x01; // PES start code prefix byte 3
        buf[7] = 0xC0; // stream_id = audio
        buf[8] = 0x00; // PES_packet_length hi = 0 (unbounded — REF-PES-01 violation)
        buf[9] = 0x00; // PES_packet_length lo = 0
        // remaining bytes are 0xFF (pad)
        demuxer.feed(&buf).unwrap();

        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();

        // Must surface ZeroLengthPesNonVideo.
        assert!(
            events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::ZeroLengthPesNonVideo {
                        pid: AUDIO_PID,
                        stream_id: 0xC0,
                    },
                    ..
                }
            )),
            "expected ZeroLengthPesNonVideo NonConformant event; got {events:?}"
        );

        // Must NOT emit a Sample — the bogus partial must be dropped, not flushed.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DemuxEvent::Sample { .. })),
            "zero-length non-video PES must not emit a Sample; got {events:?}"
        );
    }

    #[test]
    fn zero_length_unrecognized_video_stream_type_not_flagged() {
        // REF-PES-01: stream_type 0x02 (ITU-T H.262 / MPEG-2 video) is a VIDEO
        // elementary stream even though tst-core does not parse it (classified
        // StreamKind::Unknown(0x02)). A zero PES_packet_length is legal for any
        // video stream (H.222.0 §2.4.3.7), so it must NOT be flagged as
        // ZeroLengthPesNonVideo — keying the rule on StreamKind::Video alone
        // would wrongly flag conformant MPEG-1/2/4 video.
        const VID_PID: u16 = 0x0201;
        const PMT_PID: u16 = 0x0100;
        const PCR_PID: u16 = 0x0200;

        let mut demuxer = Demuxer::new();
        demuxer
            .feed(&pat_packet_with_programs(&[(1, PMT_PID)], 0))
            .unwrap();
        // stream_type 0x02 = MPEG-2 video → StreamKind::Unknown(0x02).
        demuxer
            .feed(&pmt_packet_for_test(
                PMT_PID,
                1,
                PCR_PID,
                &[(0x02, VID_PID)],
                0,
            ))
            .unwrap();
        while demuxer.next_event().is_some() {}

        // Zero-PES_packet_length PUSI packet on the MPEG-2-video PID (stream_id
        // 0xE0). Legal-for-video, so the demuxer keeps it unbounded (no flag).
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40 | ((VID_PID >> 8) as u8 & 0x1F); // PUSI + PID hi
        buf[2] = (VID_PID & 0xFF) as u8;
        buf[3] = 0x10; // payload-only, CC=0
        buf[4] = 0x00; // PES start code prefix
        buf[5] = 0x00;
        buf[6] = 0x01;
        buf[7] = 0xE0; // stream_id = video
        buf[8] = 0x00; // PES_packet_length = 0 (unbounded — legal for video)
        buf[9] = 0x00;
        demuxer.feed(&buf).unwrap();

        let events: Vec<_> = core::iter::from_fn(|| demuxer.next_event()).collect();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::ZeroLengthPesNonVideo { .. },
                    ..
                }
            )),
            "MPEG-2 video (unrecognized codec) zero-length PES must NOT be \
             flagged as ZeroLengthPesNonVideo; got {events:?}"
        );
    }

    // ── unwrap_timestamps accumulator (opt-in PTS/DTS unwrap) ───────────
    // White-box coverage of `unwrap_pts` / `unwrap_secondary_ts` in
    // isolation. Wire-level (mux -> bytes -> demux) coverage, including
    // the default-off byte-identity guarantee and the missing-PTS guard,
    // lives in `tests/mpegts/pts_unwrap.rs`.

    #[test]
    fn unwrap_pts_first_call_anchors_at_raw_value() {
        let mut d = Demuxer::new();
        let out = d.unwrap_pts(0x100, Pts90khz::new(12_345));
        assert_eq!(out.as_ticks(), 12_345);
        assert_eq!(
            d.unwrap_state.get(&0x100),
            Some(&UnwrapState {
                last_raw: 12_345,
                last_unwrapped: 12_345,
            })
        );
    }

    #[test]
    fn unwrap_pts_forward_progress_within_epoch_does_not_bump_offset() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(1_000));
        let out = d.unwrap_pts(0x100, Pts90khz::new(91_000));
        assert_eq!(out.as_ticks(), 91_000, "no wrap — offset stays 0");
        assert_eq!(
            d.unwrap_state.get(&0x100),
            Some(&UnwrapState {
                last_raw: 91_000,
                last_unwrapped: 91_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_detects_forward_wrap() {
        let mut d = Demuxer::new();
        let raw1 = (1i64 << 33) - 90_000;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1));
        // Wraps forward by 180_000 ticks past the 33-bit boundary.
        let raw2 = raw1 + 180_000 - (1i64 << 33);
        let out = d.unwrap_pts(0x100, Pts90khz::new(raw2));
        assert_eq!(out.as_ticks(), raw1 + 180_000);
        assert!(out.as_ticks() > raw1, "must be monotonic across the wrap");
        assert_eq!(
            d.unwrap_state.get(&0x100),
            Some(&UnwrapState {
                last_raw: raw2,
                last_unwrapped: raw1 + 180_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_genuine_small_backward_step_is_not_a_wrap() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(100_000));
        // A modest backward step (e.g. an out-of-order arrival) is not a
        // wrap — offset stays 0 and the emitted value is non-monotonic,
        // which correctly reflects the reorder.
        let out = d.unwrap_pts(0x100, Pts90khz::new(99_000));
        assert_eq!(out.as_ticks(), 99_000);
        assert_eq!(
            d.unwrap_state.get(&0x100),
            Some(&UnwrapState {
                last_raw: 99_000,
                last_unwrapped: 99_000,
            })
        );
    }

    #[test]
    fn unwrap_pts_is_per_pid() {
        let mut d = Demuxer::new();
        let _ = d.unwrap_pts(0x100, Pts90khz::new(500_000));
        // A different PID's first observation anchors independently —
        // untouched by PID 0x100's already-established state.
        let out = d.unwrap_pts(0x200, Pts90khz::new(1_000));
        assert_eq!(out.as_ticks(), 1_000);
    }

    #[test]
    fn unwrap_secondary_ts_reuses_current_offset_without_mutating_state() {
        let mut d = Demuxer::new();
        let raw1 = (1i64 << 33) - 90_000;
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw1)); // anchors, offset 0
        let raw2 = raw1 + 180_000 - (1i64 << 33);
        let _ = d.unwrap_pts(0x100, Pts90khz::new(raw2)); // wraps — offset becomes 1<<33
        let state_before = d.unwrap_state.get(&0x100).copied();

        // A DTS just below the (already-wrapped) PTS on the same PES
        // reuses the post-wrap offset — no independent wrap check.
        let dts = d.unwrap_secondary_ts(0x100, Pts90khz::new(raw2 - 3_000));
        assert_eq!(dts.as_ticks(), (1i64 << 33) + raw2 - 3_000);
        // The DTS call must not mutate the accumulator.
        assert_eq!(d.unwrap_state.get(&0x100).copied(), state_before);
    }

    #[test]
    fn unwrap_secondary_ts_defaults_to_zero_offset_for_unseen_pid() {
        let d = Demuxer::new();
        let out = d.unwrap_secondary_ts(0x999, Pts90khz::new(42));
        assert_eq!(out.as_ticks(), 42);
    }

    /// Fix round 1 (Finding 1 + 2) — the straddle case: PTS has already
    /// wrapped (small raw) while this AU's DTS is still pre-wrap (large
    /// raw). The pre-fix code (`unwrap_secondary_ts`, bare per-PID
    /// offset) put the DTS a full `1 << 33` epoch above where it
    /// belongs. Numbers mirror the reviewer's worked trace:
    /// `pts_diff_33bit(1<<33-200, 100) == -300`, so the unwrapped DTS is
    /// `(1<<33+100) + (-300) == 1<<33-200` — in the PRE-wrap epoch,
    /// below the unwrapped PTS, NOT `1<<33 + (1<<33-200)`.
    #[test]
    fn unwrap_dts_with_pts_straddling_wrap_stays_in_pre_wrap_epoch() {
        let pts_raw = Pts90khz::new(100);
        let dts_raw = Pts90khz::new((1i64 << 33) - 200);
        let unwrapped_pts = Pts90khz::new((1i64 << 33) + 100);

        let dts = Demuxer::unwrap_dts_with_pts(unwrapped_pts, pts_raw, dts_raw);

        assert_eq!(dts.as_ticks(), (1i64 << 33) - 200);
        assert!(
            dts.as_ticks() < unwrapped_pts.as_ticks(),
            "DTS must stay below PTS across the straddle, got dts={} pts={}",
            dts.as_ticks(),
            unwrapped_pts.as_ticks()
        );
    }

    /// Fix round 1 — same-epoch regression guard: when DTS and PTS are
    /// both on the same side of any wrap (the common case), the fix must
    /// reproduce today's correct behavior exactly.
    /// `pts_diff_33bit(87_000, 90_000) == -3000`, so unwrapped DTS ==
    /// `90_000 + (-3000) == 87_000` (offset 0, matches the raw value).
    #[test]
    fn unwrap_dts_with_pts_same_epoch_matches_raw_delta() {
        let pts_raw = Pts90khz::new(90_000);
        let dts_raw = Pts90khz::new(87_000);
        let unwrapped_pts = Pts90khz::new(90_000); // no wrap yet — offset 0

        let dts = Demuxer::unwrap_dts_with_pts(unwrapped_pts, pts_raw, dts_raw);

        assert_eq!(dts.as_ticks(), 87_000);
    }
}
