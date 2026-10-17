//! The RTP depacketization state machine (rules 1–8 of the `h264`
//! depacketizer's module doc): SSRC reset, sequence-gap poisoning,
//! timestamp boundaries, the marker fast path, access-unit completion,
//! the oversize cap and parameter-set injection. Codec-neutral; a
//! [`Format`] supplies the payload-header rules.

use std::collections::VecDeque;

use tst_core::mpegts::common::Pts90khz;

use crate::clock::Unwrap32;
use crate::depay::{AccessUnit, DepayConfig, DepayStats, ParameterSetInjection};
use crate::packet::RtpHeader;

/// What one RTP payload is, from its payload header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Payload {
    /// One whole NAL unit.
    Single,
    /// Several NAL units, each behind a 16-bit size (STAP-A, AP).
    Aggregation,
    /// A fragment of one NAL unit (FU-A, FU).
    Fragment,
    /// A packet type this depacketizer cannot reassemble: discard and
    /// poison the access unit.
    Poison,
    /// A reserved type: discard without poisoning.
    Reserved,
}

/// The payload-header rules of one RTP payload format.
pub(crate) trait Format {
    /// Parameter-set cache slots, in injection order.
    const PS_SLOTS: usize;
    /// Bytes before an aggregation packet's first size field.
    const AGG_PREFIX_LEN: usize;
    /// Bytes before a fragmentation unit's data (payload header + FU header).
    const FU_PREFIX_LEN: usize;
    /// Classify a non-empty payload.
    fn classify(payload: &[u8]) -> Payload;
    /// The NAL type of a non-empty NAL unit (header first).
    fn nal_type(nal: &[u8]) -> u8;
    /// `true` for an IDR picture's NAL type.
    fn is_idr(nal_type: u8) -> bool;
    /// The cache slot of a parameter-set NAL type.
    fn ps_slot(nal_type: u8) -> Option<usize>;
    /// The NAL header a fragmentation unit stands for, rebuilt from its
    /// first `FU_PREFIX_LEN` bytes (callers guarantee the length).
    fn fu_nal_header(fu: &[u8]) -> Vec<u8>;
}

/// Most cache slots any format uses (H.265: VPS, SPS, PPS).
pub(crate) const MAX_PS_SLOTS: usize = 3;

/// Step between the last emitted PTS and the re-anchor point after an SSRC
/// reset. Nominal one 30 fps frame at 90 kHz (3003 ticks). Chosen to be
/// non-zero and monotonically-increasing; the exact value is not meaningful.
const SSRC_RESET_PTS_STEP: i64 = 3003;

/// Annex B framing start code prepended to each NALU.
const ANNEXB_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Whether a parameter set is small enough to retain in the cache.
///
/// A parameter set whose Annex-B-framed length (`start code + NALU`) exceeds
/// `max_au_bytes` can never appear in a conformant emitted AU, so caching it
/// would grow retained memory beyond the advertised cap and rebuild an
/// over-cap injection prefix (only to drop it) on every later IDR.
fn ps_within_cap(nalu_len: usize, max_au_bytes: usize) -> bool {
    ANNEXB_START_CODE.len() + nalu_len <= max_au_bytes
}

pub(crate) struct Core<F: Format> {
    config: DepayConfig,
    /// Cached parameter-set NALUs by slot (raw bytes, no start code). Updated
    /// from in-band parameter-set NALUs; seeded from `initial_parameter_sets`
    /// at construction. Slots beyond the format's `PS_SLOTS` stay `None`.
    ps_cache: [Option<Vec<u8>>; MAX_PS_SLOTS],
    stats: DepayStats,

    // SSRC state (rule 1)
    ssrc: Option<u32>,

    // Seq tracking (rule 2)
    last_seq: Option<u16>,

    // RTP timestamp unwrapping (rule 3)
    /// Nearest-continuation unwrap chain over the RTP timestamps.
    ts_unwrap: Unwrap32,

