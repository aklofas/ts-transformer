//! Deliberately naive raw MPEG-TS reader for the wire-level oracles.
//!
//! Independence is the point: `verify.rs`'s tally comes from tst-core's
//! demuxer, i.e. the code under test. This reader shares nothing with it —
//! it parses exactly the constructs this crate's generator emits (188-byte
//! packets, single-packet PAT/PMT, adaptation-field PCR, PES headers) and
//! returns an error for anything else, so it can never quietly degrade into
//! the demuxer's view of a stream. Spec references: ITU-T H.222.0 §2.4.3.2
//! (packet header), §2.4.3.5 (adaptation field / PCR), §2.4.4.3-.8 (PAT /
//! PMT), §2.4.3.7 (PES header / PTS).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

const PKT: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0;

/// CRC-32/MPEG-2 over `bytes` — ITU-T H.222.0 Annex A: generator
/// polynomial 0x04C11DB7, initial value 0xFFFFFFFF, MSB-first, with
/// neither input nor output reflection and no final XOR.
///
/// Written out here rather than borrowed from `tst_core::mpegts` on
/// purpose. This reader exists to check the demuxer (see the module doc),
/// and sharing its checksum would be sharing a way to be wrong: a bug in
/// one implementation would make both ends agree on a corrupt table. The
/// bit-at-a-time form is deliberate too — a PAT or PMT is a couple of
/// dozen bytes a few times a second, so a 1 KiB lookup table would buy
/// nothing and hide the polynomial.
fn crc32_mpeg2(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
    /// `format_identifier` of a registration_descriptor (tag 0x05), if any.
    pub registration: Option<[u8; 4]>,
    pub descriptor_tags: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub program_number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PesShape {
    pub stream_ids: BTreeSet<u8>,
    /// First 4 payload bytes after the PES header of the FIRST PES seen.
    pub first_payload_prefix: Option<[u8; 4]>,
    /// PES whose first TWO payload bytes differ from
    /// [`first_payload_prefix`](Self::first_payload_prefix)'s — every
    /// carriage this crate muxes keeps those constant across PES
    /// (Annex-B / binding `00 00`, raw-OBU temporal delimiter `12 00`,
    /// ADTS `FF Fx`, async KLV UL `06 0E`), so on the video and audio
    /// PIDs a nonzero count means a later PES does not look like the
    /// first. A sync-KLV PID's AU-cell header varies by design and
    /// nothing judges the count there.
    pub prefix_mismatches: u64,
}

/// Per-packet header facts, decoded statelessly — the classification the
/// corruption tap (`corrupt/tap.rs`) and the tee coordinate need without
/// owning a `Reader`. Mirrors exactly the header parsing `Reader::packet`
/// does (§2.4.3.2 / §2.4.3.5); kept as one function so the two can never
/// disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub pid: u16,
    pub pusi: bool,
    /// adaptation_field_control (2 bits).
    pub afc: u8,
    pub has_payload: bool,
    /// adaptation_field_length, 0 when there is no adaptation field.
    pub af_len: usize,
    /// Byte offset of the first payload byte (188 when there is none).
    pub payload_off: usize,
    pub pcr_base: Option<u64>,
    pub cc: u8,
}

/// Parses one 188-byte packet's fixed header and adaptation field,
/// entirely independent of any [`Reader`] state — `Err` on anything
/// malformed (bad sync byte, an adaptation field that overruns the
/// packet, or a PCR flag on a too-short adaptation field).
pub fn classify_packet(p: &[u8; PKT]) -> Result<PacketInfo, String> {
    if p[0] != SYNC {
        return Err(format!("sync byte 0x{:02x}, want 0x47", p[0]));
    }
    let pusi = p[1] & 0x40 != 0;
    let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
    let afc = (p[3] >> 4) & 0x3;
    let cc = p[3] & 0x0F;
    let mut af_len = 0;
    let mut pcr_base = None;
    let mut off = 4;
    if afc & 0x2 != 0 {
        af_len = usize::from(p[4]);
        if 5 + af_len > PKT {
            return Err(format!(
                "adaptation field length {af_len} does not fit in a {PKT}-byte packet"
            ));
        }
        if af_len > 0 && p[5] & 0x10 != 0 {
            // The PCR flag claims 6 bytes (program_clock_reference_
            // base + _extension, §2.4.3.5) follow the flags byte, so
            // the adaptation field must be at least 1 (flags) + 6
            // bytes long. A PCR flag on a shorter adaptation field is
            // malformed — reading `p[6..12]` anyway would silently
            // read bytes outside the declared field (padding, or the
            // next field entirely) as if they were PCR.
            if af_len < 7 {
                return Err(format!(
                    "adaptation field: PCR flag set but adaptation_field_length {af_len} < 7"
                ));
            }
            // program_clock_reference_base: 33 bits, §2.4.3.5.
            let b = &p[6..12];
            pcr_base = Some(
                (u64::from(b[0]) << 25)
                    | (u64::from(b[1]) << 17)
                    | (u64::from(b[2]) << 9)
                    | (u64::from(b[3]) << 1)
                    | u64::from(b[4] >> 7),
            );
        }
        off = 5 + af_len;
    }
    let has_payload = afc & 0x1 != 0 && off < PKT;
    Ok(PacketInfo {
        pid,
        pusi,
        afc,
        has_payload,
        af_len,
        payload_off: if has_payload { off } else { PKT },
        pcr_base,
        cc,
    })
}

/// Byte-compare two packets for the §2.4.3.3 duplicate rule: identical
/// everywhere, or identical everywhere except the 6-byte PCR field (bytes
/// 6..12, the layout `classify_packet` assumes) when BOTH carry a PCR — a
/// legal duplicate may refresh it. `a_has_pcr`/`b_has_pcr` come from the
/// caller's own `PacketInfo::pcr_base.is_some()` rather than re-deriving
/// them here, so this stays a pure byte mask. Deliberately reimplemented
/// rather than shared with tst-core's own `pcr_masked_identical`
/// (`demux/sync_ingress.rs`) — this module's independence from the code
/// under test is the point (see the module doc).
fn pcr_masked_identical(a: &[u8; PKT], b: &[u8; PKT], a_has_pcr: bool, b_has_pcr: bool) -> bool {
    if a == b {
        return true;
    }
    a_has_pcr && b_has_pcr && a[..6] == b[..6] && a[12..] == b[12..]
}

