//! Top-level `Demuxer` state machine — the coordinator that wires
//! together the sibling-submodule helpers:
//!
//! - `sync_ingress` — byte-aligned 188-byte packet detection + PCR / CC
//!   anomaly checks.
//! - `psi_topology` — PSI section dispatch + PAT/PMT topology tracking.
//! - `pmt_classify` — PMT stream classification + descriptor recognition.
//! - `pes_emit` — PES reassembly dispatch + event construction.
//! - `stats_recorder` — counter bumping + nonconformant event queueing.
//! - `strict` — `StrictMode` policy enum.
//!
//! Public API (`new`, `with_config`, `feed`, `feed_aligned`,
//! `next_event`, `flush`, `stats`, `reset_stats`, `stream_codec_stats`)
//! lives here in the coordinator. Implementation helpers are
//! `pub(super)` and live in the sibling submodules.

use crate::error::DemuxError;
use crate::mpegts::common::{Pts90khz, StreamTypeCode};
use crate::mpegts::demux::event::{
    AdaptationFieldKind, DemuxEvent, NonConformantIssue, StreamId, StreamKind,
};
use crate::mpegts::demux::pes::{Reassembler, ReassemblyOutcome, is_damage_reason};
use crate::mpegts::demux::pid_state::{PidState, TopologyDiff};
use crate::mpegts::demux::psi_assembler::PsiSectionAssembler;
use crate::mpegts::demux::strict::StrictMode;
#[cfg(test)]
use crate::mpegts::demux::timestamp::UnwrapState;
use crate::mpegts::demux::timestamp::{PcrHistory, TimestampUnwrapper};
use crate::mpegts::demux::ts::{TsParseError, parse_ts_packet};
use crate::mpegts::demux::types::{
    DEFAULT_AU_CELL_CAP_PER_PID, DEFAULT_AU_CELL_CAP_TOTAL, DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS,
    DEFAULT_PES_CAP_PER_PID, DEFAULT_PES_CAP_TOTAL, DemuxerConfig, DemuxerStats, ProgramTracker,
};
use crate::mpegts::stats::CodecKind;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

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
    /// Programs found in the current PAT, keyed by `program_number`.
    /// Several programs may share one PMT PID (H.222.0 §2.4.4.8).
    pub(super) programs: HashMap<u16, ProgramTracker>,
    /// Every PMT PID the current PAT declares. O(1) lookup when routing
    /// PMT-bound packets.
    pub(super) pmt_pids: HashSet<u16>,
    /// Identity of the last applied PAT: `(transport_stream_id,
    /// version_number, CRC_32 of the completing section)`. A PAT that
    /// differs in any of the three triggers the diff (programs added/removed).
    pub(super) pat_identity: Option<(u16, u8, u32)>,
    /// Everything remembered per PID (kind, owner, continuity, timing,
    /// unwrap, counters, once-flags) in one map. See `pid_state.rs`.
    pub(super) pid_state: HashMap<u16, PidState>,
    /// Captured (expected, observed) CC pair when `check_continuity`
    /// flagged a real jump on the packet currently being routed. Drained
    /// by `handle_psi` when it consumes the strict-mode drop arm; cleared
    /// at the top of every `check_continuity` call so PSI packets without
    /// a jump don't carry stale state.
    pub(super) last_psi_cc_jump: Option<(u8, u8)>,
    /// Last-seen PCR per declared PCR PID.
    pub(super) pcr: PcrHistory,
    /// Per-program unwrap references and the re-anchor counter; populated
    /// and consulted only when [`DemuxerConfig::unwrap_timestamps`] is `true`.
    /// Cleared on [`Self::reset_sync`] and per program on PAT removal.
    pub(super) timestamps: TimestampUnwrapper,
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
    /// `true` when the NEXT candidate `0x47` the scan finds must pass the
    /// N-of-M stride check before being accepted, even though `is_synced`
    /// may already read `false` with `bytes_since_sync` at zero (e.g. right
    /// after an `Unrecoverable` verdict). Set in the scan branch
    /// (`live[0] != 0x47`); cleared wherever sync is (re)confirmed
    /// (`is_synced = true`), in `reset_sync`, `feed_aligned`, and the two
    /// `SyncBufExhausted` clear paths. `unrecoverable_window` derives it
    /// from whether a candidate byte is still retained in `sync_buf` — an
    /// EMPTY buffer keeps the fresh-demuxer contract (a leading
    /// `0x47` on the next `feed` is initial acquisition, not resync), while
    /// a retained candidate must still be confirmed. See
    /// `retained_candidate_after_unrecoverable_still_needs_n_of_m`.
    pub(super) resync_required: bool,
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
    /// Per-PID Metadata AU cell reassembler. Accumulates fragmented sync-
    /// metadata cells (H.222.0 §2.12.4.2 First/Middle/Last) into complete
    /// AUs. Single-cell (`Complete`) AUs pass through unchanged. Cleared
    /// wholesale on [`Self::reset_sync`] and on PMT version change.
    pub(super) au_reassembler: crate::mpegts::demux::au_reassemble::AuCellReassembler,
    /// Multi-section PAT reassembler. Buffers sections of one
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
            pmt_pids: HashSet::new(),
            pat_identity: None,
            pid_state: HashMap::new(),
            last_psi_cc_jump: None,
            pcr: PcrHistory::default(),
            timestamps: TimestampUnwrapper::default(),
            pes: Reassembler::new(cap_per_pid, cap_total),
            queue: VecDeque::new(),
            bytes_since_sync: 0,
            is_synced: false,
            resync_required: false,
            fatal: None,
            program_maps_seen: 0,
            pmt_versions_seen: 0,
            discontinuities_count: 0,
            nonconformant_count: 0,
            subtitle_streams_seen_count: 0,
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
    /// # Errors
    ///
    /// `Err(DemuxError::Unrecoverable { after_bytes })` closes one
    /// sync-search window: more than `SYNC_SEARCH_WINDOW` (32 × 188 =
    /// 6,016) bytes were scanned without a confirmed packet boundary. The
    /// scanned bytes are discarded and the counter restarts, so the next
    /// `feed` starts a fresh window — no `reset_sync` is needed, and a
    /// source that keeps sending non-TS bytes trips the verdict once per
    /// window rather than on every call. `Err(DemuxError::SyncBufExhausted
    /// { .. })` drops the buffered bytes and this call's input; the next
    /// `feed` starts from an empty buffer. See
    /// [`DemuxerConfig::sync_buf_cap`](crate::mpegts::demux::DemuxerConfig::sync_buf_cap).
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
        // The ceiling bounds LIVE bytes (this call's input plus any
        // unaligned residue) — never the consumed prefix that
        // `compact_sync_buf` reclaims lazily below its 1 MiB floor. When
        // the projected total would trip the cap only because of that
        // dead prefix, reclaim it first so a cap below the floor (a
        // documented, binding-exposed knob) accepts steady aligned feeds.
        if self.sync_consumed > 0 && self.sync_buf.len().saturating_add(bytes.len()) > cap {
            self.sync_buf.drain(..self.sync_consumed);
            self.sync_consumed = 0;
        }
        let projected = self.sync_buf.len().checked_add(bytes.len());
        if projected.is_none_or(|n| n > cap) {
            // Report what the live total would have been (saturating, so
            // the checked-add-overflow case still yields a meaningful figure).
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
            self.bytes_since_sync = 0;
            self.resync_required = false;
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
            self.bytes_since_sync = 0;
            self.resync_required = false;
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
        // Cross-feed resume: if a previous `feed()` call ran out of bytes
        // mid-scan, or closed a sync-search window on a retained candidate
        // byte (`Unrecoverable`), we re-enter here with `is_synced=false`
        // and `resync_required=true`. That field (not a derived count) keeps
        // us in resync mode across the call boundary so the next 0x47 we
        // find still has to pass N-of-M — including a candidate that
        // survived a window verdict with `bytes_since_sync` already reset to
        // zero (see `resync_required`'s doc comment).
        let mut resyncing = !self.is_synced && self.resync_required;
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
                self.resync_required = true;
                resyncing = true;
                let mut i = 1;
                while i < live.len() && live[i] != crate::mpegts::common::TS_SYNC_BYTE {
                    i += 1;
                }
                self.bytes_since_sync += i;
                // Consume the scanned span whatever the verdict: these
                // bytes were searched and hold no packet start.
                self.sync_consumed += i;
                self.compact_sync_buf();
                if self.bytes_since_sync > super::sync_ingress::SYNC_SEARCH_WINDOW {
                    return Err(self.unrecoverable_window());
                }
                continue;
            }
            // Candidate sync byte. Only validate via N-of-M stride check
            // if we got here through the scan path (`resyncing`). Per
            // H.222.0 §2.4.3.2 and ffmpeg `mpegts.c::mpegts_resync`,
            // without this a stray 0x47 inside PES payload causes false
            // sync after a packet loss.
            if resyncing {
                match super::sync_ingress::SYNC_REACQ.verify(live, 0) {
                    Some(true) => {
                        // Fall through to packet parse below.
                    }
                    Some(false) => {
                        // Stray 0x47 — advance past it and keep
                        // searching. Charge 1 byte against the
                        // sync-search window so adversarial input
                        // still hits Unrecoverable.
                        self.bytes_since_sync += 1;
                        self.sync_consumed += 1;
                        self.compact_sync_buf();
                        if self.bytes_since_sync > super::sync_ingress::SYNC_SEARCH_WINDOW {
                            return Err(self.unrecoverable_window());
                        }
                        continue;
                    }
                    None => {
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
            self.resync_required = false;
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
            if let Err(e) = self.handle_process_packet_result(result) {
                // A verdict raised by this packet must not outlive its error
                // return and fail a later, clean packet.
                self.fatal = None;
                return Err(e);
            }
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
    /// Otherwise it returns `StrictRejection` when the configured strict
    /// mode rejects a `NonConformantIssue` raised by this packet, and
    /// `MalformedPes` when a strict mode rejects a malformed PES header. In
    /// lenient mode (the default) `MalformedPes` is converted to a
    /// `NonConformant` event so a single corrupt PES doesn't tear down the
    /// receive loop.
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
    ///
    /// An aligned packet discards a partial packet buffered by an earlier
    /// [`feed`](Self::feed), which could not be continued across it.
    pub fn feed_aligned(&mut self, pkt: &[u8; 188]) -> Result<(), DemuxError> {
        if pkt[0] != crate::mpegts::common::TS_SYNC_BYTE {
            return Err(DemuxError::Unrecoverable { after_bytes: 0 });
        }
        // Caller guarantees alignment — lock sync state so the next
        // `feed` (if any) doesn't re-acquire via N-of-M.
        self.is_synced = true;
        self.resync_required = false;
        self.bytes_since_sync = 0;
        // A partial packet buffered by `feed` would otherwise be spliced
        // onto the next `feed` as if the aligned packet never arrived.
        if self.sync_buf.len() > self.sync_consumed {
            self.sync_buf.clear();
            self.sync_consumed = 0;
        }
        let result = self.process_packet(pkt);
        if let Err(e) = self.handle_process_packet_result(result) {
            self.fatal = None;
            return Err(e);
        }
        if let Some(fatal) = self.fatal.take() {
            return Err(DemuxError::StrictRejection(format!("{fatal:?}")));
        }
        Ok(())
    }

    /// Close one sync-search window with an `Unrecoverable` verdict. The
    /// verdict is per window: the caller has already consumed the
    /// scanned span, and the counter restarts at zero so the next `feed`
    /// begins a fresh search instead of re-scanning the same bytes and
    /// re-reporting a doubled count.
    ///
    /// A scan can close the window sitting ON a candidate `0x47` (the byte
    /// the scan stopped at, ≥ 188 bytes still behind it in `sync_buf`) —
    /// that candidate was never scanned past, so it's still buffered, not
    /// discarded. Derive `resync_required` from whether such a candidate
    /// remains: an empty buffer means the next `feed`'s leading `0x47` (if
    /// any) is fresh initial acquisition; a non-empty one means
    /// it must still pass N-of-M before the demuxer trusts it (see
    /// `retained_candidate_after_unrecoverable_still_needs_n_of_m`).
    fn unrecoverable_window(&mut self) -> DemuxError {
        let after_bytes = core::mem::take(&mut self.bytes_since_sync);
        self.is_synced = false;
        self.resync_required = self.sync_buf.len() > self.sync_consumed;
        DemuxError::Unrecoverable { after_bytes }
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
    /// `flush` cannot return an error; a strict verdict raised while flushing
    /// is reported only as the queued `NonConformant` event.
    ///
    /// The partial PES are released in ascending PID order, so the tail of a
    /// stream is the same on every run.
    ///
    /// # C ABI
    ///
    /// `tst_demuxer_flush` — see `bindings/c/include/tstrans.h`.
    pub fn flush(&mut self) {
        // Same marking as `handle_pes_packet`, but `flush` cannot fail:
        // lenient mode queues `MalformedPes` directly before the damaged
        // sample; a strict mode queues it and drops the sample, keeping its
        // timing. A partial that fails to parse is dropped silently, as it
        // always was at flush.
        let mut drop_next_complete: Option<u16> = None;
        let mut damage_next_complete: Option<(u16, &'static str)> = None;
        for outcome in self.pes.drain_partial_outcomes() {
            match outcome {
                ReassemblyOutcome::Complete(pes) => {
                    if drop_next_complete == Some(pes.pid) {
                        drop_next_complete = None;
                        self.drop_damaged_pes(pes);
                        continue;
                    }
                    let damage = damage_next_complete
                        .take_if(|(pid, _)| *pid == pes.pid)
                        .map(|(_, reason)| reason);
                    self.handle_complete_pes(pes, damage);
                }
                ReassemblyOutcome::Malformed { pid, reason }
                    if is_damage_reason(reason) && self.options.strict == StrictMode::Off =>
                {
                    damage_next_complete = Some((pid, reason));
                }
                ReassemblyOutcome::Malformed { pid, reason } if is_damage_reason(reason) => {
                    let stream = self.lookup_stream(pid).unwrap_or_else(|| {
                        StreamId::anonymous(pid, self.program_number_for_pid(pid))
                    });
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::MalformedPes { pid, reason },
                    );
                    drop_next_complete = Some(pid);
                }
                _ => {}
            }
        }
        self.fatal = None;
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
        // upstream link-layer flagged the packet as known-corrupt.
        // Feeding the payload to PES/PSI reassembly would corrupt downstream
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
        // The adaptation field is never scrambled (H.222.0 §2.4.3.2), so its
        // PCR stays valid; check it before the scrambled payload is dropped.
        self.check_pcr(&pkt);
        // ISO/IEC 13818-1 §2.4.3.2: a non-zero `transport_scrambling_control`
        // marks the payload as scrambled. The library does not descramble;
        // feeding scrambled bytes to PSI/PES reassembly would corrupt parse
        // state and surface as random malformation. Drop the packet (no
        // payload routed) and surface UnsupportedScrambling so consumers can
        // distinguish "unsupported scrambling" from "random corruption".
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
        // Surface adaptation-field control/length violations.
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
        let (cc_jumped, is_duplicate) = self.check_continuity(&pkt);
        // H.222.0 §2.4.3.3: a spec-legal duplicate (same CC, identical
        // bytes) carries no new payload data. Suppress all
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
        } else if self.pmt_pids.contains(&pkt.pid) {
            self.handle_psi(
                pkt.pid,
                pkt.payload,
                pkt.payload_unit_start,
                false,
                cc_jumped,
            )?;
        } else if pkt.has_payload && self.stream_kind(pkt.pid).is_some() {
            // H.222.0 §2.4.3.3: a continuity_counter gap means packets of
            // this PID were lost, so the PES in flight is missing bytes.
            if cc_jumped {
                self.pes.mark_damaged(pkt.pid);
            }
            self.handle_pes_packet(&pkt)?;
        }
        Ok(())
    }

    pub(super) fn lookup_stream(&self, pid: u16) -> Option<StreamId> {
        self.stream_kind(pid).map(|kind| {
            let program_number = self.program_number_for_pid(pid);
            StreamId {
                pid,
                kind,
                program_number,
            }
        })
    }

    /// The PES dispatch kind a PMT declared for `pid`, if any.
    pub(super) fn stream_kind(&self, pid: u16) -> Option<StreamKind> {
        self.pid_state.get(&pid).and_then(|s| s.stream_kind)
    }

    /// The program that owns `pid`, if a PMT has declared it.
    pub(super) fn program_of(&self, pid: u16) -> Option<u16> {
        self.pid_state.get(&pid).and_then(|s| s.program)
    }

    /// The state entry for `pid`, created empty on first use.
    pub(super) fn pid_state_mut(&mut self, pid: u16) -> &mut PidState {
        self.pid_state.entry(pid).or_default()
    }

    /// Look up the program_number for a PID.
    /// Returns 0 if the PID is not owned by any known program (e.g. PSI PIDs).
    pub(super) fn program_number_for_pid(&self, pid: u16) -> u16 {
        self.program_of(pid).unwrap_or(0)
    }

    /// Test probe: the unwrap accumulator of `pid`.
    #[cfg(test)]
    pub(super) fn unwrap_of(&self, pid: u16) -> Option<&UnwrapState> {
        self.pid_state.get(&pid).and_then(|s| s.unwrap.as_ref())
    }

    /// Apply a topology change to the per-PID state. The ONE place where
    /// PIDs enter and leave `pid_state` on a PAT/PMT change.
    pub(super) fn apply_topology_diff(&mut self, diff: TopologyDiff) {
        for pid in diff.retired {
            if let Some(state) = self.pid_state.get_mut(&pid) {
                state.retire();
                if state.is_vacant() {
                    self.pid_state.remove(&pid);
                }
            }
            self.pes.remove_pid(pid);
            self.au_reassembler.reset_pid(pid);
        }
        for pmt_pid in diff.retired_pmt_pids {
            if let Some(state) = self.pid_state.get_mut(&pmt_pid) {
                state.clear_continuity();
                if state.is_vacant() {
                    self.pid_state.remove(&pmt_pid);
                }
            }
        }
        for pid in diff.kind_changed {
            self.pes.remove_pid(pid);
            self.au_reassembler.reset_pid(pid);
            if let Some(state) = self.pid_state.get_mut(&pid) {
                state.clear_timing();
                state.codec = None;
            }
        }
        for (pid, kind, program, stream_type) in diff.added {
            let state = self.pid_state_mut(pid);
            state.stream_kind = Some(kind);
            state.program = Some(program);
            state.stream_type = Some(stream_type);
            // Counters outlive retirement, so a re-declared or retyped PID
            // may carry a stale byte or a counter family of another kind.
            if let Some(stats) = state.stats.as_mut() {
                stats.stream_type = StreamTypeCode::from_byte(stream_type);
            }
            let family = match kind {
                StreamKind::Video(_) => Some(CodecKind::Video),
                StreamKind::Audio(_) => Some(CodecKind::Audio),
                StreamKind::KlvSync { .. } | StreamKind::KlvAsync => Some(CodecKind::Klv),
                StreamKind::Subtitle(_) | StreamKind::Unknown(_) => None,
            };
            if state.codec.is_some_and(|c| Some(c.kind) != family) {
                state.codec = None;
            }
        }
        if !diff.pcr_pid_changes.is_empty() {
            self.retire_undeclared_pcr_history();
        }
    }

    /// Unwrap a raw 33-bit PTS for `pid` (see [`TimestampUnwrapper::unwrap`]).
    /// Only called when `options.unwrap_timestamps` is `true` and a genuine
    /// (non-synthesized) PTS was observed.
    pub(super) fn unwrap_pts(&mut self, pid: u16, raw: Pts90khz) -> Pts90khz {
        let program = self.program_of(pid);
        let own = &mut self.pid_state.entry(pid).or_default().unwrap;
        self.timestamps.unwrap(own, program, raw)
    }

    /// The one lenient/strict policy point for a malformed PES on `pid`:
    /// lenient (`StrictMode::Off`) queues `NonConformantIssue::MalformedPes`
    /// and continues; a strict mode that rejects the issue returns
    /// `DemuxError::MalformedPes`. Called from `handle_pes_packet` for each
    /// `ReassemblyOutcome::Malformed`, and from the
    /// defensive arm of `handle_process_packet_result`.
    pub(super) fn surface_malformed_pes(
        &mut self,
        pid: u16,
        reason: &'static str,
    ) -> Result<(), DemuxError> {
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

    /// Convert a `process_packet` result into lenient/strict policy.
    ///
    /// The `DemuxError::MalformedPes` arm is defensive. The PES
    /// reassembler reports malformed PESes as
    /// `ReassemblyOutcome::Malformed`, and `handle_pes_packet` applies the
    /// policy per outcome via [`Self::surface_malformed_pes`], so the only
    /// `MalformedPes` that reaches this arm today is the strict-mode error
    /// that call already returned, and re-evaluating it returns the same
    /// error. The arm is kept so any future non-reassembler producer of
    /// that error still gets the lenient `NonConformant` / strict error
    /// policy instead of passing through unexamined.
    ///
    /// All other `DemuxError` variants (`Unrecoverable`,
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
                self.surface_malformed_pes(pid, reason)
            }
            Err(other) => Err(other),
        }
    }

    /// Return a reference to the programs map for white-box unit tests.
    ///
    /// Keyed by `program_number`. Crate-internal test accessor for PAT/PMT diffing
    /// logic. Not part of the stable API.
    #[cfg(test)]
    pub(crate) fn programs_for_test(&self) -> &HashMap<u16, ProgramTracker> {
        &self.programs
    }

    /// Per-PID codec-specific counters. See
    /// [`crate::mpegts::stats::StreamCodecStats`] for the semantics of
    /// the return value (`None` vs `Some(Unknown)` vs typed variant).
    pub fn stream_codec_stats(&self, pid: u16) -> Option<crate::mpegts::stats::StreamCodecStats> {
        let state = self.pid_state.get(&pid)?;
        if let Some(c) = &state.codec {
            return Some(c.to_public());
        }
        if state.stats.is_some() {
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
            unwrap_reanchors: self.timestamps.reanchors(),
            per_stream: self
                .pid_state
                .iter()
                .filter_map(|(&pid, s)| s.stats.clone().map(|st| (pid, st)))
                .collect(),
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
        self.pmt_pids.clear();
        self.pat_identity = None;
        // Per-PID state resets entry by entry: counters and the
        // subtitle-seen flag survive a reconnect (`reset_stats` owns them).
        for state in self.pid_state.values_mut() {
            state.reset_sync();
        }
        self.pid_state.retain(|_, s| !s.is_vacant());
        self.last_psi_cc_jump = None;
        self.pcr.clear();
        // A reconnect restarts the opt-in PTS/DTS unwrap timeline (see
        // `DemuxerConfig::unwrap_timestamps`) — stale offsets from the
        // dead connection must not splice into the new one's epoch.
        self.timestamps.clear();
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
        self.resync_required = false;
        self.fatal = None;
        // Per-PMT-version PID dedupe sets clear so a fresh PMT post-
        // reconnect re-fires `SubtitleMissingDescriptor` /
        // `Av1RegistrationMalformed` / `SubtitleDescriptorAmbiguous` if
        // those still apply on the new connection.
        // Drop all in-flight AU cell reassembly buffers. Operational reset
        // — no NonConformant emitted; pre-reconnect cells are simply gone.
        self.au_reassembler.reset_all();
        // Drop any partially-assembled multi-section PAT.
        self.pat_reassembler.clear();
    }

    /// Reset all stats counters to zero and clear per-stream entries.
    ///
    /// Counters of PIDs that left the topology are kept until this call, so
    /// their entries clear here too. Per-PID entries are dropped (both the
    /// unified `stats_per_stream` and the codec-counter side table); the
    /// next event for a
    /// previously-seen PID re-materializes both entries with the kind
    /// discriminator derived from the current `stream_type`. This keeps
    /// the 3-state [`Self::stream_codec_stats`] accessor symmetric across
    /// reset — an unseen PID returns `None` both before AND after.
    ///
    /// Counters only: topology caches (the PAT and PMT versions, the
    /// declared streams) and in-flight reassembly are untouched, so a PMT
    /// repeat after this call is still a repeat — no `ProgramMap`, no
    /// `pmt_versions_seen` increment. Only a real version change counts.
    pub fn reset_stats(&mut self) {
        self.program_maps_seen = 0;
        self.pmt_versions_seen = 0;
        self.discontinuities_count = 0;
        self.nonconformant_count = 0;
        self.timestamps.reset_reanchors();
        self.subtitle_streams_seen_count = 0;
        for state in self.pid_state.values_mut() {
            state.reset_stats();
        }
        self.pid_state.retain(|_, s| !s.is_vacant());
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
    use crate::codec::VideoCodec;
    use crate::mpegts::common::Pts90khz;
    use crate::mpegts::demux::StrictMode;
    use crate::mpegts::demux::event::{DiscontinuityKind, SamplePayload, SubtitleCodec};
    use crate::mpegts::demux::pes::DAMAGE_SHORT_PES;
    use crate::mpegts::demux::test_support::{
        PktSpec, demuxer_with_streams, drain_all_events, feed_packets, pat_packet_with_programs,
        pes_bytes, pes_to_packets, pmt_packet_for_test, ts_packet,
    };
    use crate::mpegts::demux::types::{
        DemuxerConfig, default_pes_cap_per_pid, default_pes_cap_total,
    };
    use crate::test_prelude::*;

    fn full_demuxer(streams: &[(u8, u16, &[u8])], pcr_pid: u16) -> Demuxer {
        let mut b = DemuxerConfig::builder();
        b.strict(StrictMode::Full);
        demuxer_with_streams(b.build(), pcr_pid, streams)
    }

    fn null_packet() -> [u8; 188] {
        ts_packet(&PktSpec {
            pid: 0x1FFF,
            payload: &[0xFF; 184],
            ..Default::default()
        })
    }

    /// An unbounded video PES whose flags byte lacks the `10` marker.
    fn bad_marker_pes_packet(cc: u8) -> [u8; 188] {
        let mut pes = pes_bytes(0xE0, None, None, false, &[0xAA; 20], false);
        pes[6] = 0x40;
        pes_to_packets(0x100, cc, &pes)[0]
    }

    /// A strict verdict belongs to the packet that raised it; an error return
    /// must not leave it pending for a later, clean packet.
    #[test]
    fn strict_verdict_does_not_survive_an_error_return() {
        let mut d = full_demuxer(&[(0x1B, 0x100, &[])], 0x100);
        let a = bad_marker_pes_packet(0);
        // Completes the first PES (header verdict) and starts a bounded PES
        // with an invalid start code that ends inside this packet.
        let mut bad_start = pes_bytes(0xE0, Some(90_000), None, false, &[0xBB; 20], true);
        bad_start[2] = 0x02;
        let b = pes_to_packets(0x100, 1, &bad_start)[0];
        let mut both = a.to_vec();
        both.extend_from_slice(&b);
        let err = d.feed(&both).unwrap_err();
        assert!(matches!(err, DemuxError::MalformedPes { .. }), "{err:?}");
        d.feed(&null_packet())
            .expect("a clean packet must not fail");
    }

    #[test]
    fn flush_does_not_leave_a_verdict_for_the_next_feed() {
        let mut d = full_demuxer(&[(0x1B, 0x100, &[])], 0x100);
        d.feed(&bad_marker_pes_packet(0)).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(
            ev.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PesHeaderMalformed { .. },
                    ..
                }
            )),
            "{ev:?}"
        );
        d.feed(&null_packet())
            .expect("a clean packet must not fail");
    }

    #[test]
    fn flush_damage_marker_does_not_fail_the_next_feed_in_full_mode() {
        let mut d = full_demuxer(&[(0x0F, 0x101, &[])], 0x101);
        let pes = pes_bytes(0xC0, Some(90_000), None, true, &[0xAA; 400], true);
        let pkts = pes_to_packets(0x101, 0, &pes);
        feed_packets(&mut d, &pkts[..2]).unwrap();
        d.flush();
        d.feed(&null_packet())
            .expect("a clean packet must not fail");
        let ev = drain_all_events(&mut d);
        assert!(
            ev.iter().any(|e| matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MalformedPes { pid: 0x101, reason },
                    ..
                } if *reason == DAMAGE_SHORT_PES
            )),
            "{ev:?}"
        );
    }

    #[test]
    fn feed_aligned_discards_a_partial_packet_left_by_feed() {
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x101,
            &[(0x0F, 0x101, &[])],
        );
        let pkt = |cc| {
            ts_packet(&PktSpec {
                pid: 0x101,
                cc,
                payload: &[0xAA; 184],
                ..Default::default()
            })
        };
        let p0 = pkt(0);
        d.feed_aligned(&p0).unwrap();
        d.feed(&p0[..100]).unwrap();
        d.feed_aligned(&pkt(1)).unwrap();
        let mut rest = pkt(2).to_vec();
        rest.extend_from_slice(&pkt(3));
        d.feed(&rest).unwrap();
        let ev = drain_all_events(&mut d);
        assert!(
            !ev.iter().any(
                |e| matches!(e, DemuxEvent::Discontinuity { stream, .. } if stream.pid == 0x101)
            ),
            "{ev:?}"
        );
    }

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

    /// The ceiling must be measured against
    /// LIVE bytes. Consumed-but-uncompacted bytes below the 1 MiB
    /// compaction floor used to count toward it, so any cap under 1 MiB
    /// rejected ordinary aligned TS every ~cap/188 packets and dropped
    /// that feed's bytes.
    #[test]
    fn sync_buf_cap_below_compaction_floor_accepts_steady_packet_feeds() {
        let cfg = DemuxerConfig::builder().sync_buf_cap(64 * 1024).build();
        let mut d = Demuxer::with_config(cfg);
        let pkt = null_ts_packet();
        for i in 0..1000 {
            d.feed(&pkt).unwrap_or_else(|e| panic!("feed #{i}: {e}"));
        }
        assert!(
            d.sync_buf.len() - d.sync_consumed < 188,
            "no live residue expected after aligned feeds"
        );
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

    /// `Unrecoverable` is a per-window verdict,
    /// not a latch. The old path returned BEFORE consuming the scanned
    /// span, so the next feed re-scanned it, reported a doubled count and
    /// kept doing so forever (only `reset_sync` escaped).
    #[test]
    fn unrecoverable_is_per_window_and_the_next_feed_recovers() {
        let mut d = Demuxer::new();
        let garbage = vec![0xAAu8; 7_000];
        let err = d.feed(&garbage).unwrap_err();
        assert!(
            matches!(err, DemuxError::Unrecoverable { after_bytes: 7_000 }),
            "{err:?}"
        );

        // Valid aligned TS in the very next call must be accepted.
        let pkt = null_ts_packet();
        let mut ts = Vec::new();
        for _ in 0..10 {
            ts.extend_from_slice(&pkt);
        }
        d.feed(&ts)
            .unwrap_or_else(|e| panic!("second feed after the verdict: {e}"));
        assert_eq!(d.bytes_since_sync, 0);
        assert!(d.is_synced);

        // A later garbage window is a NEW verdict with its own count.
        let err = d.feed(&garbage).unwrap_err();
        assert!(
            matches!(err, DemuxError::Unrecoverable { after_bytes: 7_000 }),
            "{err:?}"
        );
    }

    /// Shared prefix for the two `resync_required` regression tests below:
    /// `SYNC_SEARCH_WINDOW + 1` garbage bytes
    /// so the scan closes the window ON a candidate `0x47` with a full
    /// 188-byte packet's worth of bytes still behind it (`Unrecoverable`),
    /// immediately followed by that stray `0x47` and 187 more bytes that
    /// decode as a `transport_error_indicator`-set packet on PID 0x0100.
    /// That payload is a trap: if the N-of-M reject path is ever skipped
    /// and the candidate is handed to `process_packet` as "confirmed"
    /// sync, it queues a `TransportErrorPacket` `NonConformant` event —
    /// an observable that a correctly-discarded candidate never produces.
    fn window_overflow_then_retained_candidate() -> Vec<u8> {
        let window = crate::mpegts::demux::sync_ingress::SYNC_SEARCH_WINDOW;
        let mut data = vec![0xAAu8; window + 1];
        let mut candidate = [0xFFu8; 188];
        candidate[0] = 0x47;
        candidate[1] = 0x81; // transport_error_indicator=1, pid_high=0b00001
        candidate[2] = 0x00; // pid_low=0 -> pid 0x0100
        candidate[3] = 0x10; // adaptation_field_control=01 (payload only), cc=0
        data.extend_from_slice(&candidate);
        data
    }

    /// A candidate `0x47` retained in `sync_buf` when a sync-search window
    /// closes with `Unrecoverable` must still pass N-of-M on the next `feed` —
    /// including an EMPTY one. Before the `resync_required` fix,
    /// `unrecoverable_window` zeroed `bytes_since_sync` and the resync
    /// predicate derived straight from that counter, so the very next `feed`
    /// computed `resyncing = false` and parsed the retained candidate as
    /// "confirmed" sync with no re-validation at all.
    #[test]
    fn retained_candidate_after_unrecoverable_still_needs_n_of_m() {
        let mut d = Demuxer::new();
        let data = window_overflow_then_retained_candidate();
        let err = d.feed(&data).unwrap_err();
        assert!(matches!(err, DemuxError::Unrecoverable { .. }), "{err:?}");
        let sync_consumed_after_verdict = d.sync_consumed;
        assert!(
            d.sync_buf.len() > sync_consumed_after_verdict,
            "the retained candidate must still be sitting in sync_buf"
        );

        // An empty feed carries no new bytes, but the retained candidate is
        // only 188 bytes deep — N-of-M cannot yet confirm or reject it
        // (it needs up to 7 strides of evidence), so it must stay buffered
        // untouched rather than being parsed as confirmed sync.
        d.feed(&[])
            .unwrap_or_else(|e| panic!("empty feed after the verdict: {e}"));
        // The intent, not the mechanism: the retained candidate is still
        // live and unconsumed. Asserting `sync_consumed` unchanged would
        // instead couple to the 1 MiB compaction floor — compaction can
        // shift both `sync_buf` and `sync_consumed` together without
        // touching the candidate at all, which would trip that assertion
        // on an unrelated implementation detail.
        assert_eq!(
            d.sync_buf.len() - d.sync_consumed,
            188,
            "the retained candidate must not be consumed without N-of-M confirmation"
        );
        assert!(
            !d.is_synced,
            "an unconfirmed retained candidate must not flip is_synced"
        );
    }

    /// Companion to the test above: once enough real, NON-stride-aligned
    /// sync follows, N-of-M must REJECT the retained candidate (consuming
    /// it as garbage) and lock onto the real packets instead of trusting
    /// the stray byte.
    #[test]
    fn retained_candidate_is_rejected_when_real_sync_follows() {
        let mut d = Demuxer::new();
        let data = window_overflow_then_retained_candidate();
        let err = d.feed(&data).unwrap_err();
        assert!(matches!(err, DemuxError::Unrecoverable { .. }), "{err:?}");

        // One padding byte shifts every subsequent packet boundary off the
        // 188-byte strides N-of-M probes from the retained candidate, so
        // none of the real packets' sync bytes land where the candidate's
        // stride check looks — it must be rejected, not accepted.
        let mut next = vec![0xAAu8];
        let pkt = null_ts_packet();
        for _ in 0..10 {
            next.extend_from_slice(&pkt);
        }
        d.feed(&next)
            .unwrap_or_else(|e| panic!("resync after the retained candidate: {e}"));

        assert!(
            d.is_synced,
            "the demuxer must re-lock onto the real, aligned packets"
        );
        // The stray candidate must have been discarded as garbage, never
        // handed to `process_packet` — if it had been (the pre-fix bug),
        // its transport_error_indicator payload would have queued a
        // `TransportErrorPacket` NonConformant event. The 10 null packets
        // on their own emit nothing, so any event here proves the
        // candidate leaked through unconfirmed.
        let mut events = Vec::new();
        while let Some(e) = d.next_event() {
            events.push(e);
        }
        assert!(
            events.is_empty(),
            "the retained candidate must be discarded as garbage, not parsed: {events:?}"
        );
    }

    /// The cap path also closes the search window: a partial scan that
    /// is then cleared by `SyncBufExhausted` must not carry its count
    /// into the next feed.
    #[test]
    fn sync_buf_exhausted_resets_the_search_counter() {
        let mut d = Demuxer::new();
        d.feed(&[0xAAu8; 3_000]).unwrap(); // 3_000 < SYNC_SEARCH_WINDOW: Ok, counter 3_000
        assert_eq!(d.bytes_since_sync, 3_000);
        let oversize = vec![0xFFu8; crate::mpegts::demux::sync_ingress::MAX_SYNC_BUF_BYTES + 1];
        assert!(matches!(
            d.feed(&oversize).unwrap_err(),
            DemuxError::SyncBufExhausted { .. }
        ));
        assert_eq!(d.bytes_since_sync, 0);
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
        // crates/tst-core/tests/codec/codec_stats.rs.
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
    fn codec_counters_increment_existing_entry() {
        let mut demux = Demuxer::new();
        demux
            .pid_state_mut(0x100)
            .codec_mut(crate::mpegts::stats::CodecKind::Video)
            .add_video(2, 1);
        match demux.stream_codec_stats(0x100) {
            Some(crate::mpegts::stats::StreamCodecStats::Video {
                nals_or_obus: 2,
                random_access_aus: 1,
                ..
            }) => {}
            other => panic!("expected Video {{2,1}}, got {:?}", other),
        }
        demux
            .pid_state_mut(0x100)
            .codec_mut(crate::mpegts::stats::CodecKind::Video)
            .add_video(3, 0);
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
        use crate::codec::AudioCodec;
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

    /// One ADTS frame whose header declares its own 11-byte length.
    const ADTS_FRAME: [u8; 11] = [
        0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x7F, 0xFC, 0xDE, 0xAD, 0xBE, 0xEF,
    ];

    fn feed_pmt(d: &mut Demuxer, version: u8, streams: &[(u8, u16)]) {
        d.feed(&pmt_packet_for_test(0x1000, 1, 0x100, streams, version))
            .unwrap();
    }

    fn feed_one_pes(d: &mut Demuxer, pid: u16, cc: u8, stream_id: u8, es: &[u8]) {
        let pes = pes_bytes(stream_id, Some(90_000), None, true, es, true);
        feed_packets(d, &pes_to_packets(pid, cc, &pes)).unwrap();
        d.flush();
    }

    fn demuxer_with_pat() -> Demuxer {
        let mut d = Demuxer::new();
        d.feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        d
    }

    #[test]
    fn stats_stream_type_is_the_pmt_byte_not_the_canonical_codec_byte() {
        let mut d = demuxer_with_pat();
        feed_pmt(&mut d, 0, &[(0x04, 0x101)]);
        feed_one_pes(&mut d, 0x101, 0, 0xC0, &ADTS_FRAME);
        let st = d.stats();
        assert_eq!(
            st.per_stream[&0x101].stream_type,
            StreamTypeCode::from_byte(0x04)
        );
    }

    #[test]
    fn kind_change_rederives_codec_counters_and_stats_stream_type() {
        use crate::mpegts::stats::StreamCodecStats;
        let mut d = demuxer_with_pat();
        feed_pmt(&mut d, 0, &[(0x0F, 0x101)]);
        feed_one_pes(&mut d, 0x101, 0, 0xC0, &ADTS_FRAME);
        assert!(matches!(
            d.stream_codec_stats(0x101),
            Some(StreamCodecStats::Audio { .. })
        ));

        feed_pmt(&mut d, 1, &[(0x1B, 0x101)]);
        let idr = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x00];
        let pes = pes_bytes(0xE0, Some(90_000), None, true, &idr, true);
        let pkt = ts_packet(&PktSpec {
            pid: 0x101,
            cc: 1,
            pusi: true,
            rai: true,
            payload: &pes,
            ..Default::default()
        });
        d.feed_aligned(&pkt).unwrap();
        d.flush();
        assert!(matches!(
            d.stream_codec_stats(0x101),
            Some(StreamCodecStats::Video { .. })
        ));
        assert_eq!(
            d.stats().per_stream[&0x101].stream_type,
            StreamTypeCode::from_byte(0x1B)
        );
    }

    #[test]
    fn retired_then_redeclared_pid_rederives_codec_and_stream_type() {
        use crate::mpegts::stats::StreamCodecStats;
        let mut d = demuxer_with_pat();
        feed_pmt(&mut d, 0, &[(0x0F, 0x101), (0x0F, 0x102)]);
        feed_one_pes(&mut d, 0x101, 0, 0xC0, &ADTS_FRAME);
        feed_pmt(&mut d, 1, &[(0x0F, 0x102)]);
        feed_pmt(&mut d, 2, &[(0x1B, 0x101), (0x0F, 0x102)]);
        let idr = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x00];
        let pes = pes_bytes(0xE0, Some(90_000), None, true, &idr, true);
        let pkt = ts_packet(&PktSpec {
            pid: 0x101,
            cc: 1,
            pusi: true,
            rai: true,
            payload: &pes,
            ..Default::default()
        });
        d.feed_aligned(&pkt).unwrap();
        d.flush();
        assert!(matches!(
            d.stream_codec_stats(0x101),
            Some(StreamCodecStats::Video { .. })
        ));
        let st = d.stats();
        assert_eq!(
            st.per_stream[&0x101].stream_type,
            StreamTypeCode::from_byte(0x1B)
        );
        assert_eq!(st.per_stream[&0x101].items, 2);
    }

    #[test]
    fn same_kind_retype_updates_stats_stream_type_and_keeps_totals() {
        let mut d = demuxer_with_pat();
        feed_pmt(&mut d, 0, &[(0x03, 0x101)]);
        feed_one_pes(&mut d, 0x101, 0, 0xC0, &ADTS_FRAME);
        feed_pmt(&mut d, 1, &[(0x04, 0x101)]);
        feed_one_pes(&mut d, 0x101, 1, 0xC0, &ADTS_FRAME);
        assert_eq!(
            d.stats().per_stream[&0x101].stream_type,
            StreamTypeCode::from_byte(0x04)
        );
        assert_eq!(d.stats().per_stream[&0x101].items, 2);
    }

    #[test]
    fn retired_pid_keeps_its_counters_until_reset_stats() {
        let mut d = demuxer_with_pat();
        feed_pmt(&mut d, 0, &[(0x0F, 0x101), (0x0F, 0x102)]);
        for cc in 0..3 {
            feed_one_pes(&mut d, 0x101, cc, 0xC0, &ADTS_FRAME);
        }
        feed_pmt(&mut d, 1, &[(0x0F, 0x102)]);
        assert_eq!(d.stats().per_stream[&0x101].items, 3);
        d.reset_stats();
        assert!(!d.stats().per_stream.contains_key(&0x101));
    }

    #[test]
    fn treat_as_overrides_pmt_classification_to_typed_audio() {
        use crate::codec::AudioCodec;
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
                && stream.pid == 0x200
                && codec == DemuxSubtitleCodec::WebVttInTs
            {
                got_subtitle = true;
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
                && pid == 0x200
            {
                got_missing_descriptor = true;
                count += 1;
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
                && pid == 0x200
            {
                got_ambiguous = true;
                count += 1;
                // 0x59 (subtitling) priority before 0xF0 (synthetic VTTC).
                assert_eq!(tags, vec![0x59, 0xF0]);
            }
        }
        assert!(
            got_ambiguous,
            "expected SubtitleDescriptorAmbiguous for PID 0x200"
        );
        assert_eq!(count, 1, "deduped: one event per PID per PMT version");
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
    /// marks a packet as link-layer-corrupt. We drop these: silently
    /// feeding the payload to PES/PSI reassembly produces garbage parse
    /// output downstream.
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
        au.extend(core::iter::repeat_n(0xAB, 64));
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

    /// The muxer wraps DVB-sub PES payloads in the
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

    // -------------------------------------------------------------------------
    // Zero PES_packet_length on a non-video stream
    // -------------------------------------------------------------------------

    #[test]
    fn zero_length_non_video_pes_emits_nonconformant_no_sample() {
        // A PES with zero PES_packet_length on an audio PID must
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
        buf[8] = 0x00; // PES_packet_length hi = 0 (unbounded — illegal on non-video)
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
        // stream_type 0x02 (ITU-T H.262 / MPEG-2 video) is a VIDEO
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

    // `reset_stats` resets counters only: the cached PMT version survives,
    // so a PMT repeat after it is still a repeat.
    #[test]
    fn reset_stats_then_pmt_repeat_keeps_in_flight_au_and_emits_no_program_map() {
        use crate::mpegts::au_cell::{AuCellHeader, CellFragmentIndication};
        use crate::mpegts::demux::event::MetadataKind;
        use crate::mpegts::demux::test_support::{au_cell_pes_packets, pmt_packet_for_test_cc};
        let cell = |seq, cfi| AuCellHeader {
            metadata_service_id: 0,
            sequence_number: seq,
            cell_fragment_indication: cfi,
            decoder_config_flag: false,
            random_access_indicator: true,
        };
        let mut d = Demuxer::new();
        d.feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        d.feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1FFF,
            &[(0x15, 0x0101)],
            0,
        ))
        .unwrap();
        drain_all_events(&mut d);

        feed_packets(
            &mut d,
            &au_cell_pes_packets(0x0101, 0, cell(0, CellFragmentIndication::First), b"ab"),
        )
        .unwrap();
        d.reset_stats();
        d.feed(&pmt_packet_for_test_cc(
            0x1000,
            1,
            0x1FFF,
            &[(0x15, 0x0101)],
            0,
            1,
        ))
        .unwrap();
        feed_packets(
            &mut d,
            &au_cell_pes_packets(0x0101, 1, cell(1, CellFragmentIndication::Last), b"cd"),
        )
        .unwrap();

        let events = drain_all_events(&mut d);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DemuxEvent::ProgramMap(_))),
            "a PMT repeat after reset_stats is no new ProgramMap: {events:?}"
        );
        let metas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DemuxEvent::Metadata {
                    kind:
                        MetadataKind::KlvSyncAuCell {
                            was_reassembled,
                            cell_count,
                            ..
                        },
                    payload,
                    ..
                } => Some((*was_reassembled, *cell_count, payload.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            metas,
            vec![(true, 2, b"abcd".to_vec())],
            "events: {events:?}"
        );
        assert_eq!(d.stats().pmt_versions_seen, 0);
    }

    // A First or Complete cell that reveals a lost cell fails the PID's
    // in-flight AU, and the demuxer replays it: the Complete still emits.
    #[test]
    fn a_gap_on_a_complete_cell_reports_the_lost_au_and_still_emits_the_cell() {
        use crate::mpegts::au_cell::{AuCellHeader, CellFragmentIndication};
        use crate::mpegts::demux::event::{MetadataKind, MultiCellAuReason};
        use crate::mpegts::demux::test_support::{au_cell_pes_packets, issues};
        let cell = |service, seq, cfi| AuCellHeader {
            metadata_service_id: service,
            sequence_number: seq,
            cell_fragment_indication: cfi,
            decoder_config_flag: false,
            random_access_indicator: true,
        };
        let mut d = Demuxer::new();
        d.feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
            .unwrap();
        d.feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1FFF,
            &[(0x15, 0x0101)],
            0,
        ))
        .unwrap();
        drain_all_events(&mut d);

        feed_packets(
            &mut d,
            &au_cell_pes_packets(0x0101, 0, cell(0, 0, CellFragmentIndication::First), b"ab"),
        )
        .unwrap();
        // Cell 1 is lost; cell 2 is another service's Complete.
        feed_packets(
            &mut d,
            &au_cell_pes_packets(
                0x0101,
                1,
                cell(1, 2, CellFragmentIndication::Complete),
                b"c",
            ),
        )
        .unwrap();

        let events = drain_all_events(&mut d);
        assert!(
            issues(&events).iter().any(|i| matches!(
                i,
                NonConformantIssue::MultiCellAu {
                    reason: MultiCellAuReason::SequenceGap,
                    dropped_bytes: 2,
                    ..
                }
            )),
            "events: {events:?}"
        );
        let metas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DemuxEvent::Metadata {
                    kind:
                        MetadataKind::KlvSyncAuCell {
                            metadata_service_id,
                            cell_count,
                            ..
                        },
                    payload,
                    ..
                } => Some((*metadata_service_id, *cell_count, payload.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(metas, vec![(1, 1, b"c".to_vec())], "events: {events:?}");
    }
}
