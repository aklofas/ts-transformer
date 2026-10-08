//! RFC 6184 H.264 RTP depacketizer — AU reassembly state machine.
//!
//! # Overview
//!
//! [`H264Depacketizer`] reassembles Access Units (AUs) from RTP packets
//! carrying H.264 NAL units per RFC 6184. It follows the `feed()` / `next_au()`
//! idiom used by [`tst_core::mpegts::demux::Demuxer`]: call [`feed`] for each
//! received RTP packet, then drain [`next_au`] until it returns `None`.
//!
//! # AU boundary rules (state-machine contract)
//!
//! 1. **SSRC**: the first packet latches the SSRC. A different SSRC signals a
//!    source restart — the open AU is discarded (no `aus_dropped` tick), seq/
//!    timestamp state is reset, and `ssrc_changes` is incremented. PTS stays
//!    monotonic across SSRC resets (the re-anchor adds one nominal frame step
//!    to the last emitted value before anchoring the new stream).
//!
//! 2. **Sequence gaps**: a gap (`delta ≠ 1`) poisons the currently-accumulating
//!    AU (if any) *before* boundary handling, then also poisons the AU this
//!    packet joins *after* boundary handling (sticky `gap_pending` flag).
//!    Duplicate packets (`delta = 0`) are silently ignored.
//!
//! 3. **Timestamp boundary**: when `header.timestamp` differs from the ongoing
//!    AU's timestamp, `complete_au()` is called first, then the new timestamp is
//!    adopted. One AU per RTP timestamp per RFC 6184 §5.1.
//!
//! 4. **Marker fast-path**: `M=1` calls `complete_au()` immediately after
//!    dispatching the packet's payload. This is an early-emission hint, not the
//!    sole boundary mechanism — rule 3 is the correctness path.
//!
//! 5. **`complete_au()`**: an open FU at completion counts as a discarded NALU
//!    and poisons the AU. Empty unpoisoned AUs reset silently. Poisoned AUs
//!    increment `aus_dropped`. Clean AUs push an [`H264Au`] onto the ready queue.
//!
//! 6. **Payload dispatch** on `payload[0] & 0x1F` (NALU type): types 1–23 are
//!    single NAL units; type 24 = STAP-A aggregation packets; type 28 = FU-A
//!    fragmentation units; types 25/26/27/29 are interleaved-mode-only and poison
//!    the AU; types 0/30/31 are reserved and are discarded without poisoning.
//!
//! 7. **`push_nalu(bytes)`**: if the F bit (`bytes[0] & 0x80`) is set the NALU
//!    is discarded and the AU is poisoned. Otherwise `[0,0,0,1]` + bytes is
//!    appended. NALU types 5/7/8 set `au_has_idr`/`au_has_sps`/`au_has_pps`.
//!
//! 8. **[`flush`](H264Depacketizer::flush)**: equivalent to `complete_au()` then
//!    [`next_au`](H264Depacketizer::next_au). The caller should drain `next_au()`
//!    before calling `flush`.
//!
//! # B-frames
//!
//! This depacketizer targets low-latency, no-B-frame camera streams (the primary
//! use case for gimbaled-platform video). RTP timestamps reflect decode order, not
//! display order, so [`H264Au::pts`] is a **decode-order** timestamp — B-frame
//! content can produce non-monotonic PTS values, which are passed through unaltered.
//! DTS is not derivable from RTP; callers with B-frame sources must derive DTS
//! themselves and supply it to `push_video_to_with_dts`.
//!
//! [`feed`]: H264Depacketizer::feed
//! [`next_au`]: H264Depacketizer::next_au

use std::collections::VecDeque;

use tst_core::mpegts::common::Pts90khz;

use crate::packet::RtpHeader;

/// Step between the last emitted PTS and the re-anchor point after an SSRC
/// reset. Nominal one 30 fps frame at 90 kHz (3003 ticks). Chosen to be
/// non-zero and monotonically-increasing; the exact value is not meaningful.
const SSRC_RESET_PTS_STEP: i64 = 3003;

/// Annex B framing start code prepended to each NALU.
const ANNEXB_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Whether a parameter set is small enough to retain in the SPS/PPS cache.
///
/// A parameter set whose Annex-B-framed length (`start code + NALU`) exceeds
/// `max_au_bytes` can never appear in a conformant emitted AU, so caching it
/// would grow retained memory beyond the advertised cap and rebuild an
/// over-cap injection prefix (only to drop it) on every later IDR.
fn ps_within_cap(nalu_len: usize, max_au_bytes: usize) -> bool {
    ANNEXB_START_CODE.len() + nalu_len <= max_au_bytes
}

// ──────────────────────────────────────────────────────────────────────────────
// Public types (frozen — later tasks and two binding mirrors depend on
// these exact names / signatures).
// ──────────────────────────────────────────────────────────────────────────────

/// A fully reassembled H.264 Access Unit, ready for decoding or muxing.
///
/// The `annexb` buffer contains all NALUs concatenated in Annex B framing
/// (`[0,0,0,1]` start code before each NALU), in RTP packet order.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct H264Au {
    /// Annex B–framed NALU bytes (one or more NALUs concatenated with
    /// `[0,0,0,1]` start codes).
    pub annexb: Vec<u8>,
    /// 90 kHz presentation timestamp, delivered in decode order, derived
    /// from the RTP timestamp.
    ///
    /// Zero-based at the first started AU (even one later dropped as
    /// poisoned, so the first emitted AU need not read 0); unwrapped across the 32-bit RTP
    /// timestamp rollover. Because RTP timestamps are presentation times sent in decode order, B-frame
    /// content produces non-monotonic PTS values — the depacketizer passes them
    /// through unaltered. DTS is not derivable from RTP; callers with B-frame
    /// sources must derive DTS themselves for `push_video_to_with_dts`.
    ///
    /// Values can be negative if a later AU's unwrapped timestamp falls below
    /// the first AU's anchor (e.g. after an SSRC reset with a lower timestamp
    /// origin). PTS **is** monotonic across SSRC resets: the re-anchor adds
    /// one nominal frame step (3003 ticks at 90 kHz ≈ 30 fps) to the last
    /// emitted value before anchoring the new stream.
    pub pts: Pts90khz,
    /// `true` if the AU contains at least one IDR slice (NALU type 5).
    pub key_frame: bool,
    /// The RTP timestamp for this AU as carried in the RTP header.
    pub rtp_timestamp: u32,
}