/// One sync-recovery event recorded by a [`Reader`] in resync mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resync {
    /// `Reader::packets()` at the moment the hunt started.
    pub at_packets: u64,
    pub skipped_bytes: usize,
}

/// The 33-bit modulus both PCR bases and PES PTS values count on (ITU-T
/// H.222.0 V9 §2.4.3.5). Spelled out here rather than imported from
/// `oracles.rs` for the same reason this module carries its own CRC: an
/// independent reader that borrowed a constant from the code it checks
/// would be sharing a way to be wrong.
const TS_MODULUS: u64 = 1 << 33;

/// How much of a per-PID timestamp series a [`Reader`] keeps.
///
/// A capture's derived statistics (interval min/median/max, step median,
/// wrap count) need consecutive DELTAS, not the timestamps themselves,
/// and every one of them except the median is exactly computable from a
/// running counter. Only the median needs a sample of the deltas — and
/// how big that sample is allowed to get is the difference between the
/// two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    /// Keep every delta, so every statistic including the median is
    /// exact. The offline path ([`summarize_file`], `verify`'s own
    /// reader) already holds the whole capture in memory, so bounding it
    /// would buy nothing and would make an offline verdict depend on a
    /// sample.
    #[default]
    Full,
    /// Keep a bounded uniform sample of the deltas
    /// ([`SAMPLE_CAP`] per PID). A live receiver runs for days: at soak
    /// rates the unbounded form grew this reader's state by ~1.5-3 MiB
    /// per hour per process, which on its own would fail the 72-hour
    /// run's own 200 KiB/h `rss_slope_*` gate.
    ///
    /// Only the MEDIAN accessors become estimates; counts, wrap counts
    /// and the forward-interval min/max stay exact, so
    /// `VerifyMode::Strict`'s "every interval within bounds" rule (which
    /// reads the max) is unaffected by the bound.
    Bounded,
}

/// Deltas a [`Retention::Bounded`] series keeps per PID. 4096 `i64`s is
/// 32 KiB per series — constant for the life of the process, against a
/// sample large enough that a median drawn from it lands within a
/// fraction of a percent of the exact one at any capture length.
pub const SAMPLE_CAP: usize = 4096;

/// Consecutive-delta statistics for one PID's timestamp series (PCR
/// bases, or PES PTS values), accumulated in constant memory.
///
/// Fed one raw 33-bit timestamp at a time, in arrival order. Every
/// statistic the wire oracles ask for is derived from the delta between
/// consecutive values, in one of two readings:
///
/// - the WRAP-AWARE FORWARD distance (`delta mod 2^33`), which is what a
///   PCR interval is — a clock that wrapped did not go backwards;
/// - the RAW SIGNED delta, whose sign is the whole signal for a PTS
///   rollover and whose positive values are the frame-to-frame step an
///   audio cadence check measures.
///
/// Both are recoverable from the signed delta, so one sample serves both
/// and a [`Retention::Bounded`] series costs one [`SAMPLE_CAP`] buffer.
#[derive(Debug, Clone)]
pub struct TimestampSeries {
    count: u64,
    last: Option<u64>,
    /// Reservoir of raw signed deltas — every delta under
    /// [`Retention::Full`], a uniform sample of them under
    /// [`Retention::Bounded`].
    sample: Vec<i64>,
    /// Deltas OFFERED, which is `count - 1` and may exceed
    /// `sample.len()`.
    deltas: u64,
    fwd_min: u64,
    fwd_max: u64,
    /// Consecutive pairs that went backwards by more than half the
    /// modulus: a raw timestamp rollover, counted exactly in both
    /// retention modes.
    wraps: u64,
    retention: Retention,
    /// Reservoir-sampling state. Seeded per PID from a fixed constant so
    /// a bounded run is as reproducible as an unbounded one.
    rng: u64,
}

impl TimestampSeries {
    fn new(retention: Retention, pid: u16) -> Self {
        TimestampSeries {
            count: 0,
            last: None,
            sample: Vec::new(),
            deltas: 0,
            fwd_min: u64::MAX,
            fwd_max: 0,
            wraps: 0,
            retention,
            // Golden-ratio odd constant, salted by the PID: distinct
            // per-PID streams, identical from run to run.
            rng: 0x9E37_79B9_7F4A_7C15 ^ u64::from(pid).wrapping_mul(0x0100_0000_01B3),
        }
    }

    /// Build a series directly from a list of timestamps, bypassing a
    /// [`Reader`] — how the oracle unit tests state a PCR or PTS series
    /// they want checked.
    pub fn from_values(retention: Retention, pid: u16, values: &[u64]) -> Self {
        let mut s = TimestampSeries::new(retention, pid);
        for &v in values {
            s.push(v);
        }
        s
    }

    /// Record one timestamp. Values are 33-bit by construction (both
    /// carriers mask to 33 bits before they reach here).
    fn push(&mut self, value: u64) {
        self.count += 1;
        let Some(prev) = self.last.replace(value) else {
            return;
        };
        let delta = value as i64 - prev as i64;
        self.deltas += 1;
        let fwd = delta.rem_euclid(TS_MODULUS as i64) as u64;
        self.fwd_min = self.fwd_min.min(fwd);
        self.fwd_max = self.fwd_max.max(fwd);
        if delta < 0 && delta.unsigned_abs() > TS_MODULUS / 2 {
            self.wraps += 1;
        }
        self.retain(delta);
    }