    // PTS anchoring
    /// Extended timestamp at which the current AU started.
    au_ts_ext: i64,
    /// PTS offset: `pts_raw = au_ts_ext - pts_base`. Set on first AU after
    /// construction or SSRC reset.
    pts_base: Option<i64>,
    /// Last PTS emitted. Initialised to `-SSRC_RESET_PTS_STEP` so the first
    /// AU lands at PTS 0 through the same re-anchor path used after resets.
    last_emitted_pts: i64,

    // Per-AU accumulator (rule 5)
    /// The RTP timestamp for the open AU (u32 wrapping form).
    au_ts: Option<u32>,
    /// Annex B buffer for the open AU.
    au_buf: Vec<u8>,
    /// Open fragmentation-unit accumulation buffer (filled by fragment reassembly; exists for
    /// rule 2 gap-clearing and rule 5 completion checks).
    fu: Option<Vec<u8>>,
    /// True if the AU should be dropped when completed.
    au_poisoned: bool,
    /// True if the next AU start should be poisoned (sticky from a seq gap).
    gap_pending: bool,
    /// AU metadata flags (rules 7, 5)
    au_has_idr: bool,
    /// Per-slot: the open AU already carries this parameter set in-band.
    au_has_ps: [bool; MAX_PS_SLOTS],

    /// Fully reassembled AUs waiting to be consumed.
    ready: VecDeque<AccessUnit>,

    _format: core::marker::PhantomData<F>,
}

impl<F: Format> Core<F> {
    /// Construct a new depacketizer with the given configuration.
    pub(crate) fn new(mut config: DepayConfig) -> Self {
        const { assert!(F::PS_SLOTS <= MAX_PS_SLOTS) };
        // Seed parameter-set cache from initial_parameter_sets. Last of each
        // type wins. Does NOT tick parameter_set_updates (seeding ≠ an update).
        let mut ps_cache: [Option<Vec<u8>>; MAX_PS_SLOTS] = [None, None, None];
        for nalu in &config.initial_parameter_sets {
            if nalu.is_empty() {
                continue;
            }
            // Skip parameter sets too large to fit the AU cap (see ps_within_cap).
            if !ps_within_cap(nalu.len(), config.max_au_bytes) {
                continue;
            }
            // Other types are ignored defensively.
            if let Some(slot) = F::ps_slot(F::nal_type(nalu)) {
                ps_cache[slot] = Some(nalu.clone());
            }
        }
        // The seed vectors are consumed into the caches above and never read
        // again (nothing accesses `config.initial_parameter_sets` after
        // construction, and there is no config accessor). Drop them so a large
        // caller-supplied seed is not retained for the depacketizer's lifetime.
        config.initial_parameter_sets = Vec::new();
        Self {
            config,
            ps_cache,
            stats: DepayStats::default(),
            ssrc: None,
            last_seq: None,
            ts_unwrap: Unwrap32::default(),
            au_ts_ext: 0,
            pts_base: None,
            last_emitted_pts: -SSRC_RESET_PTS_STEP,
            au_ts: None,
            au_buf: Vec::new(),
            fu: None,
            au_poisoned: false,
            gap_pending: false,
            au_has_idr: false,
            au_has_ps: [false; MAX_PS_SLOTS],
            ready: VecDeque::new(),
            _format: core::marker::PhantomData,
        }
    }

    /// Feed one RTP packet into the depacketizer.
    ///
    /// This never fails and never panics — any malformed or unexpected input
    /// is handled by discarding and ticking the appropriate stat counter.
    pub(crate) fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
        // ── Rule 1: SSRC ──────────────────────────────────────────────────
        if let Some(known) = self.ssrc {
            if header.ssrc != known {
                // Source restart: discard open AU without counting it.
                self.au_ts = None;
                self.au_buf.clear();
                self.fu = None;
                self.au_poisoned = false;
                self.gap_pending = false;
                self.au_has_idr = false;
                self.au_has_ps = [false; MAX_PS_SLOTS];
                // Reset seq/ts tracking.
                self.last_seq = None;
                self.ts_unwrap = Unwrap32::default();
                // PTS stays monotonic; next AU re-anchors at
                // last_emitted_pts + SSRC_RESET_PTS_STEP.
                self.pts_base = None;
                self.ssrc = Some(header.ssrc);
                self.stats.ssrc_changes += 1;
            }
        } else {
            self.ssrc = Some(header.ssrc);
        }