/// Controls whether out-of-band SPS/PPS are injected before IDR frames.
///
/// See the parameter-set cache section of the [`H264Depacketizer`] docs for
/// the full cache-and-injection contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParameterSetInjection {
    /// No injection — pass NALUs through exactly as received.
    None,
    /// Inject cached SPS and PPS NALUs before every IDR frame. Useful for
    /// enabling random-access decoding.
    BeforeIdr,
}

impl Default for ParameterSetInjection {
    fn default() -> Self {
        Self::BeforeIdr
    }
}

/// Configuration for [`H264Depacketizer`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct H264DepayConfig {
    /// Expected RTP payload type (7 bits).
    ///
    /// This field is carried here so that SDP negotiation can hand the receiver
    /// a single config object. [`H264Depacketizer::feed`] itself does **not**
    /// read it — the depacketizer processes whatever packet it is given,
    /// regardless of PT. PT filtering is the I/O layer's responsibility: the
    /// receiver ([`H264Receiver`](crate::h264::H264Receiver)) compares `header.payload_type`
    /// against this value before calling `feed`. Callers that source packets
    /// from a foreign mux or socket must filter PT upstream before feeding.
    pub payload_type: u8,
    /// Whether to inject cached SPS/PPS before IDR frames.
    pub parameter_set_injection: ParameterSetInjection,
    /// Out-of-band parameter sets from SDP `sprop-parameter-sets`. Each
    /// element is one raw NALU (type 7 or 8). Seeded into the parameter-set
    /// cache at construction and used by [`ParameterSetInjection::BeforeIdr`].
    pub initial_parameter_sets: Vec<Vec<u8>>,
    /// Maximum combined byte count for a single AU's accumulation buffers
    /// (`au_buf` + the open FU-A buffer). When this limit is exceeded the
    /// buffers are immediately cleared (memory released, not just flagged),
    /// the AU is poisoned, and [`H264DepayStats::aus_dropped_oversize`] is
    /// incremented. The drop is also counted in
    /// [`H264DepayStats::aus_dropped`] at the normal AU-boundary tick site.
    ///
    /// This bound closes a DoS vector: on an unconnected UDP socket (or a
    /// hostile interleaved RTSP server) an attacker can hold a constant RTP
    /// timestamp and send contiguous sequence numbers indefinitely, causing
    /// `au_buf` to grow without limit. The default (8 MiB) is generous for
    /// any real H.264 AU; lower the value in memory-constrained environments.
    ///
    /// A value of `0` is degenerate: every non-empty AU exceeds a zero cap and
    /// is dropped, so nothing is ever emitted. The Python and JVM config
    /// builders reject `0`; the raw Rust struct accepts it (documented here).
    pub max_au_bytes: usize,
}

impl Default for H264DepayConfig {
    fn default() -> Self {
        Self {
            payload_type: 96,
            parameter_set_injection: ParameterSetInjection::BeforeIdr,
            initial_parameter_sets: Vec::new(),
            max_au_bytes: 8 * 1024 * 1024, // 8 MiB
        }
    }
}

/// Counters for monitoring the depacketizer.
///
/// Returned by value from [`H264Depacketizer::stats`] (the struct is `Copy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct H264DepayStats {
    /// Number of complete, unpoisoned AUs emitted.
    pub aus_emitted: u64,
    /// Number of AUs discarded due to poisoning (seq gaps, F-bit, etc.).
    /// Includes AUs dropped for exceeding `max_au_bytes`
    /// (those are also counted in `aus_dropped_oversize`).
    pub aus_dropped: u64,
    /// Number of AUs dropped specifically because their accumulated buffer
    /// size exceeded [`H264DepayConfig::max_au_bytes`]. Every oversize drop
    /// also increments `aus_dropped` at the normal AU-boundary tick site.
    pub aus_dropped_oversize: u64,
    /// Number of RTP packets discarded (empty, reserved, interleaved types).
    pub packets_discarded: u64,
    /// Number of NALUs discarded (F-bit set, open FU at AU completion, etc.).
    pub nalus_discarded: u64,
    /// Number of sequence-number gaps detected.
    pub seq_gaps: u64,
    /// Number of duplicate sequence numbers detected.
    pub duplicate_packets: u64,
    /// Number of times cached parameter sets were updated (in-band SPS/PPS
    /// bytes differed from the cached value).
    pub parameter_set_updates: u64,
    /// Number of SSRC changes (source restarts) detected.
    pub ssrc_changes: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Depacketizer
// ──────────────────────────────────────────────────────────────────────────────