    /// Vitter's Algorithm R: the first [`SAMPLE_CAP`] deltas are kept
    /// outright, and the k-th one after that replaces a uniformly chosen
    /// existing entry with probability `cap/k` — which leaves the buffer
    /// a uniform sample of everything offered, not merely of the most
    /// recent stretch. That distinction is the point on a multi-day run:
    /// a sliding window would report the median of the last few minutes
    /// and call it the median of the capture.
    fn retain(&mut self, delta: i64) {
        if self.retention == Retention::Full || self.sample.len() < SAMPLE_CAP {
            self.sample.push(delta);
            return;
        }
        // xorshift64*, deterministic and adequate for a reservoir.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let j = (self.rng % self.deltas) as usize;
        if j < SAMPLE_CAP {
            self.sample[j] = delta;
        }
    }

    /// Timestamps recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Consecutive deltas measured — `count - 1`, or 0 for an empty
    /// series.
    #[must_use]
    pub fn intervals(&self) -> u64 {
        self.deltas
    }

    /// Consecutive pairs that rolled the 33-bit counter over.
    #[must_use]
    pub fn wraps(&self) -> u64 {
        self.wraps
    }

    /// Smallest wrap-aware forward delta, exact in both retention modes.
    #[must_use]
    pub fn forward_min(&self) -> Option<u64> {
        (self.deltas > 0).then_some(self.fwd_min)
    }

    /// Largest wrap-aware forward delta, exact in both retention modes —
    /// so a `Strict` "every interval is within bounds" check is exact
    /// even on a bounded series.
    #[must_use]
    pub fn forward_max(&self) -> Option<u64> {
        (self.deltas > 0).then_some(self.fwd_max)
    }

    /// Median wrap-aware forward delta. Exact under
    /// [`Retention::Full`]; a sample median under [`Retention::Bounded`].
    #[must_use]
    pub fn forward_median(&self) -> Option<f64> {
        Self::median(self.sample.iter().map(|&d| {
            let fwd = d.rem_euclid(TS_MODULUS as i64) as u64;
            fwd as f64
        }))
    }

    /// Median of the POSITIVE raw deltas — the frame-to-frame step of a
    /// series that does not wrap within the capture. `None` when no
    /// positive delta was sampled.
    #[must_use]
    pub fn positive_step_median(&self) -> Option<f64> {
        Self::median(self.sample.iter().filter(|&&d| d > 0).map(|&d| d as f64))
    }

    /// Upper-middle element of an even-length sample, deliberately not
    /// an average of the middle two: an average can land outside the
    /// range of any single observation, which is exactly what a check
    /// gating on "the typical interval" must not do.
    fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
        let mut v: Vec<f64> = values.collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).expect("finite deltas"));
        Some(v[v.len() / 2])
    }

    /// Deltas currently retained — the reservoir's occupancy, never more
    /// than [`SAMPLE_CAP`] under [`Retention::Bounded`]. Exposed so a
    /// test can assert the bound holds.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.sample.len()
    }
}

#[derive(Debug, Clone, Default)]
pub struct WireSummary {
    pub programs: BTreeMap<u16, Program>,
    /// Per-PID PCR-base series. See [`TimestampSeries`] — the raw bases
    /// are not kept, only the statistics the oracles ask for.
    pub pcr: BTreeMap<u16, TimestampSeries>,
    /// Per-PID PES PTS series, same treatment as [`pcr`](Self::pcr).
    pub pts: BTreeMap<u16, TimestampSeries>,
    pub pes: BTreeMap<u16, PesShape>,
    pub packets_per_pid: BTreeMap<u16, u64>,
    /// PES starts per PID — packets with `payload_unit_start_indicator`
    /// whose payload opens with the `00 00 01` PES start code. Counted
    /// for every PID, with or without a PTS (an async KLV PES may carry
    /// none, and `pts` only holds timestamped ones). A packet that
    /// repeats the previous packet's continuity counter on its PID AND is
    /// otherwise byte-identical (PCR field exempted) is a spec-legal
    /// duplicate (§2.4.3.3) and is not counted twice — see
    /// `pcr_masked_identical`; the demuxer suppresses it too
    /// (`demux/sync_ingress.rs`), and the corruption tap's `Dup` class
    /// emits exactly that. A same-CC packet whose other bytes differ is
    /// NOT a duplicate and counts as its own PES start.
    pub pes_starts_per_pid: BTreeMap<u16, u64>,
    pub packets: u64,
    /// PSI sections discarded because their CRC-32 did not check out —
    /// see `Reader::section` (private). Zero on any capture this harness
    /// generates; non-zero means something damaged a PAT or PMT in
    /// flight, which is exactly what the corruption tap does on purpose.
    pub psi_crc_rejected: u64,
}

pub struct Reader {
    summary: WireSummary,
    /// PMT PID -> program_number, learned from the PAT.
    pmt_pids: BTreeMap<u16, u16>,
    carry: Vec<u8>,
    resync_mode: bool,
    resyncs: Vec<Resync>,
    /// Lifetime number of sync recoveries, across every
    /// [`Reader::take_resyncs`] drain.
    resync_count: u64,
    /// Garbage bytes already dropped from the carry since the last
    /// accepted packet — see the no-candidate arm of [`Reader::feed`].
    /// Folded into the next [`Resync`].
    pending_skipped: usize,
    /// Applied to every [`TimestampSeries`] this reader creates.
    retention: Retention,
    /// `(pcr_base, packet ordinal of the packet that carried it)` for
    /// every PCR decoded since the last [`Reader::take_pcr_events`] —
    /// see that method's doc comment for why the ordinal is kept here
    /// rather than re-derived by a caller watching the latest base.
    pcr_events: Vec<(u64, u64)>,
    /// Last payload-carrying packet seen per PID, kept whole (not just its
    /// CC) so a duplicate can be told from a same-CC packet whose other
    /// bytes differ — see [`WireSummary::pes_starts_per_pid`] and
    /// [`pcr_masked_identical`].
    last_pkt: BTreeMap<u16, [u8; PKT]>,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    /// A reader that keeps every timestamp delta
    /// ([`Retention::Full`]) — the offline default.
    pub fn new() -> Self {
        Self::with_retention(Retention::Full)
    }