        // ── Rule 2: seq-gap / duplicate detection ─────────────────────────
        // Do this BEFORE boundary handling (rule 2 says "poison the
        // currently-accumulating AU before boundary handling").
        let is_gap;
        if let Some(last) = self.last_seq {
            let delta = header.seq.wrapping_sub(last);
            if delta == 0 {
                self.stats.duplicate_packets += 1;
                return; // whole packet ignored
            } else if delta == 1 {
                is_gap = false;
            } else {
                // Gap: poison the open AU right now (before boundary handling).
                if self.au_ts.is_some() {
                    self.au_poisoned = true;
                }
                // Also discard any open FU so its bytes cannot leak into a
                // later NALU after the gap (§7.3 discard guidance).
                if self.fu.take().is_some() {
                    self.stats.nalus_discarded += 1;
                }
                self.stats.seq_gaps += 1;
                is_gap = true;
            }
        } else {
            is_gap = false;
        }
        // last_seq is updated before the early return so the next packet's
        // delta is computed correctly (e.g. the empty-payload discard path).
        self.last_seq = Some(header.seq);

        // ── Rule 3: timestamp boundary ────────────────────────────────────
        // Unwrap the RTP timestamp. The chain anchors at the first value;
        // only differences matter (`pts_base` is taken from the same chain).
        let ts_ext = self.ts_unwrap.unwrap(header.timestamp);

        if let Some(open_ts) = self.au_ts {
            if header.timestamp != open_ts {
                // Different timestamp → complete the open AU first.
                self.complete_au();
            }
        }

        // Adopt this timestamp for the current AU.
        let is_new_au = self.au_ts.is_none();
        if is_new_au {
            self.au_ts = Some(header.timestamp);
            self.au_ts_ext = ts_ext;

            // Anchor PTS on first AU after construction / SSRC reset.
            if self.pts_base.is_none() {
                self.pts_base = Some(ts_ext - (self.last_emitted_pts + SSRC_RESET_PTS_STEP));
            }

            // Apply sticky gap poison to this newly-started AU.
            if is_gap || self.gap_pending {
                self.au_poisoned = true;
            }
            // gap_pending was consumed by the new-AU check above; clear it so
            // it does not cascade to the AU AFTER this one.  If is_gap is also
            // true the gap has already poisoned THIS AU — no further carry-over.
            self.gap_pending = false;
        }
        // For subsequent packets of the SAME (already-open) AU, a gap poisons
        // and sets gap_pending so the NEXT AU (which we may be about to start
        // after a markerless boundary) also gets poisoned.
        if is_gap && !is_new_au {
            self.au_poisoned = true;
            self.gap_pending = true;
        } else if !is_gap {
            self.gap_pending = false;
        }

        // ── Rule 6: payload dispatch ──────────────────────────────────────
        if payload.is_empty() {
            self.stats.packets_discarded += 1;
            return;
        }
        match F::classify(payload) {
            Payload::Single => {
                // Single NALU packet.
                self.push_nalu(payload);
            }
            Payload::Aggregation => {
                // ── Rule 6: aggregation packet ───────────────────────────────
                self.push_aggregation(payload);
            }
            Payload::Fragment => {
                // Fragmentation unit.
                self.push_fragment(payload);
            }
            Payload::Poison => {
                // Interleaved-mode-only types — unrecoverable, poison the AU.
                self.stats.packets_discarded += 1;
                self.au_poisoned = true;
            }
            Payload::Reserved => {
                // Reserved types: discard WITHOUT poisoning.
                self.stats.packets_discarded += 1;
            }
        }