/// RFC 6184 H.264 RTP depacketizer.
///
/// Call [`feed`](Self::feed) for each received RTP packet (in order of
/// arrival), then drain [`next_au`](Self::next_au) until it returns `None`.
///
/// # Panic-freedom
///
/// `feed` never panics on any input byte pattern — adversarial payloads are
/// handled by silently discarding and ticking the appropriate stat counter.
///
/// # Parameter-set cache and BeforeIdr injection
///
/// The depacketizer maintains a one-slot SPS cache and one-slot PPS cache.
/// Both are seeded from [`H264DepayConfig::initial_parameter_sets`] at
/// construction (type 7 → SPS, type 8 → PPS; last of each wins; seeding does
/// NOT tick `parameter_set_updates`).  In-band SPS/PPS NALUs update the cache
/// only when their bytes differ from the cached value; each update ticks
/// `parameter_set_updates`.
///
/// When [`ParameterSetInjection::BeforeIdr`] is active (the default), every
/// IDR AU that does not already carry an in-band SPS or PPS receives the
/// missing cached parameter set(s) prepended — SPS first, then PPS — each
/// with a 4-byte Annex B start code.  [`ParameterSetInjection::None`] passes
/// NALUs through unchanged.
///
/// # AU boundary / marker / poisoning contract summary
///
/// See the [module-level doc](self) for the full 8-rule state-machine contract.
pub struct H264Depacketizer {
    config: H264DepayConfig,
    /// Cached SPS NALU (raw bytes, no start code). Updated from in-band type-7
    /// NALUs; seeded from `initial_parameter_sets` at construction.
    sps_cache: Option<Vec<u8>>,
    /// Cached PPS NALU (raw bytes, no start code). Updated from in-band type-8
    /// NALUs; seeded from `initial_parameter_sets` at construction.
    pps_cache: Option<Vec<u8>>,
    stats: H264DepayStats,

    // SSRC state (rule 1)
    ssrc: Option<u32>,

    // Seq tracking (rule 2)
    last_seq: Option<u16>,

    // RTP timestamp unwrapping (rule 3)
    /// Last RTP timestamp seen (wrapping u32).
    last_ts: Option<u32>,
    /// Extended (unwrapped) timestamp corresponding to `last_ts`.
    ts_ext: i64,

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
    /// Open FU-A accumulation buffer (filled by FU-A reassembly; exists for
    /// rule 2 gap-clearing and rule 5 completion checks).
    fu: Option<Vec<u8>>,
    /// True if the AU should be dropped when completed.
    au_poisoned: bool,
    /// True if the next AU start should be poisoned (sticky from a seq gap).
    gap_pending: bool,
    /// AU metadata flags (rules 7, 5)
    au_has_idr: bool,
    au_has_sps: bool,
    au_has_pps: bool,

    /// Fully reassembled AUs waiting to be consumed.
    ready: VecDeque<H264Au>,
}

impl H264Depacketizer {
    /// Construct a new depacketizer with the given configuration.
    pub fn new(mut config: H264DepayConfig) -> Self {
        // Seed parameter-set cache from initial_parameter_sets. Last of each
        // type wins. Does NOT tick parameter_set_updates (seeding ≠ an update).
        let mut sps_cache: Option<Vec<u8>> = None;
        let mut pps_cache: Option<Vec<u8>> = None;
        for nalu in &config.initial_parameter_sets {
            if nalu.is_empty() {
                continue;
            }
            // Skip parameter sets too large to fit the AU cap (see ps_within_cap).
            if !ps_within_cap(nalu.len(), config.max_au_bytes) {
                continue;
            }
            match nalu[0] & 0x1F {
                7 => sps_cache = Some(nalu.clone()),
                8 => pps_cache = Some(nalu.clone()),
                _ => {} // other types ignored defensively
            }
        }
        // The seed vectors are consumed into the caches above and never read
        // again (nothing accesses `config.initial_parameter_sets` after
        // construction, and there is no config accessor). Drop them so a large
        // caller-supplied seed is not retained for the depacketizer's lifetime.
        config.initial_parameter_sets = Vec::new();
        Self {
            config,
            sps_cache,
            pps_cache,
            stats: H264DepayStats::default(),
            ssrc: None,
            last_seq: None,
            last_ts: None,
            ts_ext: 0,
            au_ts_ext: 0,
            pts_base: None,
            last_emitted_pts: -SSRC_RESET_PTS_STEP,
            au_ts: None,
            au_buf: Vec::new(),
            fu: None,
            au_poisoned: false,
            gap_pending: false,
            au_has_idr: false,
            au_has_sps: false,
            au_has_pps: false,
            ready: VecDeque::new(),
        }
    }