    /// A reader whose per-PID timestamp series follow `retention`. A
    /// live receive loop passes [`Retention::Bounded`]: it is fed every
    /// byte of a multi-day capture and must not grow with it.
    pub fn with_retention(retention: Retention) -> Self {
        Self {
            summary: WireSummary::default(),
            pmt_pids: BTreeMap::new(),
            carry: Vec::new(),
            resync_mode: false,
            resyncs: Vec::new(),
            resync_count: 0,
            pending_skipped: 0,
            retention,
            pcr_events: Vec::new(),
            last_pkt: BTreeMap::new(),
        }
    }

    /// Bytes buffered toward the next packet (a partial packet, or
    /// unconfirmed bytes in resync mode).
    pub fn carry(&self) -> &[u8] {
        &self.carry
    }

    /// Packets accepted so far.
    pub fn packets(&self) -> u64 {
        self.summary.packets
    }

    /// Take every `(pcr_base, at)` decoded since the previous call, where
    /// `at` is the 0-based ordinal of the packet that CARRIED the base
    /// (i.e. [`Reader::packets`] as it read immediately before that
    /// packet was counted).
    ///
    /// A caller could almost derive this by watching the most recent PCR
    /// base and noticing when it changes — but only to the granularity of
    /// whatever it feeds, and a live receiver feeds whole transport reads
    /// (up to 7 packets each), so the ordinal it would attach could be
    /// off by up to that many packets. `crate::corrupt::Attribution`
    /// converts these pairs into receiver positions for logged corruption
    /// coordinates, and an off-by-a-chunk anchor there silently shifts
    /// every attribution window, so the exact ordinal is recorded at the
    /// moment the PCR is decoded instead.
    pub fn take_pcr_events(&mut self) -> Vec<(u64, u64)> {
        std::mem::take(&mut self.pcr_events)
    }

    /// Whether `pid` is a PMT PID — learned from the PAT.
    pub fn is_pmt_pid(&self, pid: u16) -> bool {
        self.pmt_pids.contains_key(&pid)
    }

    /// Turns resync (sync-recovery) mode on or off. When on, `feed` hunts
    /// past corrupted or truncated packets instead of failing the whole
    /// feed, recording each recovery as a [`Resync`] (see
    /// [`Reader::take_resyncs`]).
    pub fn set_resync_mode(&mut self, on: bool) {
        self.resync_mode = on;
    }

    /// Take every sync-recovery recorded since the previous call —
    /// populated only in resync mode. Drained, not peeked, for the same
    /// reason as [`Reader::take_pcr_events`]: a live receiver polls this
    /// for days and must not retain (or re-copy) the whole history.
    /// Does not include a trailing partial packet still sitting in the
    /// carry; see [`Reader::trailing_resync`].
    pub fn take_resyncs(&mut self) -> Vec<Resync> {
        std::mem::take(&mut self.resyncs)
    }

    /// Lifetime number of sync recoveries, across every drain.
    pub fn resync_count(&self) -> u64 {
        self.resync_count
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.carry.extend_from_slice(bytes);
        let mut off = 0;
        while off + PKT <= self.carry.len() {
            if self.resync_mode {
                let ahead_ok = off + PKT >= self.carry.len() || self.carry[off + PKT] == SYNC;
                if self.carry[off] != SYNC || !ahead_ok {
                    // `off` is either garbage outright, or a sync byte whose
                    // claimed 188-byte span runs into a byte that is not a
                    // sync byte — typically because an earlier packet lost
                    // bytes (truncation), so every packet after it now
                    // "starts" 188 bytes too early. Either way we cannot
                    // trust reading a whole packet from `off` without first
                    // checking whether a genuinely confirmed packet start
                    // sits CLOSER than `off + PKT`: if one does, `off` was
                    // itself bogus (a splice of the truncated packet's real
                    // header with the next real packet's leading bytes) and
                    // must be abandoned too, not merely skipped past.
                    let start = off;
                    let mut k = off + 1;
                    let mut found = None;
                    while k + PKT <= self.carry.len() {
                        if self.carry[k] == SYNC
                            && (k + PKT >= self.carry.len() || self.carry[k + PKT] == SYNC)
                        {
                            found = Some(k);
                            break;
                        }
                        k += 1;
                    }
                    match found {
                        // `off` itself was never trustworthy (not even a
                        // sync byte), or the nearest confirmed packet start
                        // sits inside `off`'s own claimed 188-byte span:
                        // abandon `off` and resync onto it.
                        Some(k) if self.carry[off] != SYNC || k < off + PKT => {
                            self.resyncs.push(Resync {
                                at_packets: self.summary.packets,
                                skipped_bytes: self.pending_skipped + (k - start),
                            });
                            self.resync_count += 1;
                            self.pending_skipped = 0;
                            off = k;
                            continue;
                        }
                        // `off` is a sync byte and nothing closer contradicts
                        // it — any corruption starts cleanly at `off + PKT`
                        // (e.g. inserted garbage) and the next iteration's
                        // own check finds it there.
                        Some(_) => {}
                        None => {
                            if self.carry[off] != SYNC {
                                // Confirmed garbage up to the last 187
                                // bytes — a candidate 0x47 in that suffix
                                // cannot be confirmed until a whole
                                // packet follows it, so only the suffix
                                // may wait for the next feed. Everything
                                // before it is dropped NOW and counted,
                                // never retained and rescanned: a long
                                // malformed stream would otherwise grow
                                // the carry without bound (and made the
                                // harness's RSS look like a library leak).
                                let keep_from = self.carry.len().saturating_sub(PKT - 1).max(off);
                                self.pending_skipped += keep_from - off;
                                off = keep_from;
                                break;
                            }
                            // Not enough buffered data yet to confirm or
                            // refute `off` — accept it optimistically,
                            // matching the hunt's own "or is the last whole
                            // packet in the carry" leniency.
                        }
                    }
                }
                if self.pending_skipped > 0 {
                    // `off` is about to be read as a packet while garbage
                    // dropped on an earlier feed is still uncounted: the
                    // retained ≤ 187-byte suffix began at a candidate sync
                    // byte that a later feed then confirmed, so the hunt
                    // above never ran and never recorded the recovery.
                    // Record it here — those bytes cost sync just as much
                    // as the ones the hunt walks past, and leaving the
                    // count pending would bolt it onto an unrelated later
                    // recovery instead.
                    self.resyncs.push(Resync {
                        at_packets: self.summary.packets,
                        skipped_bytes: self.pending_skipped,
                    });
                    self.resync_count += 1;
                    self.pending_skipped = 0;
                }
            }
            let pkt: [u8; PKT] = self.carry[off..off + PKT].try_into().expect("PKT bytes");
            let before = self.summary.packets;
            let pcr_events_before = self.pcr_events.len();
            if let Err(e) = self.packet(&pkt) {
                if self.resync_mode {
                    // `off` passed the hunt-mode header check above (its
                    // sync byte and next-packet lookahead both looked
                    // fine) but the packet's own internals are malformed —
                    // e.g. an adaptation_field_length that overruns the
                    // packet, or a PSI section error. `Reader::packet` may
                    // have partially mutated state before hitting the
                    // error (a PSI error happens after `packets` is
                    // already incremented), so roll the counter back to
                    // `before`: this one packet is skipped, not counted,
                    // and every OTHER packet's count still needs to be
                    // right for downstream corruption verdicts to attribute
                    // the event correctly.
                    self.summary.packets = before;
                    // Same rollback for a PCR this skipped packet may
                    // already have recorded (the PCR is decoded before
                    // the PSI parse that failed): its ordinal named a
                    // packet that is no longer counted, and the next
                    // accepted packet is about to claim that ordinal
                    // itself.
                    self.pcr_events.truncate(pcr_events_before);
                    // The rollback stops there, ON PURPOSE. Exactly the
                    // two pieces of state whose MEANING is ordinal-based
                    // are undone; everything else `packet()` recorded
                    // before it failed — `summary.pcr`, `summary.pts`,
                    // `summary.packets_per_pid`, the PES shape's
                    // `stream_ids` — is a reading of what was genuinely
                    // on the wire and stays.
                    //
                    // It has to stay. Those series feed the wire oracles:
                    // `oracles::pcr_interval` reads `summary.pcr` and
                    // bounds the MAX interval in `Strict` mode, so
                    // discarding one real, correctly-decoded PCR base
                    // would double one interval and manufacture an oracle
                    // failure on precisely the corrupted captures this
                    // mode exists to judge. A packet whose adaptation
                    // field parsed cleanly and whose PES payload did not
                    // still carried that clock reading.
                    //
                    // The asymmetry is therefore not an oversight: an
                    // ordinal that no longer names a counted packet is
                    // unusable, a clock sample from a skipped packet is
                    // not.
                    self.resyncs.push(Resync {
                        at_packets: before,
                        skipped_bytes: PKT,
                    });
                    self.resync_count += 1;
                } else {
                    return Err(format!("packet {}: {e}", self.summary.packets));
                }
            }
            off += PKT;
        }
        self.carry.drain(..off);
        Ok(())
    }