        // ── Rule 4: marker fast-path ──────────────────────────────────────
        if header.marker {
            self.complete_au();
            // A marker establishes a firm AU boundary — the gap ambiguity
            // from any in-flight `gap_pending` is resolved; the next AU
            // starts clean unless it has its own gap.
            self.gap_pending = false;
        }
    }

    /// Pull the next completed Access Unit, if one is available.
    ///
    /// Call repeatedly until `None` after each [`feed`](Self::feed) call.
    pub(crate) fn next_au(&mut self) -> Option<AccessUnit> {
        self.ready.pop_front()
    }

    /// Force completion of any open AU and return it.
    ///
    /// The caller should drain [`Self::next_au`] before calling this. After
    /// `flush` the depacketizer state is clean.
    pub(crate) fn flush(&mut self) -> Option<AccessUnit> {
        self.complete_au();
        self.ready.pop_front()
    }

    /// Return a snapshot of the current statistics.
    pub(crate) fn stats(&self) -> DepayStats {
        self.stats
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Complete the open AU (rule 5).
    fn complete_au(&mut self) {
        if self.au_ts.is_none() {
            // Nothing open.
            return;
        }

        // An open FU at completion → discard + poison.
        if self.fu.take().is_some() {
            self.stats.nalus_discarded += 1;
            self.au_poisoned = true;
        }

        if self.au_buf.is_empty() && !self.au_poisoned {
            // Empty unpoisoned AU — reset silently.
            self.reset_au();
            return;
        }

        if self.au_poisoned {
            self.stats.aus_dropped += 1;
            self.reset_au();
            return;
        }

        // ── BeforeIdr injection ───────────────────────────────────────────────
        // If the AU is an IDR and BeforeIdr injection is enabled, prepend any
        // cached parameter sets that are not already present in this AU.
        if self.au_has_idr
            && self.config.parameter_set_injection == ParameterSetInjection::BeforeIdr
        {
            // Framed length of each cached set that WOULD be injected (0 when the
            // AU already carries it in-band or the cache is empty).
            let mut inject = [0usize; MAX_PS_SLOTS];
            for (i, len) in inject.iter_mut().enumerate().take(F::PS_SLOTS) {
                if !self.au_has_ps[i] {
                    *len = self.ps_cache[i]
                        .as_ref()
                        .map_or(0, |s| ANNEXB_START_CODE.len() + s.len());
                }
            }
            let inject_len: usize = inject.iter().sum();
            if inject_len > 0 {
                // Preflight the cap BEFORE allocating/copying the prefix:
                // injection prepends cached sets after the append-time checks, so
                // individually-in-cap parameter sets combined with the IDR AU can
                // still exceed the cap. Drop early — never build an over-cap
                // prefix (fu is already None here).
                let total = inject_len.saturating_add(self.au_buf.len());
                if total > self.config.max_au_bytes {
                    self.stats.aus_dropped_oversize += 1;
                    self.stats.aus_dropped += 1;
                    self.reset_au();
                    return;
                }
                let mut prefix = Vec::with_capacity(total);
                for (i, len) in inject.iter().enumerate().take(F::PS_SLOTS) {
                    if *len > 0 {
                        prefix.extend_from_slice(&ANNEXB_START_CODE);
                        prefix.extend_from_slice(self.ps_cache[i].as_deref().unwrap());
                    }
                }
                prefix.extend_from_slice(&self.au_buf);
                self.au_buf = prefix;
            }
        }

        // Compute PTS.
        let pts_base = self.pts_base.unwrap_or(0);
        let pts_raw = self.au_ts_ext - pts_base;
        let pts = Pts90khz::new(pts_raw);
        self.last_emitted_pts = pts_raw;

        // Collect the completed AU.
        let annexb = core::mem::take(&mut self.au_buf);
        let au = AccessUnit {
            annexb,
            pts,
            key_frame: self.au_has_idr,
            rtp_timestamp: self.au_ts.unwrap_or(0),
        };
        self.stats.aus_emitted += 1;
        self.ready.push_back(au);
        self.reset_au();
    }

    /// Enforce the AU accumulation cap after an append. If the combined size
    /// of `au_buf` and the open FU buffer exceeds [`DepayConfig::max_au_bytes`]:
    /// immediately clear both buffers (memory is released, not just flagged),
    /// poison the AU, and tick `aus_dropped_oversize`. Returns `true` if the
    /// cap was exceeded so the caller can short-circuit further work.
    ///
    /// The AU is poisoned so `complete_au()` will tick `aus_dropped` at the
    /// normal boundary site.
    fn check_and_apply_oversize_cap(&mut self) -> bool {
        let fu_len = self.fu.as_ref().map_or(0, |b| b.len());
        let total = self.au_buf.len().saturating_add(fu_len);
        if total <= self.config.max_au_bytes {
            return false;
        }
        // Release memory immediately — poisoning alone would let bytes continue
        // accumulating until the next AU boundary.
        self.au_buf = Vec::new();
        self.fu = None;
        self.au_poisoned = true;
        self.stats.aus_dropped_oversize += 1;
        true
    }

    /// Push one NALU into the open AU buffer (rule 7).
    ///
    /// If the F bit is set the NALU is corrupt — discard and poison.
    fn push_nalu(&mut self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty(), "dispatch guarantees non-empty NALUs");
        if bytes.is_empty() {
            // Unreachable via feed()'s dispatch guard; kept as a release-mode
            // safety net (aggregation unpacking also feeds sub-NALUs through here).
            return;
        }
        self.push_nalu_owned(bytes.to_owned());
    }

    /// Owned-buffer variant of [`push_nalu`] — used by fragment reassembly to
    /// avoid a copy when the reconstructed buffer is already heap-allocated.
    ///
    /// Checks the F bit, updates AU metadata flags, and appends Annex B
    /// framing exactly as `push_nalu` does.
    fn push_nalu_owned(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        // Skip buffering entirely for poisoned AUs — their bytes are dropped
        // at `complete_au()` anyway, and continuing to extend `au_buf` would
        // defeat the `max_au_bytes` cap (attacker could still fill memory).
        if self.au_poisoned {
            return;
        }
        if bytes[0] & 0x80 != 0 {
            // F bit set → advertised corrupt, §5.3.
            self.stats.nalus_discarded += 1;
            self.au_poisoned = true;
            return;
        }
        let t = F::nal_type(&bytes);
        if F::is_idr(t) {
            self.au_has_idr = true;
        }
        if let Some(slot) = F::ps_slot(t) {
            self.au_has_ps[slot] = true;
            // Update the cache (only if it fits the AU cap; see ps_within_cap);
            // tick the counter only when bytes change.
            if ps_within_cap(bytes.len(), self.config.max_au_bytes)
                && self.ps_cache[slot].as_deref() != Some(bytes.as_slice())
            {
                self.ps_cache[slot] = Some(bytes.clone());
                self.stats.parameter_set_updates += 1;
            }
        }
        // Append Annex B start code + NALU bytes, then enforce the cap.
        // Checking after the append catches both accumulated growth and a single
        // enormous NALU in one place without pre-computing the new size.
        self.au_buf.extend_from_slice(&ANNEXB_START_CODE);
        self.au_buf.extend_from_slice(&bytes);
        self.check_and_apply_oversize_cap();
    }

    /// Unpack and reassemble a fragmentation unit.
    ///
    /// Payload layout: the payload header and FU header occupy the first
    /// `FU_PREFIX_LEN` bytes, the FU header (the last of them) carrying the
    /// `S` (start) and `E` (end) bits; the rest is fragment data. The
    /// reconstructed NAL header comes from [`Format::fu_nal_header`].
    fn push_fragment(&mut self, payload: &[u8]) {
        // ── Malformed: shorter than the payload header + FU header ───────────
        if payload.len() < F::FU_PREFIX_LEN {
            self.stats.packets_discarded += 1;
            // Discard open FU and poison.
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
            }
            self.au_poisoned = true;
            return;
        }

        let ind = payload[0];
        let fh = payload[F::FU_PREFIX_LEN - 1];
        let fragment = &payload[F::FU_PREFIX_LEN..]; // may be empty, legal

        // ── F bit on the FU indicator → corrupt NALU ─────────────────────────
        if ind & 0x80 != 0 {
            self.stats.nalus_discarded += 1;
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
            }
            self.au_poisoned = true;
            return;
        }

        let s = fh & 0x80 != 0; // Start bit
        let e = fh & 0x40 != 0; // End bit

        // ── S==1 && E==1: forbidden ──────────────────────────────────────
        if s && e {
            self.stats.packets_discarded += 1;
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
            }
            self.au_poisoned = true;
            return;
        }

        if s {
            // ── Start of a new fragmented NALU ──────────────────────────────────────
            // If there is already an open FU, the previous NALU never finished.
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
                self.au_poisoned = true;
            }
            // Skip buffering for already-poisoned AUs (cap enforcement or prior
            // error) — no point accumulating bytes we'll discard at completion.
            if self.au_poisoned {
                return;
            }
            // Reconstruct the NALU header and open the accumulation buffer.
            let mut buf = F::fu_nal_header(payload);
            buf.reserve(fragment.len());
            buf.extend_from_slice(fragment);
            self.fu = Some(buf);
            // Enforce cap after the first fragment is stored. A single large
            // start fragment may already push `au_buf + fu` over the limit.
            if self.check_and_apply_oversize_cap() {
                return;
            }
            // `fu` is now Some — safe to continue.
        } else {
            // ── Continuation or end fragment ──────────────────────────────────
            if self.au_poisoned {
                // Already poisoned (e.g. by cap enforcement on a prior packet):
                // discard continuation bytes without extending any buffer.
                return;
            }
            if let Some(ref mut buf) = self.fu {
                buf.extend_from_slice(fragment);
                // Check cap after each append — a slow-drip attack feeds small
                // continuation fragments indefinitely.
                if self.check_and_apply_oversize_cap() {
                    return;
                }
            } else {
                // No open FU — the start packet was lost (§7.3 discard guidance).
                self.stats.nalus_discarded += 1;
                self.au_poisoned = true;
                return;
            }
        }

        if e {
            // ── End of the fragmented NALU: close and push ─────────────────────────
            let buf = self.fu.take().expect("fu is Some — set above or continued");
            self.push_nalu_owned(buf);
        }
    }

    /// Unpack an aggregation packet.
    ///
    /// Layout: `[header (AGG_PREFIX_LEN bytes)] [size u16 BE] [NALU] ... [size u16 BE] [NALU]`
    ///
    /// Malformed conditions (any → `packets_discarded += 1` + poison + return):
    /// - fewer than 2 bytes remain for the size field (trailing 1-byte remainder)
    /// - `size == 0` (degenerate)
    /// - `size` extends past the end of the payload (truncated NALU data)
    fn push_aggregation(&mut self, payload: &[u8]) {
        // Skip the aggregation packet's header.
        let mut cursor = F::AGG_PREFIX_LEN;
        while cursor < payload.len() {
            // ── Need 2 bytes for the size field ──────────────────────────────
            if cursor + 2 > payload.len() {
                // Trailing 1-byte remainder — malformed.
                self.stats.packets_discarded = self.stats.packets_discarded.saturating_add(1);
                self.au_poisoned = true;
                return;
            }
            let size = u16::from_be_bytes([payload[cursor], payload[cursor + 1]]) as usize;

            // ── Zero size or data runs past end ───────────────────────────────
            if size == 0 || cursor + 2 + size > payload.len() {
                self.stats.packets_discarded = self.stats.packets_discarded.saturating_add(1);
                self.au_poisoned = true;
                return;
            }

            // ── Push this aggregation unit's NALU ────────────────────────────
            self.push_nalu(&payload[cursor + 2..cursor + 2 + size]);

            cursor += 2 + size;
        }
    }

    /// Reset per-AU state (called after completing or discarding an AU).
    fn reset_au(&mut self) {
        self.au_ts = None;
        self.au_buf.clear();
        self.fu = None;
        self.au_poisoned = false;
        self.au_has_idr = false;
        self.au_has_ps = [false; MAX_PS_SLOTS];
        // gap_pending is NOT reset here — it must persist to poison the next
        // AU; consumed in the au_ts.is_none() new-AU block in feed().
    }
}