    /// Feed one RTP packet into the depacketizer.
    ///
    /// This never fails and never panics — any malformed or unexpected input
    /// is handled by discarding and ticking the appropriate stat counter.
    pub fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
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
                self.au_has_sps = false;
                self.au_has_pps = false;
                // Reset seq/ts tracking.
                self.last_seq = None;
                self.last_ts = None;
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
        // Unwrap the RTP timestamp.
        let ts_ext = if let Some(last_ts) = self.last_ts {
            // Nearest-distance unwrap: cast the wrapping_sub to i32 then i64.
            self.ts_ext + (header.timestamp.wrapping_sub(last_ts) as i32 as i64)
        } else {
            // First packet seeds ts_ext = 0.
            0
        };
        self.last_ts = Some(header.timestamp);
        self.ts_ext = ts_ext;

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
        let nalu_type = payload[0] & 0x1F;
        match nalu_type {
            1..=23 => {
                // Single NALU packet (RFC 6184 §5.6).
                self.push_nalu(payload);
            }
            24 => {
                // ── Rule 6: STAP-A aggregation packet (RFC 6184 §5.7.1) ────────
                self.push_stap_a(payload);
            }
            28 => {
                // FU-A fragmentation unit (RFC 6184 §5.8).
                self.push_fu_a(payload);
            }
            25 | 26 | 27 | 29 => {
                // Interleaved-mode-only types — unrecoverable, poison the AU.
                self.stats.packets_discarded += 1;
                self.au_poisoned = true;
            }
            _ => {
                // Types 0 and 30–31: reserved per §5.4 Table 3 ("ig").
                // Discard WITHOUT poisoning.
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
    pub fn next_au(&mut self) -> Option<H264Au> {
        self.ready.pop_front()
    }

    /// Force completion of any open AU and return it.
    ///
    /// The caller should drain [`Self::next_au`] before calling this. After
    /// `flush` the depacketizer state is clean.
    pub fn flush(&mut self) -> Option<H264Au> {
        self.complete_au();
        self.ready.pop_front()
    }

    /// Return a snapshot of the current statistics.
    pub fn stats(&self) -> H264DepayStats {
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
            let sps_inject = if !self.au_has_sps {
                self.sps_cache
                    .as_ref()
                    .map_or(0, |s| ANNEXB_START_CODE.len() + s.len())
            } else {
                0
            };
            let pps_inject = if !self.au_has_pps {
                self.pps_cache
                    .as_ref()
                    .map_or(0, |p| ANNEXB_START_CODE.len() + p.len())
            } else {
                0
            };
            let inject_len = sps_inject + pps_inject;
            if inject_len > 0 {
                // Preflight the cap BEFORE allocating/copying the prefix:
                // injection prepends cached sets after the append-time checks, so
                // individually-in-cap SPS/PPS combined with the IDR AU can still
                // exceed the cap. Drop early — never build an over-cap prefix (fu
                // is already None here).
                let total = inject_len.saturating_add(self.au_buf.len());
                if total > self.config.max_au_bytes {
                    self.stats.aus_dropped_oversize += 1;
                    self.stats.aus_dropped += 1;
                    self.reset_au();
                    return;
                }
                let mut prefix = Vec::with_capacity(total);
                if sps_inject > 0 {
                    prefix.extend_from_slice(&ANNEXB_START_CODE);
                    prefix.extend_from_slice(self.sps_cache.as_deref().unwrap());
                }
                if pps_inject > 0 {
                    prefix.extend_from_slice(&ANNEXB_START_CODE);
                    prefix.extend_from_slice(self.pps_cache.as_deref().unwrap());
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
        let au = H264Au {
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
    /// of `au_buf` and the open FU buffer exceeds [`H264DepayConfig::max_au_bytes`]:
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
            // safety net (STAP-A unpacking also feeds sub-NALUs through here).
            return;
        }
        self.push_nalu_owned(bytes.to_owned());
    }

    /// Owned-buffer variant of [`push_nalu`] — used by FU-A reassembly to
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
        let nalu_type = bytes[0] & 0x1F;
        match nalu_type {
            5 => self.au_has_idr = true,
            7 => {
                self.au_has_sps = true;
                // Update SPS cache (only if it fits the AU cap; see ps_within_cap);
                // tick the counter only when bytes change.
                if ps_within_cap(bytes.len(), self.config.max_au_bytes)
                    && self.sps_cache.as_deref() != Some(bytes.as_slice())
                {
                    self.sps_cache = Some(bytes.clone());
                    self.stats.parameter_set_updates += 1;
                }
            }
            8 => {
                self.au_has_pps = true;
                // Update PPS cache (only if it fits the AU cap; see ps_within_cap);
                // tick the counter only when bytes change.
                if ps_within_cap(bytes.len(), self.config.max_au_bytes)
                    && self.pps_cache.as_deref() != Some(bytes.as_slice())
                {
                    self.pps_cache = Some(bytes.clone());
                    self.stats.parameter_set_updates += 1;
                }
            }
            _ => {}
        }
        // Append Annex B start code + NALU bytes, then enforce the cap.
        // Checking after the append catches both accumulated growth and a single
        // enormous NALU in one place without pre-computing the new size.
        self.au_buf.extend_from_slice(&ANNEXB_START_CODE);
        self.au_buf.extend_from_slice(&bytes);
        self.check_and_apply_oversize_cap();
    }

    /// Unpack and reassemble an FU-A fragmentation unit (RFC 6184 §5.8).
    ///
    /// FU-A payload layout:
    /// - byte 0: FU indicator — `F | NRI | 28`
    /// - byte 1: FU header   — `S | E | R | orig-type`
    /// - bytes 2..: fragment data
    ///
    /// The reconstructed NALU header is `(indicator & 0xE0) | (fu_header & 0x1F)`.
    fn push_fu_a(&mut self, payload: &[u8]) {
        // ── Malformed: fewer than 2 bytes ────────────────────────────────────
        if payload.len() < 2 {
            self.stats.packets_discarded += 1;
            // Discard open FU and poison.
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
            }
            self.au_poisoned = true;
            return;
        }

        let ind = payload[0];
        let fh = payload[1];
        let fragment = &payload[2..]; // may be empty — §5.8 note, legal

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

        // ── S==1 && E==1: §5.8 MUST NOT ──────────────────────────────────────
        if s && e {
            self.stats.packets_discarded += 1;
            if self.fu.take().is_some() {
                self.stats.nalus_discarded += 1;
            }
            self.au_poisoned = true;
            return;
        }

        if s {
            // ── Start of a new FU-A NALU ──────────────────────────────────────
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
            let nalu_hdr = (ind & 0xE0) | (fh & 0x1F);
            let mut buf = Vec::with_capacity(1 + fragment.len());
            buf.push(nalu_hdr);
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
            // ── End of the FU-A NALU: close and push ─────────────────────────
            let buf = self.fu.take().expect("fu is Some — set above or continued");
            self.push_nalu_owned(buf);
        }
    }

    /// Unpack a STAP-A aggregation packet (RFC 6184 §5.7.1 Figure 6).
    ///
    /// Layout: `[STAP-A hdr (1 byte)] [size u16 BE] [NALU] ... [size u16 BE] [NALU]`
    ///
    /// Malformed conditions (any → `packets_discarded += 1` + poison + return):
    /// - fewer than 2 bytes remain for the size field (trailing 1-byte remainder)
    /// - `size == 0` (degenerate)
    /// - `size` extends past the end of the payload (truncated NALU data)
    fn push_stap_a(&mut self, payload: &[u8]) {
        // cursor starts at 1: skip the STAP-A header byte.
        let mut cursor = 1usize;
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
        self.au_has_sps = false;
        self.au_has_pps = false;
        // gap_pending is NOT reset here — it must persist to poison the next
        // AU; consumed in the au_ts.is_none() new-AU block in feed().
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u16, ts: u32, marker: bool) -> RtpHeader {
        let mut h = RtpHeader::new(seq, ts, 0xAABB);
        h.marker = marker;
        h.payload_type = 96;
        h
    }

    fn depay() -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            parameter_set_injection: ParameterSetInjection::None, // injection tested separately below
            ..H264DepayConfig::default()
        })
    }

    #[test]
    fn single_nalu_au_completed_by_marker() {
        let mut d = depay();
        d.feed(&hdr(1, 90_000, true), &[0x65, 0xAA]); // type-5 IDR slice
        let au = d.next_au().expect("marker completes AU");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert!(au.key_frame);
        assert_eq!(au.pts, Pts90khz::new(0)); // first AU zero-based
        assert_eq!(au.rtp_timestamp, 90_000);
    }

    #[test]
    fn markerless_au_completed_by_timestamp_change() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // non-IDR slice
        assert!(d.next_au().is_none()); // no marker → held
        d.feed(&hdr(2, 4003, false), &[0x41, 0x02]); // next AU's first packet
        let au = d.next_au().expect("ts change completes previous AU");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
        assert!(!au.key_frame);
        assert_eq!(au.pts, Pts90khz::new(0));
        // second AU: PTS = delta from first = 3003
        let au2 = d.flush().expect("flush emits final AU");
        assert_eq!(au2.pts, Pts90khz::new(3003));
    }

    #[test]
    fn two_nalus_one_timestamp_one_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x67, 0x42]); // SPS
        d.feed(&hdr(2, 1000, true), &[0x65, 0x88]); // IDR, marker
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x65, 0x88]);
        assert!(au.key_frame);
    }

    #[test]
    fn timestamp_wrap_unwraps_monotonically() {
        let mut d = depay();
        // (u32::MAX - 1000) + 3003 wraps to 2002, so +3003 ticks across the u32 boundary.
        // Note: the brief used 2003 here, but (u32::MAX-1000) → 2003 is a delta of 3004,
        // not 3003. Using 2002 matches the expected output of Pts90khz::new(3003).
        d.feed(&hdr(1, u32::MAX - 1000, true), &[0x41, 0x01]);
        d.feed(&hdr(2, 2002, true), &[0x41, 0x02]); // wrapped: +3003 across the u32 boundary
        let a = d.next_au().unwrap();
        let b = d.next_au().unwrap();
        assert_eq!(a.pts, Pts90khz::new(0));
        assert_eq!(b.pts, Pts90khz::new(3003));
    }

    #[test]
    fn reserved_types_ignored_without_poisoning() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        d.feed(&hdr(2, 1000, false), &[30, 0xFF]); // reserved type 30: dropped, AU survives
        d.feed(&hdr(3, 1000, true), &[0x41, 0x02]);
        let au = d.next_au().expect("reserved type must not poison");
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(au.annexb.len(), 4 + 2 + 4 + 2);
    }

    #[test]
    fn duplicate_seq_ignored() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // exact dup
        d.feed(&hdr(2, 1000, true), &[0x41, 0x02]);
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb.len(), 12);
        assert_eq!(d.stats().duplicate_packets, 1);
    }

    // ── Additional correctness tests ─────────────────────────────────────────

    #[test]
    fn ssrc_change_resets_and_keeps_pts_monotonic() {
        let mut d = depay();
        // First AU from SSRC 0xAABB (hdr() uses ssrc=0xAABB).
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au1 = d.next_au().unwrap();
        assert_eq!(au1.pts, Pts90khz::new(0));

        // SSRC change → source restart. Use a different SSRC (0xCCDD).
        let mut h2 = RtpHeader::new(1, 4003, 0xCCDD);
        h2.marker = true;
        h2.payload_type = 96;
        d.feed(&h2, &[0x41, 0x02]);
        let au2 = d.next_au().unwrap();
        // After SSRC reset the next AU re-anchors at last_emitted_pts + SSRC_RESET_PTS_STEP.
        // last_emitted_pts was 0, so new anchor = 0 + 3003 = 3003.
        assert_eq!(au2.pts, Pts90khz::new(3003));
    }

    #[test]
    fn f_bit_poisons_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // clean NALU
        d.feed(&hdr(2, 1000, true), &[0xE1, 0xFF]); // F=1, type 1 — corrupt
        assert!(d.next_au().is_none()); // AU dropped
        assert_eq!(d.stats().aus_dropped, 1);
        assert_eq!(d.stats().nalus_discarded, 1);
    }

    #[test]
    fn interleaved_types_poison_au() {
        for &nalu_type in &[25u8, 26, 27, 29] {
            let mut d = depay();
            d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // clean NALU
            let payload = [nalu_type, 0xFF];
            d.feed(&hdr(2, 1000, true), &payload);
            assert!(d.next_au().is_none(), "type {} should poison", nalu_type);
            assert_eq!(d.stats().aus_dropped, 1, "type {}", nalu_type);
        }
    }

    #[test]
    fn empty_payload_discarded() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[]); // empty
        d.feed(&hdr(2, 1000, true), &[0x41, 0x01]);
        // AU should succeed — empty packet only ticks packets_discarded.
        let au = d.next_au().expect("non-empty AU should complete");
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
    }

    // ── STAP-A tests ──────────────────────────────────────────────────────────

    #[test]
    fn stap_a_unpacks_units_in_order() {
        // STAP-A header (24, NRI=3 → 0x78) + [size=2][SPS 0x67,0x42] + [size=3][PPS 0x68,0xCE,0x38]
        let stap = [0x78, 0, 2, 0x67, 0x42, 0, 3, 0x68, 0xCE, 0x38];
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        let au = d.next_au().unwrap();
        assert_eq!(
            au.annexb,
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0x38]
        );
    }

    #[test]
    fn stap_a_zero_size_unit_poisons() {
        let stap = [0x78, 0, 0, 0x67]; // size=0 is degenerate (size includes the NALU header byte)
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    #[test]
    fn stap_a_truncated_unit_poisons() {
        let stap = [0x78, 0, 5, 0x67, 0x42]; // claims 5 bytes, only 2 present
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().aus_dropped, 1);
    }

    // ── FU-A tests ────────────────────────────────────────────────────────────

    /// FU-A packet: indicator (F/NRI + type 28), header (S/E + orig type), fragment bytes.
    fn fua(nri: u8, s: bool, e: bool, typ: u8, frag: &[u8]) -> Vec<u8> {
        let ind = (nri << 5) | 28;
        let fh = (u8::from(s) << 7) | (u8::from(e) << 6) | (typ & 0x1F);
        let mut v = vec![ind, fh];
        v.extend_from_slice(frag);
        v
    }

    #[test]
    fn fu_a_reassembles_across_three_packets() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &fua(3, true, false, 5, &[0xAA]));
        d.feed(&hdr(2, 1000, false), &fua(3, false, false, 5, &[0xBB]));
        d.feed(&hdr(3, 1000, true), &fua(3, false, true, 5, &[0xCC]));
        let au = d.next_au().unwrap();
        // Reconstructed header: (ind & 0xE0) | (fh & 0x1F) = 0x60 | 5 = 0x65
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA, 0xBB, 0xCC]);
        assert!(au.key_frame);
    }

    #[test]
    fn fu_middle_fragment_loss_drops_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &fua(3, true, false, 5, &[0xAA]));
        d.feed(&hdr(3, 1000, true), &fua(3, false, true, 5, &[0xCC])); // seq 2 lost
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().seq_gaps, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    #[test]
    fn fu_start_loss_discards_tail_fragments() {
        let mut d = depay();
        // First packet ever is a mid-NALU fragment: no gap detectable, but no open FU either.
        d.feed(&hdr(9, 1000, false), &fua(3, false, false, 5, &[0xBB]));
        d.feed(&hdr(10, 1000, true), &fua(3, false, true, 5, &[0xCC]));
        assert!(d.next_au().is_none()); // AU poisoned — its head is missing (§7.3 discard guidance)
        assert!(d.stats().nalus_discarded >= 1);
    }

    #[test]
    fn fu_s_and_e_both_set_is_malformed() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &fua(3, true, true, 5, &[0xAA])); // §5.8 MUST NOT
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().packets_discarded, 1);
    }

    #[test]
    fn fu_empty_payload_is_legal() {
        let mut d = depay();
        // NRI=1, type=1: ind=0x3C → reconstructed header = (0x3C & 0xE0) | 1 = 0x21
        d.feed(&hdr(1, 1000, false), &fua(1, true, false, 1, &[0xAA]));
        d.feed(&hdr(2, 1000, false), &fua(1, false, false, 1, &[])); // empty fragment, §5.8 note
        d.feed(&hdr(3, 1000, true), &fua(1, false, true, 1, &[0xBB]));
        let au = d.next_au().expect("empty FU fragment is legal");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x21, 0xAA, 0xBB]);
    }

    #[test]
    fn gap_after_marker_terminated_au_kills_only_next_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]); // AU-1 complete (marker)
        d.feed(&hdr(5, 4003, true), &[0x41, 0x02]); // gap; AU-2 head may be lost → poisoned
        d.feed(&hdr(6, 7006, true), &[0x41, 0x03]); // AU-3 clean
        let a = d.next_au().unwrap();
        assert_eq!(a.annexb[4], 0x41);
        assert_eq!(a.annexb[5], 0x01);
        let c = d.next_au().unwrap();
        assert_eq!(c.annexb[5], 0x03); // AU-2 was dropped
        assert_eq!(d.stats().aus_dropped, 1);
        assert_eq!(c.pts, Pts90khz::new(6006)); // PTS still tracks timestamps, not emission count
    }

    #[test]
    fn markerless_boundary_gap_kills_both_aus() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // AU-1, no marker
        d.feed(&hdr(4, 4003, false), &[0x41, 0x02]); // gap spanning the boundary
        d.feed(&hdr(5, 7006, true), &[0x41, 0x03]); // AU-3
        let only = d.next_au().unwrap();
        assert_eq!(only.annexb[5], 0x03);
        assert_eq!(d.stats().aus_dropped, 2);
    }

    #[test]
    fn ssrc_change_resets_and_keeps_pts_monotonic_fua() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let mut h2 = hdr(1, 900_000, true); // new source, unrelated ts base
        h2.ssrc = 0xDEAD;
        d.feed(&h2, &[0x41, 0x02]);
        let a = d.next_au().unwrap();
        let b = d.next_au().unwrap();
        assert_eq!(a.pts, Pts90khz::new(0));
        assert_eq!(b.pts, Pts90khz::new(3003)); // last_emitted + SSRC_RESET_PTS_STEP
        assert_eq!(d.stats().ssrc_changes, 1);
    }

    #[test]
    fn f_bit_nalu_discarded_and_poisons() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x80 | 0x41, 0x01]); // F=1
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().nalus_discarded, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    // ── Parameter-set cache + BeforeIdr injection tests ──────────────────────

    fn sps() -> Vec<u8> {
        vec![0x67, 0x42, 0x00, 0x1E]
    }
    fn pps() -> Vec<u8> {
        vec![0x68, 0xCE, 0x38, 0x80]
    }
    fn depay_inject(sprop: Vec<Vec<u8>>) -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            initial_parameter_sets: sprop,
            ..H264DepayConfig::default() // BeforeIdr is the default
        })
    }

    #[test]
    fn sprop_injected_before_idr_when_absent_inband() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // bare IDR
        let au = d.next_au().unwrap();
        let mut expect = vec![0, 0, 0, 1];
        expect.extend(sps());
        expect.extend([0, 0, 0, 1]);
        expect.extend(pps());
        expect.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expect);
    }

    #[test]
    fn injection_idempotent_when_au_already_carries_ps() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, false), &sps());
        d.feed(&hdr(2, 1000, false), &pps());
        d.feed(&hdr(3, 1000, true), &[0x65, 0xAA]);
        let au = d.next_au().unwrap();
        // Byte-identical pass-through: the AU's own SPS + PPS + IDR, exactly
        // as fed — nothing prepended.
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(sps());
        expected.extend([0, 0, 0, 1]);
        expected.extend(pps());
        expected.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expected);
        // Redundant with the byte-equality above, but states the intent:
        // exactly 3 NALUs, no duplicates prepended.
        assert_eq!(
            au.annexb.windows(4).filter(|w| *w == [0, 0, 0, 1]).count(),
            3
        );
    }

    #[test]
    fn non_idr_aus_never_get_injection() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
    }

    #[test]
    fn inband_ps_updates_cache_and_counter() {
        let mut d = depay_inject(vec![sps(), pps()]);
        let new_sps = vec![0x67, 0x42, 0x00, 0x28]; // different SPS arrives in-band
        d.feed(&hdr(1, 1000, true), &new_sps);
        d.feed(&hdr(2, 4003, true), &[0x65, 0xAA]); // bare IDR in the NEXT AU
        d.next_au().unwrap();
        let au = d.next_au().unwrap();
        // Second AU gets the UPDATED SPS (in-band replaced the seed) followed
        // by the SEEDED PPS (never seen in-band, still served from the cache).
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(&new_sps);
        expected.extend([0, 0, 0, 1]);
        expected.extend(pps());
        expected.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expected);
        assert_eq!(d.stats().parameter_set_updates, 1); // sprop→in-band change counted once
    }

    #[test]
    fn injection_none_is_byte_faithful() {
        let mut d = H264Depacketizer::new(H264DepayConfig {
            parameter_set_injection: ParameterSetInjection::None,
            initial_parameter_sets: vec![sps(), pps()],
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]);
        assert_eq!(d.next_au().unwrap().annexb, [0, 0, 0, 1, 0x65, 0xAA]);
    }

    #[test]
    fn seq_gap_poisons_current_and_next_au() {
        // A gap poisons the AU it lands on.  When that AU is closed by a
        // marker the boundary is established: the NEXT AU starts clean.
        // gap_pending carries through markerless boundaries only.
        let mut d = depay();
        // AU1: clean, completed by marker.
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au1 = d.next_au().expect("AU1 clean");
        assert_eq!(au1.pts, Pts90khz::new(0));

        // AU2: seq 2, marker-closed.
        d.feed(&hdr(2, 2000, true), &[0x41, 0x02]);
        let au2 = d.next_au().expect("AU2 clean");
        assert!(!au2.key_frame);

        // Gap: seq 5 (skipped 3,4) on AU3. AU3 is poisoned by the gap AND
        // closed by its own marker — boundary is known after this point.
        d.feed(&hdr(5, 3000, true), &[0x41, 0x03]);
        assert!(d.next_au().is_none(), "AU3 should be poisoned by gap");
        assert_eq!(d.stats().seq_gaps, 1);
        assert_eq!(d.stats().aus_dropped, 1);

        // AU4: seq 6, right after the marker-closed gap AU — boundary was
        // established by the marker so AU4 starts clean.
        d.feed(&hdr(6, 4000, true), &[0x41, 0x04]);
        assert!(d.next_au().is_some(), "AU4 clean: marker resolved boundary");
        assert_eq!(d.stats().aus_dropped, 1); // still 1 — AU4 is clean

        // AU5: seq 7, clean.
        d.feed(&hdr(7, 5000, true), &[0x41, 0x05]);
        assert!(d.next_au().is_some(), "AU5 should be clean");
    }

    // ── max_au_bytes cap tests ────────────────────────────────────────────────

    /// Build an `H264Depacketizer` with a tiny cap for oversize testing.
    fn depay_capped(max_bytes: usize) -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: max_bytes,
            parameter_set_injection: ParameterSetInjection::None,
            ..H264DepayConfig::default()
        })
    }

    /// FU-A fragments totalling > cap must be dropped with memory cleared and
    /// `aus_dropped_oversize` ticked. A subsequent clean AU must still work
    /// (depacketizer recovers cleanly).
    #[test]
    fn oversize_fu_a_au_is_dropped_and_stats_ticked() {
        const CAP: usize = 32; // tiny cap for the test
        let mut d = depay_capped(CAP);

        // Feed FU-A packets for one AU with a constant timestamp.
        // Each fragment is 16 bytes; after 2 packets au_buf+fu exceeds CAP=32.
        // FU-A indicator: NRI=3→0x60, type 28→0x1C → ind=0x7C
        // Fragment type 1 (non-IDR): fh start = 0x80|1=0x81; mid = 0x01; end = 0x40|1=0x41
        let ind: u8 = (3 << 5) | 28; // NRI=3, type=FU-A
        let frag = vec![0xAAu8; 16];

        // Start fragment
        let mut start = vec![ind, 0x80 | 1]; // S=1, E=0, orig type=1
        start.extend_from_slice(&frag);
        d.feed(&hdr(1, 1000, false), &start);

        // Continuation fragment — pushes over CAP
        let mut cont = vec![ind, 0x01]; // S=0, E=0, orig type=1
        cont.extend_from_slice(&frag);
        d.feed(&hdr(2, 1000, false), &cont);

        // End fragment (would close the FU-A, but cap was already hit)
        let mut end = vec![ind, 0x40 | 1]; // S=0, E=1, orig type=1
        end.extend_from_slice(&[0xBBu8; 4]);
        d.feed(&hdr(3, 1000, true), &end); // marker → complete_au → drop

        // The AU must be dropped, not emitted.
        assert!(d.next_au().is_none(), "oversize AU must be dropped");
        let stats = d.stats();
        assert_eq!(
            stats.aus_dropped_oversize, 1,
            "aus_dropped_oversize must tick"
        );
        assert_eq!(stats.aus_dropped, 1, "aus_dropped must also tick");

        // Verify memory is bounded: the internal buffers must have been cleared
        // (no retained bytes). We verify this indirectly by confirming a clean
        // subsequent AU succeeds — if the buffers were NOT cleared, the old
        // bytes would still be there and the cap would fire again.
        d.feed(&hdr(4, 4003, true), &[0x41, 0x01]); // clean non-IDR AU
        let clean = d.next_au().expect("subsequent clean AU must succeed");
        assert_eq!(clean.annexb, [0, 0, 0, 1, 0x41, 0x01]);
        assert_eq!(d.stats().aus_dropped_oversize, 1, "no new oversize drops");
    }

    /// Parameter-set injection must not bypass the cap. A bare IDR AU that
    /// fits under the cap on its own is pushed over once cached SPS/PPS are
    /// prepended in `complete_au()`; the injected AU must be dropped, never
    /// emitted above the advertised `max_au_bytes`.
    #[test]
    fn injection_cannot_bypass_max_au_bytes() {
        // Bare IDR au_buf = 4 (start code) + 2 = 6 bytes, under CAP=8.
        // Injection prepends (4+4) SPS + (4+4) PPS = 16 bytes → 22 > 8.
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![sps(), pps()], // BeforeIdr default
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // bare IDR, marker closes
        assert!(
            d.next_au().is_none(),
            "injected AU exceeding the cap must be dropped, not emitted"
        );
        assert_eq!(d.stats().aus_dropped_oversize, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    /// A build helper: an SPS (NAL type 7) whose Annex-B-framed length
    /// exceeds `cap` bytes.
    fn oversize_sps(cap: usize) -> Vec<u8> {
        let mut v = vec![0x67u8]; // NAL type 7 (SPS)
        v.resize(cap + 4, 0xAA); // 4 (start code) + (cap+4) len > cap
        v
    }

    /// An over-cap SPS arriving in-band must NOT be retained in the cache.
    /// If it were, every later IDR would rebuild an over-cap injection prefix
    /// and be dropped — `max_au_bytes` would fail to bound cached parameter-set
    /// memory. With the SPS uncached, a later tiny IDR emits clean.
    #[test]
    fn oversize_inband_sps_is_not_retained_in_cache() {
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            ..H264DepayConfig::default() // BeforeIdr injection is the default
        });
        d.feed(&hdr(1, 1000, true), &oversize_sps(CAP)); // dropped oversize
        assert!(d.next_au().is_none(), "over-cap SPS AU dropped");
        assert_eq!(d.stats().aus_dropped_oversize, 1);

        // Tiny IDR must not inherit the over-cap SPS from cache.
        d.feed(&hdr(2, 4003, true), &[0x65, 0xAA]); // 4 + 2 = 6 <= CAP
        let au = d
            .next_au()
            .expect("tiny IDR emits clean (no stale over-cap SPS in cache)");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(d.stats().aus_dropped_oversize, 1, "no second oversize drop");
    }

    /// Over-cap `initial_parameter_sets` (e.g. seeded from an SDP
    /// `sprop-parameter-sets`) must not be seeded into the cache either.
    #[test]
    fn oversize_initial_parameter_sets_are_not_seeded() {
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![oversize_sps(CAP)],
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // tiny IDR
        let au = d
            .next_au()
            .expect("tiny IDR emits clean; over-cap seed was skipped");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(d.stats().aus_dropped_oversize, 0);
    }

    /// SPS and PPS each fit the cap individually, but their combined injection
    /// plus a tiny IDR AU exceeds it. The AU must be dropped as oversize — the
    /// `complete_au` preflight computes the injected length and drops before
    /// allocating/copying the over-cap prefix.
    #[test]
    fn injection_preflights_combined_over_cap_and_drops() {
        const CAP: usize = 20;
        let sps6 = vec![0x67u8, 1, 2, 3, 4, 5]; // framed 4 + 6 = 10 <= CAP
        let pps6 = vec![0x68u8, 1, 2, 3, 4, 5]; // framed 4 + 6 = 10 <= CAP
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![sps6, pps6],
            ..H264DepayConfig::default()
        });
        // Injection 10 + 10 = 20, plus IDR au_buf 6 = 26 > CAP.
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]);
        assert!(
            d.next_au().is_none(),
            "combined over-cap injection AU dropped"
        );
        assert_eq!(d.stats().aus_dropped_oversize, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    /// Single-NALU AU that exceeds the cap: `aus_dropped_oversize` ticks and
    /// the AU is dropped. Verifies the single-NALU (non-FU-A) path.
    #[test]
    fn oversize_single_nalu_au_is_dropped() {
        const CAP: usize = 8; // only 8 bytes before oversize
        let mut d = depay_capped(CAP);

        // Push a clean small NALU to start filling au_buf (4+1 = 5 bytes).
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        assert_eq!(d.stats().aus_dropped_oversize, 0);

        // Push another NALU at the same timestamp — this will push au_buf over 8.
        // 5 (current) + 4 (start code) + 2 (nalu) = 11 bytes > 8 → oversize.
        d.feed(&hdr(2, 1000, true), &[0x41, 0x02]); // marker closes AU

        assert!(d.next_au().is_none(), "oversize AU must be dropped");
        let stats = d.stats();
        assert_eq!(stats.aus_dropped_oversize, 1);
        assert_eq!(stats.aus_dropped, 1);
    }
}