    /// Return the accumulated [`WireSummary`] — `Err` iff a trailing
    /// partial packet (fewer than 188 bytes) is still sitting in the
    /// carry, naming how many bytes short it is. A well-formed capture
    /// is always a whole number of TS packets; a trailing fragment means
    /// the capture was cut off mid-packet (e.g. a live recv session
    /// closing between transport reads), and callers surface that as an
    /// explicit failure rather than silently discarding it. In resync
    /// mode, that same trailing fragment is tolerated instead of failing
    /// the feed — the whole point of resync mode is to keep going
    /// through corruption/truncation rather than fail. This consumes the
    /// reader, so call [`Reader::trailing_resync`] first if you need that
    /// final recovery event's fields.
    pub fn finish(self) -> Result<WireSummary, String> {
        if !self.carry.is_empty() && !self.resync_mode {
            return Err(format!(
                "{} trailing byte(s) short of a {PKT}-byte packet",
                self.carry.len()
            ));
        }
        Ok(self.summary)
    }

    /// Preview of the [`Resync`] that `finish` will silently tolerate for
    /// a trailing partial packet still sitting in the carry — `None`
    /// unless resync mode is on AND a trailing fragment is present.
    /// `finish` consumes the reader, so this is the only way to inspect
    /// that final recovery event's fields; call it before `finish`, not
    /// after. Not recorded in [`Reader::take_resyncs`] (which only holds
    /// recoveries `feed` made while it still owned the reader).
    pub fn trailing_resync(&self) -> Option<Resync> {
        if self.carry.is_empty() || !self.resync_mode {
            return None;
        }
        Some(Resync {
            at_packets: self.summary.packets,
            skipped_bytes: self.pending_skipped + self.carry.len(),
        })
    }

    /// Clear the in-flight byte carry (a partial trailing packet left
    /// over from whatever connection just broke) without discarding the
    /// accumulated [`WireSummary`] or the PAT-learned PMT-PID map. A
    /// managed reconnect's replacement transport starts delivering bytes
    /// at a fresh packet boundary of its own — bytes still sitting in
    /// `carry` from the connection that just died can never be validly
    /// completed by bytes from the new one (they aren't even
    /// guaranteed to be the same TS multiplex position), so feeding them
    /// together would corrupt packet sync rather than merely delay it.
    /// Call this once per reconnect, before the replacement transport's
    /// first `feed`.
    pub fn resync(&mut self) {
        self.carry.clear();
        self.pending_skipped = 0;
    }

    fn packet(&mut self, p: &[u8; PKT]) -> Result<(), String> {
        let info = classify_packet(p)?;
        self.summary.packets += 1;
        *self.summary.packets_per_pid.entry(info.pid).or_insert(0) += 1;
        if let Some(base) = info.pcr_base {
            let retention = self.retention;
            self.summary
                .pcr
                .entry(info.pid)
                .or_insert_with(|| TimestampSeries::new(retention, info.pid))
                .push(base);
            // An attribution anchor only from a PID the PMT declares as a
            // PCR_PID (before the first PMT — a mid-stream attach — any PID
            // is accepted). After a framing injection the hunt can accept a
            // payload 0x47 whose bytes look like an adaptation field with
            // the PCR flag set; that "PCR" is a random number, and as an
            // anchor it stranded every injection pending behind it.
            let declared = self.summary.programs.is_empty()
                || self
                    .summary
                    .programs
                    .values()
                    .any(|p| p.pcr_pid == info.pid);
            if declared {
                // `packets` was incremented just above, so this packet's own
                // 0-based ordinal is one less — see `take_pcr_events`.
                self.pcr_events.push((base, self.summary.packets - 1));
            }
        }
        if !info.has_payload {
            return Ok(());
        }
        let payload = &p[info.payload_off..];
        // `has_payload` is true here (the AF-only return above), so
        // §2.4.3.3 says this packet's CC advanced — unless it repeats the
        // previous one AND is otherwise byte-identical (PCR field
        // exempted: a legal duplicate may refresh it), which makes it a
        // spec-legal duplicate. A same-CC packet whose other bytes differ
        // is NOT a duplicate (non-conformant input, e.g. an encoder that
        // forgot to advance the counter) and must still count as its own
        // PES start. Tracked on every payload PID (PSI included) so a
        // later media packet on a PID that was first seen as PSI is
        // judged against a real value.
        let duplicate = self.last_pkt.get(&info.pid).is_some_and(|prev| {
            let prev_info = classify_packet(prev).expect("previously accepted packet");
            prev_info.cc == info.cc
                && pcr_masked_identical(
                    prev,
                    p,
                    prev_info.pcr_base.is_some(),
                    info.pcr_base.is_some(),
                )
        });
        self.last_pkt.insert(info.pid, *p);
        if info.pid == PAT_PID {
            if info.pusi {
                self.pat(payload)?;
            }
            return Ok(());
        }
        if let Some(&program_number) = self.pmt_pids.get(&info.pid) {
            if info.pusi {
                self.pmt(info.pid, program_number, payload)?;
            }
            return Ok(());
        }
        if info.pusi && payload.len() >= 9 && payload[..3] == [0, 0, 1] {
            if !duplicate {
                *self.summary.pes_starts_per_pid.entry(info.pid).or_insert(0) += 1;
            }
            self.pes(info.pid, payload)?;
        }
        Ok(())
    }

    /// One PSI section starting at a pointer field; single-packet
    /// sections only. `body` (once returned) is always exactly `len`
    /// bytes and always at least 9 (`table_id_ext(2) + version/
    /// current_next(1) + section_number(1) + last_section_number(1) +
    /// CRC32(4)` — the minimum shape of ANY PSI section, PAT or PMT,
    /// even with zero loop entries), so callers can rely on indices
    /// `0..9` existing without their own length check.
    ///
    /// `Ok(None)` means the section's CRC-32 did not check out and the
    /// section must be IGNORED — not treated as an error. The distinction
    /// is load-bearing:
    ///
    /// - Ignoring, rather than trusting, is what stops one damaged byte
    ///   from poisoning the reader for the rest of a capture. A flip in a
    ///   PAT's program loop rewrites a PMT PID; trusting it registers
    ///   whatever PID that lands on — which in a two-program multiplex is
    ///   a real ELEMENTARY STREAM — as a PMT PID, after which every one
    ///   of that stream's packets fails to parse as a PSI section.
    /// - Ignoring, rather than erroring, is what stops it from being
    ///   mistaken for a sync loss. Packet framing is untouched; only the
    ///   section's contents are unusable. An `Err` here would make the
    ///   resync-mode caller record a [`Resync`] and skip a perfectly
    ///   well-framed packet.
    ///
    /// Discarding a CRC-failed table is also just what a PSI filter does:
    /// the next repetition of the table carries the same information.
    fn section<'a>(&mut self, payload: &'a [u8]) -> Result<Option<(u8, &'a [u8])>, String> {
        const SECTION_HEADER_MIN: usize = 9;
        let &ptr_byte = payload.first().ok_or("PSI section: empty payload")?;
        let sec = payload
            .get(1 + usize::from(ptr_byte)..)
            .ok_or("PSI section: pointer field past packet")?;
        if sec.len() < 3 {
            return Err(format!(
                "PSI section: {} header byte(s) after the pointer field, want >= 3",
                sec.len()
            ));
        }
        let table_id = sec[0];
        let len = (usize::from(sec[1] & 0x0F) << 8) | usize::from(sec[2]);
        if len < SECTION_HEADER_MIN {
            return Err(format!(
                "PSI section_length {len}, want >= {SECTION_HEADER_MIN} (table_id_ext + \
                 version/current_next + section_number + last_section_number + CRC32)"
            ));
        }
        let body = sec
            .get(3..3 + len)
            .ok_or("PSI section spans packets (unsupported)")?;
        // The CRC covers the whole section: the 3-byte header through the
        // last byte before the trailer. `len >= 9` above guarantees the
        // split below has both halves.
        let (covered, trailer) = sec[..3 + len].split_at(3 + len - 4);
        let declared = u32::from_be_bytes(trailer.try_into().expect("4 trailer bytes"));
        if crc32_mpeg2(covered) != declared {
            self.summary.psi_crc_rejected += 1;
            return Ok(None);
        }
        // body = [table_id_ext(2), ver/cni(1), sec_num(1), last_sec(1), ..., crc(4)]
        Ok(Some((table_id, body)))
    }

    fn pat(&mut self, payload: &[u8]) -> Result<(), String> {
        let Some((tid, body)) = self.section(payload)? else {
            return Ok(());
        };
        if tid != 0 {
            return Err(format!("PAT table_id 0x{tid:02x}"));
        }
        // Redundant with `section()`'s own >= 9 minimum today (a PAT's
        // minimum shape IS the generic PSI minimum: 5 header bytes + 4
        // CRC bytes, zero program entries), but named explicitly so a
        // PAT-shaped error survives independently of that shared check.
        if body.len() < 5 + 4 {
            return Err(format!(
                "PAT section {} byte(s), want >= 9 (5 header + CRC32)",
                body.len()
            ));
        }
        let loop_bytes = &body[5..body.len() - 4];
        for e in loop_bytes.chunks_exact(4) {
            let program_number = (u16::from(e[0]) << 8) | u16::from(e[1]);
            let pmt_pid = (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]);
            if program_number != 0 {
                self.pmt_pids.insert(pmt_pid, program_number);
            }
        }
        Ok(())
    }

    fn pmt(&mut self, pmt_pid: u16, program_number: u16, payload: &[u8]) -> Result<(), String> {
        let Some((tid, body)) = self.section(payload)? else {
            return Ok(());
        };
        if tid != 2 {
            return Err(format!("PMT table_id 0x{tid:02x}"));
        }
        // `body[5..9]` (PCR_PID + program_info_length) is covered by
        // `section()`'s own >= 9 minimum; `info_len` is only known AFTER
        // reading those bytes, so the program-info + ES loop + CRC space
        // needs its own check here — a PMT declaring more program-info
        // bytes than the section actually has room for must not silently
        // slice into (or past) the CRC.
        let pcr_pid = (u16::from(body[5] & 0x1F) << 8) | u16::from(body[6]);
        let info_len = (usize::from(body[7] & 0x0F) << 8) | usize::from(body[8]);
        let min_len = 9 + info_len + 4;
        if body.len() < min_len {
            return Err(format!(
                "PMT section {} byte(s), want >= {min_len} (5 header + pcr_pid + \
                 program_info_length + {info_len} program_info byte(s) + CRC32)",
                body.len()
            ));
        }
        let es = &body[9 + info_len..body.len() - 4];
        let mut streams = Vec::new();
        let mut k = 0;
        while k + 5 <= es.len() {
            let stream_type = es[k];
            let pid = (u16::from(es[k + 1] & 0x1F) << 8) | u16::from(es[k + 2]);
            let es_info_len = (usize::from(es[k + 3] & 0x0F) << 8) | usize::from(es[k + 4]);
            let desc = es
                .get(k + 5..k + 5 + es_info_len)
                .ok_or("ES info past section")?;
            let mut registration = None;
            let mut descriptor_tags = Vec::new();
            let mut m = 0;
            while m + 2 <= desc.len() {
                let tag = desc[m];
                let len = usize::from(desc[m + 1]);
                let data = desc
                    .get(m + 2..m + 2 + len)
                    .ok_or("descriptor past ES info")?;
                descriptor_tags.push(tag);
                if tag == 0x05 && data.len() >= 4 {
                    registration = Some([data[0], data[1], data[2], data[3]]);
                }
                m += 2 + len;
            }
            streams.push(Stream {
                pid,
                stream_type,
                registration,
                descriptor_tags,
            });
            k += 5 + es_info_len;
        }
        self.summary.programs.insert(
            program_number,
            Program {
                program_number,
                pmt_pid,
                pcr_pid,
                streams,
            },
        );
        Ok(())
    }

    fn pes(&mut self, pid: u16, payload: &[u8]) -> Result<(), String> {
        let stream_id = payload[3];
        let retention = self.retention;
        let shape = self.summary.pes.entry(pid).or_default();
        shape.stream_ids.insert(stream_id);
        // §2.4.3.7: byte 6 = '10' + flags, byte 7 = PTS_DTS_flags.., byte 8 = header_data_length.
        let pts_dts = payload[7] >> 6;
        let hdr_len = usize::from(payload[8]);
        if pts_dts & 0x2 != 0 {
            let q = payload.get(9..14).ok_or("PTS past packet")?;
            let pts = (u64::from((q[0] >> 1) & 0x7) << 30)
                | (u64::from(q[1]) << 22)
                | (u64::from((q[2] >> 1) & 0x7F) << 15)
                | (u64::from(q[3]) << 7)
                | u64::from(q[4] >> 1);
            self.summary
                .pts
                .entry(pid)
                .or_insert_with(|| TimestampSeries::new(retention, pid))
                .push(pts);
        }
        if let Some(d) = payload.get(9 + hdr_len..9 + hdr_len + 4) {
            match shape.first_payload_prefix {
                None => shape.first_payload_prefix = Some([d[0], d[1], d[2], d[3]]),
                Some(first) if first[..2] != d[..2] => shape.prefix_mismatches += 1,
                Some(_) => {}
            }
        }
        Ok(())
    }
}

pub fn summarize_file(path: &Path) -> io::Result<WireSummary> {
    let bytes = std::fs::read(path)?;
    let mut r = Reader::new();
    r.feed(&bytes).map_err(io::Error::other)?;
    r.finish().map_err(io::Error::other)
}

/// One elementary stream of the first program, as the mux tests read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    pub pid: u16,
    pub stream_type: u8,
    /// The PMT carries a `KLVA` registration descriptor for this stream.
    pub klva: bool,
}

/// What the tst-core mux tests read back from a muxed buffer: the PMT
/// facts from [`Reader`], plus per-PID PES payloads and PCR samples.
#[derive(Debug, Clone, Default)]
pub struct ParsedStream {
    pub pmt_pid: Option<u16>,
    pub pcr_pid: Option<u16>,
    pub streams: Vec<PmtStream>,
    /// Per PID, each PES as `(PTS if present, payload after the header)`.
    #[allow(clippy::type_complexity)]
    pub pes_by_pid: BTreeMap<u16, Vec<(Option<u64>, Vec<u8>)>>,
    /// `(pid, PCR in 27 MHz ticks)` per PCR-bearing packet, in order.
    pub pcr_samples: Vec<(u16, u64)>,
}

/// Parse a whole buffer of muxer output. Tolerances match the reader this
/// replaced: a trailing partial packet is dropped, a PCR flag on an
/// adaptation field too short to hold one is ignored, and the program
/// reported is the one the PAT lists first. Any other malformed input
/// panics; this is a test helper.
pub fn parsed(buf: &[u8]) -> ParsedStream {
    let mut buf = buf[..buf.len() - buf.len() % PKT].to_vec();
    for pkt in buf.chunks_exact_mut(PKT) {
        // PCR flag without room for the 6 PCR bytes: clear the flag.
        if pkt[3] & 0x20 != 0 && pkt[4] > 0 && pkt[5] & 0x10 != 0 && pkt[4] < 7 {
            pkt[5] &= !0x10;
        }
    }
    let buf = &buf[..];
    let first_pmt_pid = buf.chunks_exact(PKT).find_map(|p| {
        let info = classify_packet(p.try_into().ok()?).ok()?;
        if info.pid != PAT_PID || !info.pusi || !info.has_payload {
            return None;
        }
        let body = p.get(info.payload_off + 1 + usize::from(p[info.payload_off])..)?;
        // PAT entries start 8 bytes into the section, after the 3 header bytes.
        let end = 3 + ((usize::from(body.get(1)? & 0x0F) << 8) | usize::from(*body.get(2)?)) - 4;
        body.get(8..end.min(body.len()))?
            .chunks_exact(4)
            .find(|e| e[0] != 0 || e[1] != 0)
            .map(|e| (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]))
    });
    let mut reader = Reader::new();
    reader.feed(buf).expect("raw-TS reader accepts the buffer");
    let summary = reader.finish().expect("buffer ends on a packet boundary");
    let mut out = ParsedStream::default();
    if let Some(prog) = summary
        .programs
        .values()
        .find(|p| Some(p.pmt_pid) == first_pmt_pid)
        .or_else(|| summary.programs.values().next())
    {
        out.pmt_pid = Some(prog.pmt_pid);
        out.pcr_pid = Some(prog.pcr_pid);
        out.streams = prog
            .streams
            .iter()
            .map(|s| PmtStream {
                pid: s.pid,
                stream_type: s.stream_type,
                klva: s.registration == Some(*b"KLVA"),
            })
            .collect();
    }
    let mut open: BTreeMap<u16, (Option<u64>, Vec<u8>)> = BTreeMap::new();
    for pkt in buf.chunks_exact(PKT) {
        let pkt: &[u8; PKT] = pkt.try_into().expect("chunk is one packet");
        let info = classify_packet(pkt).expect("reader accepted the packet");
        if info.pcr_base.is_some() {
            let ext = (u64::from(pkt[10] & 1) << 8) | u64::from(pkt[11]);
            out.pcr_samples
                .push((info.pid, info.pcr_base.unwrap_or(0) * 300 + ext));
        }
        if !info.has_payload || !out.streams.iter().any(|s| s.pid == info.pid) {
            continue;
        }
        let payload = &pkt[info.payload_off..];
        if info.pusi {
            if let Some(done) = open.remove(&info.pid) {
                out.pes_by_pid.entry(info.pid).or_default().push(done);
            }
            let pts = (payload[7] & 0x80 != 0).then(|| {
                let b = &payload[9..14];
                (u64::from((b[0] >> 1) & 7) << 30)
                    | (u64::from(b[1]) << 22)
                    | (u64::from(b[2] >> 1) << 15)
                    | (u64::from(b[3]) << 7)
                    | u64::from(b[4] >> 1)
            });
            open.insert(
                info.pid,
                (pts, payload[9 + usize::from(payload[8])..].to_vec()),
            );
        } else if let Some((_, body)) = open.get_mut(&info.pid) {
            body.extend_from_slice(payload);
        }
    }
    for (pid, done) in open {
        out.pes_by_pid.entry(pid).or_default().push(done);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(table_id: u8, ext: u16, body: &[u8]) -> Vec<u8> {
        let len = 5 + body.len() + 4;
        let mut s = vec![
            table_id,
            0xB0 | (len >> 8) as u8,
            len as u8,
            (ext >> 8) as u8,
            ext as u8,
            0xC1,
            0,
            0,
        ];
        s.extend_from_slice(body);
        let crc = crc32_mpeg2(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    fn packet(pid: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![SYNC, 0x40 | (pid >> 8) as u8, pid as u8, 0x10];
        p.extend_from_slice(payload);
        p.resize(PKT, 0xFF);
        p
    }

    fn psi(pid: u16, sec: &[u8]) -> Vec<u8> {
        let mut pl = vec![0];
        pl.extend_from_slice(sec);
        packet(pid, &pl)
    }

    fn pat(entries: &[(u16, u16)]) -> Vec<u8> {
        let body: Vec<u8> = entries
            .iter()
            .flat_map(|&(n, pid)| [(n >> 8) as u8, n as u8, 0xE0 | (pid >> 8) as u8, pid as u8])
            .collect();
        psi(0, &section(0, 1, &body))
    }

    fn pmt(pid: u16, program: u16, es_pid: u16) -> Vec<u8> {
        let body = [
            0xE0 | (es_pid >> 8) as u8,
            es_pid as u8,
            0xF0,
            0,
            0x1B,
            0xE0 | (es_pid >> 8) as u8,
            es_pid as u8,
            0xF0,
            0,
        ];
        psi(pid, &section(2, program, &body))
    }

    #[test]
    fn a_trailing_partial_packet_is_dropped() {
        let mut b = pat(&[(1, 0x100)]);
        b.extend(pmt(0x100, 1, 0x101));
        b.extend([0x47; 100]);
        assert_eq!(parsed(&b).pmt_pid, Some(0x100));
    }

    #[test]
    fn a_pcr_flag_without_room_is_ignored() {
        let mut b = pat(&[(1, 0x100)]);
        b.extend(pmt(0x100, 1, 0x101));
        // adaptation field of length 3 claiming a PCR.
        let mut p = vec![SYNC, 0x01, 0x01, 0x30, 3, 0x10, 0, 0];
        p.resize(PKT, 0xFF);
        b.extend(p);
        let out = parsed(&b);
        assert!(out.pcr_samples.is_empty());
        assert_eq!(out.streams.len(), 1);
    }

    #[test]
    fn the_first_pat_entry_picks_the_program() {
        let mut b = pat(&[(2, 0x200), (1, 0x100)]);
        b.extend(pmt(0x200, 2, 0x201));
        b.extend(pmt(0x100, 1, 0x101));
        let out = parsed(&b);
        assert_eq!(out.pmt_pid, Some(0x200));
        assert_eq!(out.streams[0].pid, 0x201);
    }
}
