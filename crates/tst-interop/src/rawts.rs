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
/// corruption tap (`corrupt.rs`) and the tee coordinate need without
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
    #[cfg(test)]
    pub(crate) fn from_values(retention: Retention, pid: u16, values: &[u64]) -> Self {
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
            // `packets` was incremented just above, so this packet's own
            // 0-based ordinal is one less — see `take_pcr_events`.
            self.pcr_events.push((base, self.summary.packets - 1));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Per-process counter that keeps two concurrently-running tests
    /// from picking the same scratch path. Two of these helpers' callers
    /// ask for the same profile, and cargo runs tests in parallel, so a
    /// path keyed only on the profile name has one test reading the file
    /// another is still writing.
    static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let n = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tst-interop-rawts-{tag}-{}-{n}.ts",
            std::process::id()
        ))
    }

    fn summary_of(profile: &str, seconds: f64) -> WireSummary {
        let p = crate::profiles::by_name(profile).unwrap();
        let path = scratch_path(profile);
        crate::r#gen::run(
            p,
            seconds,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let s = summarize_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        s
    }

    /// A [`Retention::Bounded`] series must stop growing while every
    /// statistic that is not a median stays exactly what an unbounded one
    /// would report. This is the whole memory argument for a multi-day
    /// receive loop, asserted directly: `SAMPLE_CAP * 8` bytes and not a
    /// byte more, no matter how long the capture runs.
    #[test]
    fn a_bounded_series_caps_its_sample_and_keeps_its_counters_exact() {
        const N: u64 = 200_000;
        let step = 1_800u64;
        let values: Vec<u64> = (0..N).map(|i| (i * step) % TS_MODULUS).collect();
        let bounded = TimestampSeries::from_values(Retention::Bounded, 0x1011, &values);
        let full = TimestampSeries::from_values(Retention::Full, 0x1011, &values);

        assert!(
            bounded.retained() <= SAMPLE_CAP,
            "retained {} deltas, cap is {SAMPLE_CAP}",
            bounded.retained()
        );
        assert_eq!(full.retained(), (N - 1) as usize, "Full keeps everything");
        // Exact in both modes: counts, wrap count, and the forward
        // min/max `VerifyMode::Strict` gates every interval on.
        assert_eq!(bounded.count(), full.count());
        assert_eq!(bounded.intervals(), full.intervals());
        assert_eq!(bounded.wraps(), full.wraps());
        assert_eq!(bounded.forward_min(), full.forward_min());
        assert_eq!(bounded.forward_max(), full.forward_max());
        // The two medians are the same statistic drawn from a sample
        // rather than the population — on a steady cadence they agree
        // exactly, which is the case every oracle actually meets.
        assert_eq!(bounded.forward_median(), full.forward_median());
        assert_eq!(bounded.positive_step_median(), full.positive_step_median());
    }

    /// The bound must not change a verdict. Every wire oracle is run
    /// twice over the same capture — once off a `Full` reader, once off a
    /// `Bounded` one — and must produce identical failure lists, in both
    /// verify modes, for a profile whose PCR cadence, audio cadence and
    /// PTS rollover exercise all four series statistics.
    #[test]
    fn bounded_retention_does_not_change_any_wire_oracle_verdict() {
        for profile in ["baseline", "audio", "pts-rollover"] {
            let bytes = bytes_of(profile, 7.0, &format!("bounded-{profile}"));
            let summarize = |retention| {
                let mut r = Reader::with_retention(retention);
                r.feed(&bytes).unwrap();
                r.finish().unwrap()
            };
            let full = summarize(Retention::Full);
            let bounded = summarize(Retention::Bounded);
            let p = crate::profiles::by_name(profile).unwrap();
            let inv = crate::profiles::invariants(p);
            // The per-program demux counts the oracles also read are
            // irrelevant here (identical for both readers by
            // construction), so an empty map keeps the comparison on the
            // wire half, which is the half retention touches.
            let per_program = BTreeMap::new();
            for mode in [
                crate::verify::VerifyMode::Strict,
                crate::verify::VerifyMode::Lossy,
            ] {
                assert_eq!(
                    crate::oracles::check(p, &inv, &full, &per_program, 7.0, 0.7, mode, 0),
                    crate::oracles::check(p, &inv, &bounded, &per_program, 7.0, 0.7, mode, 0),
                    "{profile} in {mode:?}"
                );
            }
        }
    }

    /// Bytes of `profile`/`seconds`, straight from the generator.
    fn bytes_of(profile: &str, seconds: f64, tag: &str) -> Vec<u8> {
        let p = crate::profiles::by_name(profile).unwrap();
        let path = scratch_path(tag);
        crate::r#gen::run(
            p,
            seconds,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        b
    }

    /// A clean capture must have ZERO rejected sections. Without this the
    /// CRC check below would pass just as well if `crc32_mpeg2` always
    /// disagreed: "rejects a damaged table" and "rejects every table" look
    /// identical from the damaged side.
    #[test]
    fn a_clean_capture_rejects_no_psi_section() {
        let s = summary_of("two-program", 3.0);
        assert_eq!(s.psi_crc_rejected, 0);
        assert_eq!(s.programs.len(), 2, "both PMTs were accepted");
    }

    /// One flipped byte in a PAT's program loop must not be trusted.
    ///
    /// In `two-program`, PAT byte 16 is the low byte of program 1's PMT
    /// PID (0x1000). Flipping it to 0x1031 names a PID that is a real
    /// ELEMENTARY STREAM in this multiplex — so a reader that trusted the
    /// CRC-failed table would register 0x1031 as a PMT PID and then fail
    /// to parse every one of that stream's 600 packets as a PSI section,
    /// skipping each one. Measured before the CRC check existed: 600
    /// resyncs and 6000 of 6600 packets counted.
    ///
    /// The section is IGNORED instead: the PID map is untouched, packet
    /// framing is unaffected so nothing resyncs, every packet is counted,
    /// and the discard is reported rather than hidden.
    #[test]
    fn a_crc_failed_pat_is_ignored_and_poisons_nothing() {
        let mut bytes = bytes_of("two-program", 60.0, "crcpat");
        let packets = bytes.len() / PKT;
        assert_eq!(packets, 6600);
        assert_eq!(
            classify_packet(bytes[..PKT].try_into().unwrap())
                .unwrap()
                .pid,
            0
        );
        bytes[16] ^= 0x31;

        let mut r = Reader::new();
        r.set_resync_mode(true);
        r.feed(&bytes).unwrap();
        assert!(
            !r.is_pmt_pid(0x1031),
            "an elementary stream must not become a PMT PID"
        );
        assert!(
            r.take_resyncs().is_empty(),
            "framing is intact: no sync was lost"
        );
        let s = r.finish().unwrap();
        assert_eq!(s.packets, packets as u64, "no packet was skipped");
        assert_eq!(s.psi_crc_rejected, 1, "exactly the damaged PAT");
        // The undamaged PAT repetitions still carry the real topology.
        assert_eq!(s.programs.len(), 2);
        assert_eq!(s.programs[&1].pmt_pid, 0x1000);
    }

    #[test]
    fn baseline_pmt_and_pes_shapes_match_the_measured_wire() {
        let s = summary_of("baseline", 3.0);
        let prog = &s.programs[&1];
        assert_eq!(prog.pmt_pid, 0x1000);
        assert_eq!(prog.pcr_pid, 0x1011);
        let video = prog.streams.iter().find(|st| st.pid == 0x1011).unwrap();
        assert_eq!(video.stream_type, 0x1B);
        assert_eq!(video.registration, None);
        let klv = prog.streams.iter().find(|st| st.pid == 0x1031).unwrap();
        assert_eq!(klv.stream_type, 0x06);
        assert_eq!(klv.registration, Some(*b"KLVA"));
        assert_eq!(s.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
        assert_eq!(s.pes[&0x1011].first_payload_prefix, Some([0, 0, 0, 1]));
        assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xBD]));
        assert_eq!(s.pts[&0x1011].count(), 90);
        assert_eq!(s.pcr[&0x1011].count(), 45);
        assert_eq!(s.packets, 180);
    }

    #[test]
    fn klv_sync_pmt_carries_0x15_with_metadata_descriptors() {
        let s = summary_of("klv-sync", 3.0);
        let klv = s.programs[&1]
            .streams
            .iter()
            .find(|st| st.pid == 0x1031)
            .unwrap();
        assert_eq!(klv.stream_type, 0x15);
        assert_eq!(klv.registration, Some(*b"KLVA"));
        assert!(klv.descriptor_tags.contains(&0x26) && klv.descriptor_tags.contains(&0x27));
        assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xFC]));
    }

    #[test]
    fn av1_modes_differ_only_in_pes_stream_id_and_prefix() {
        let a = summary_of("av1-klv-a", 3.0);
        let b = summary_of("av1-klv-b", 3.0);
        assert_eq!(
            a.programs[&1]
                .streams
                .iter()
                .map(|s| (s.pid, s.stream_type, s.registration))
                .collect::<Vec<_>>(),
            b.programs[&1]
                .streams
                .iter()
                .map(|s| (s.pid, s.stream_type, s.registration))
                .collect::<Vec<_>>()
        );
        assert_eq!(a.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
        assert_eq!(b.pes[&0x1011].stream_ids, BTreeSet::from([0xBD]));
        assert_eq!(
            &b.pes[&0x1011].first_payload_prefix.unwrap()[..3],
            &[0, 0, 1]
        );
        assert_ne!(
            &a.pes[&0x1011].first_payload_prefix.unwrap()[..3],
            &[0, 0, 1]
        );
    }

    #[test]
    fn two_program_summary_has_both_programs_with_distinct_pids() {
        let s = summary_of("two-program", 3.0);
        assert_eq!(s.programs.len(), 2);
        assert_eq!(s.programs[&2].pmt_pid, 0x1100);
        assert!(s.packets_per_pid[&0x1111] > 0 && s.packets_per_pid[&0x1131] > 0);
    }

    #[test]
    fn pts_rollover_stream_shows_one_raw_pts_decrease_at_seven_seconds() {
        let s = summary_of("pts-rollover", 7.0);
        // The series counts a rollover exactly (a property of one
        // consecutive pair), which is what the `pts_wrap` oracle reads.
        assert_eq!(s.pts[&0x1011].wraps(), 1);
    }

    /// `take_pcr_events` must report each base against the ordinal of the
    /// packet that CARRIED it, independently of how the bytes were
    /// chunked into `feed`. Checked against `classify_packet` walked over
    /// the same bytes packet by packet — the `Reader`'s own streaming
    /// bookkeeping compared to a flat, chunk-free enumeration.
    #[test]
    fn take_pcr_events_stamps_each_base_at_its_own_packet_ordinal() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-pcrat-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let want: Vec<(u64, u64)> = bytes
            .chunks_exact(PKT)
            .enumerate()
            .filter_map(|(i, c)| {
                let pkt: [u8; PKT] = c.try_into().expect("PKT bytes");
                classify_packet(&pkt)
                    .expect("generated packets classify")
                    .pcr_base
                    .map(|b| (b, i as u64))
            })
            .collect();
        assert!(!want.is_empty(), "the baseline profile carries PCRs");

        // Odd chunk size on purpose: a PCR packet routinely straddles two
        // feeds, which is exactly the case a "did the latest PCR base
        // change since the previous feed" diff would mis-stamp.
        let mut r = Reader::new();
        let mut got = Vec::new();
        for chunk in bytes.chunks(101) {
            r.feed(chunk).unwrap();
            got.extend(r.take_pcr_events());
        }
        assert_eq!(got, want);
        // Drained: everything was taken, nothing is reported twice.
        assert!(r.take_pcr_events().is_empty());
    }

    #[test]
    fn feed_tolerates_arbitrary_chunking_and_rejects_sync_loss() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-chunk-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        for chunk in bytes.chunks(101) {
            r.feed(chunk).unwrap();
        }
        assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
        let mut bad = bytes.clone();
        bad[188 * 5] = 0x00;
        let mut r = Reader::new();
        let e = r.feed(&bad).unwrap_err();
        assert!(e.contains("sync"), "{e}");
    }

    #[test]
    fn resync_discards_a_stale_partial_packet_without_losing_the_summary() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-resync-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let mut r = Reader::new();
        // A partial packet (100 bytes < 188) left over from a connection
        // that just broke — never completes on its own.
        r.feed(&bytes[..100]).unwrap();
        r.resync();
        // A whole, independent valid stream from the "reconnected"
        // transport must feed clean (no sync-loss error) and its packet
        // count must reflect only THIS stream — the discarded partial
        // packet contributed nothing.
        r.feed(&bytes).unwrap();
        assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
    }

    #[test]
    fn finish_rejects_a_trailing_partial_packet() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-trailing-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let cut = 3 * PKT + 100;
        assert!(bytes.len() > cut, "fixture too short for this test");

        let mut r = Reader::new();
        r.feed(&bytes[..cut]).unwrap();
        let e = r.finish().unwrap_err();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn feed_rejects_a_pat_pointer_field_past_the_payload() {
        // A minimal PID-0 (PAT) packet, PUSI set, adaptation_field_control
        // = payload-only (afc=01 -> byte 3 = 0x10), pointer_field = 255 —
        // nowhere near valid for a 184-byte payload, so `section()` must
        // reject it rather than reading (or panicking on) bytes past the
        // packet.
        let mut pkt = [0xFFu8; PKT];
        pkt[0] = SYNC;
        pkt[1] = 0x40; // PUSI set, PID high bits = 0
        pkt[2] = 0x00; // PID low byte = 0 (PAT)
        pkt[3] = 0x10; // afc = payload only
        pkt[4] = 0xFF; // pointer_field = 255

        let mut r = Reader::new();
        let e = r.feed(&pkt).unwrap_err();
        assert!(e.contains("pointer field"), "{e}");
    }

    #[test]
    fn feed_rejects_a_pcr_flag_with_a_too_short_adaptation_field() {
        // adaptation_field_control = adaptation-field-only (afc=10 ->
        // byte 3 = 0x20), adaptation_field_length = 3 (too short to hold
        // the flags byte + a 6-byte PCR), PCR flag set anyway — malformed,
        // must error rather than reading past the declared field.
        let mut pkt = [0xFFu8; PKT];
        pkt[0] = SYNC;
        pkt[1] = 0x00;
        pkt[2] = 0x11; // an arbitrary non-PAT/PMT PID
        pkt[3] = 0x20; // afc = adaptation field only
        pkt[4] = 3; // adaptation_field_length
        pkt[5] = 0x10; // PCR flag set

        let mut r = Reader::new();
        let e = r.feed(&pkt).unwrap_err();
        assert!(e.contains('7'), "{e}");
    }

    #[test]
    fn classify_packet_reports_pid_pusi_pcr_and_payload_offset() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-cls-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            1.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let first: [u8; PKT] = bytes[..PKT].try_into().unwrap();
        let info = classify_packet(&first).unwrap();
        assert_eq!(info.pid, 0, "first packet of a fresh mux is the PAT");
        assert!(info.pusi);
        assert!(info.has_payload);
        assert_eq!(info.payload_off, 4);
        // Find a PCR-bearing packet and check the base decodes.
        let pcr_pkt = bytes
            .chunks_exact(PKT)
            .find(|c| c[3] & 0x20 != 0 && c[4] > 0 && c[5] & 0x10 != 0)
            .unwrap();
        let info = classify_packet(pcr_pkt.try_into().unwrap()).unwrap();
        assert!(info.pcr_base.is_some());
        assert_eq!(info.payload_off, 5 + usize::from(pcr_pkt[4]));
        let mut bad = first;
        bad[0] = 0x00;
        assert!(classify_packet(&bad).unwrap_err().contains("sync"));
    }

    #[test]
    fn reader_exposes_packet_count_pcr_events_and_pmt_pids() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-acc-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            1.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        assert_eq!(r.packets(), 0);
        assert!(r.take_pcr_events().is_empty());
        r.feed(&bytes).unwrap();
        assert_eq!(r.packets() as usize, bytes.len() / PKT);
        assert!(r.take_pcr_events().last().is_some());
        assert!(r.is_pmt_pid(0x1000));
        assert!(!r.is_pmt_pid(0x1011));
    }

    /// `pes_starts_per_pid` counts one per PES start on every PID —
    /// including the async KLV PID, whose PES may carry no PTS — and
    /// counts a spec-legal duplicate packet once.
    #[test]
    fn reader_counts_pes_starts_per_pid_and_ignores_a_duplicate_packet() {
        let bytes = bytes_of("baseline", 3.0, "pes-starts");
        let mut r = Reader::new();
        r.feed(&bytes).unwrap();
        let s = r.finish().unwrap();
        assert_eq!(s.pes_starts_per_pid[&0x1011], 90, "30 fps x 3 s");
        assert_eq!(s.pes_starts_per_pid[&0x1031], 30, "10 Hz x 3 s");

        // Duplicate the first video PES-start packet in place (same CC,
        // same bytes): the count must not move.
        let first = bytes
            .chunks_exact(PKT)
            .position(|c| {
                let pkt: [u8; PKT] = c.try_into().unwrap();
                let info = classify_packet(&pkt).unwrap();
                info.pid == 0x1011 && info.pusi
            })
            .expect("a video PES start");
        let mut dup = Vec::with_capacity(bytes.len() + PKT);
        dup.extend_from_slice(&bytes[..(first + 1) * PKT]);
        dup.extend_from_slice(&bytes[first * PKT..(first + 1) * PKT]);
        dup.extend_from_slice(&bytes[(first + 1) * PKT..]);
        let mut r = Reader::new();
        r.feed(&dup).unwrap();
        let s = r.finish().unwrap();
        assert_eq!(
            s.pes_starts_per_pid[&0x1011], 90,
            "a duplicate is not a second PES"
        );
        assert_eq!(s.packets_per_pid[&0x1011], bytes_of_pid(&bytes, 0x1011) + 1);
    }

    /// PR review finding: duplicate detection compared only the
    /// continuity counter, so a non-conformant encoder that repeats a CC
    /// WITHOUT repeating the packet's other bytes had its second PES
    /// start silently swallowed — undercounting `pes_starts_per_pid` and
    /// weakening `wire_vs_demux_*`. A same-CC packet whose payload
    /// differs must count as its own PES start.
    #[test]
    fn reader_counts_a_same_cc_packet_with_different_bytes_as_a_new_pes_start() {
        let bytes = bytes_of("baseline", 3.0, "pes-starts-non-dup");
        let first = bytes
            .chunks_exact(PKT)
            .position(|c| {
                let pkt: [u8; PKT] = c.try_into().unwrap();
                let info = classify_packet(&pkt).unwrap();
                info.pid == 0x1011 && info.pusi
            })
            .expect("a video PES start");
        // Same CC as `first`, but flip a payload byte well past the PES
        // header so it stays a valid PES start with different content —
        // the "non-conformant encoder" case `pcr_masked_identical` must
        // reject as NOT a duplicate.
        let mut altered: [u8; PKT] = bytes[first * PKT..(first + 1) * PKT].try_into().unwrap();
        altered[100] ^= 0x01;
        let mut mutated = Vec::with_capacity(bytes.len() + PKT);
        mutated.extend_from_slice(&bytes[..(first + 1) * PKT]);
        mutated.extend_from_slice(&altered);
        mutated.extend_from_slice(&bytes[(first + 1) * PKT..]);
        let mut r = Reader::new();
        r.feed(&mutated).unwrap();
        let s = r.finish().unwrap();
        assert_eq!(
            s.pes_starts_per_pid[&0x1011], 91,
            "a same-CC packet with different bytes is not a duplicate: {s:?}"
        );
    }

    /// Packets on `pid` in `bytes` — the raw denominator
    /// `reader_counts_pes_starts_per_pid_and_ignores_a_duplicate_packet`
    /// checks the duplicated capture against.
    fn bytes_of_pid(bytes: &[u8], pid: u16) -> u64 {
        bytes
            .chunks_exact(PKT)
            .filter(|c| classify_packet((*c).try_into().unwrap()).unwrap().pid == pid)
            .count() as u64
    }

    #[test]
    fn resync_mode_hunts_forward_and_records_each_resync() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-hunt-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // Truncate packet 5 to 100 bytes and insert 37 garbage bytes after packet 20.
        let mut bad = Vec::new();
        for (i, pkt) in bytes.chunks_exact(PKT).enumerate() {
            if i == 5 {
                bad.extend_from_slice(&pkt[..100]);
            } else {
                bad.extend_from_slice(pkt);
            }
            if i == 20 {
                bad.extend(std::iter::repeat(0x55u8).take(37));
            }
        }
        let mut r = Reader::new();
        r.set_resync_mode(true);
        for chunk in bad.chunks(1316) {
            r.feed(chunk).unwrap();
        }
        let got = r.take_resyncs();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].at_packets, 5);
        assert_eq!(got[0].skipped_bytes, 100);
        assert_eq!(got[1].at_packets, 20);
        assert_eq!(got[1].skipped_bytes, 37);
        assert_eq!(r.resync_count(), 2);
        // Every other packet was accepted.
        let s = r.finish().unwrap();
        assert_eq!(s.packets as usize, bytes.len() / PKT - 1);
    }

    #[test]
    fn resync_mode_treats_a_trailing_partial_packet_as_a_resync_not_an_error() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path =
            std::env::temp_dir().join(format!("tst-interop-rawts-tail-{}.ts", std::process::id()));
        crate::r#gen::run(
            p,
            1.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut r = Reader::new();
        r.set_resync_mode(true);
        r.feed(&bytes[..3 * PKT + 50]).unwrap();
        let trailing = r.trailing_resync().expect("non-empty carry in resync mode");
        assert_eq!(trailing.at_packets, 3);
        assert_eq!(trailing.skipped_bytes, 50);
        let s = r.finish().unwrap();
        assert_eq!(s.packets, 3);
    }

    #[test]
    fn resync_mode_records_a_malformed_packet_as_a_resync_and_continues() {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rawts-malformed-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            2.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut bad = bytes.clone();
        // Packet 7 (0-based): force the adaptation-field bit on and claim
        // an adaptation_field_length that overruns the 188-byte packet
        // (5 + 200 > 188) — malformed, but the sync byte is untouched, so
        // the resync-hunt's own byte/lookahead check accepts `off` as a
        // normal packet start and only the deeper `classify_packet` parse
        // catches it.
        bad[7 * PKT + 3] |= 0x20;
        bad[7 * PKT + 4] = 200;
        let mut r = Reader::new();
        r.set_resync_mode(true);
        for chunk in bad.chunks(1316) {
            r.feed(chunk).unwrap();
        }
        let got = r.take_resyncs();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].at_packets, 7);
        assert_eq!(got[0].skipped_bytes, PKT);
        let s = r.finish().unwrap();
        assert_eq!(s.packets as usize, bytes.len() / PKT - 1);

        let mut r = Reader::new();
        let e = r.feed(&bad).unwrap_err();
        assert!(e.contains("adaptation field length"), "{e}");
    }

    /// X-CORR-06A (E06): garbage with no sync candidate used to be kept
    /// in the carry and rescanned on every feed — 10 000 x 1316 bytes of
    /// 0x55 retained 13 160 000 bytes. Only the <= 187-byte suffix that
    /// could still complete a packet may survive a feed, and the bytes
    /// thrown away must still be counted in the recovery that follows.
    #[test]
    fn resync_mode_bounds_the_carry_on_confirmed_garbage_and_counts_it() {
        const CHUNKS: usize = 10_000;
        let garbage = [0x55u8; 1316];
        let mut r = Reader::with_retention(Retention::Bounded);
        r.set_resync_mode(true);
        for _ in 0..CHUNKS {
            r.feed(&garbage).unwrap();
            assert!(
                r.carry.len() < PKT,
                "carry holds {} bytes; only a partial-packet suffix may survive a feed",
                r.carry.len()
            );
        }
        // Now three clean packets: the hunt confirms the first and the
        // recovery names every garbage byte ever fed.
        let clean = bytes_of("baseline", 1.0, "carry-bound");
        r.feed(&clean[..3 * PKT]).unwrap();
        let got = r.take_resyncs();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].at_packets, 0);
        assert_eq!(got[0].skipped_bytes, CHUNKS * 1316);
        assert_eq!(r.packets(), 3);
        assert!(r.carry.is_empty());
    }

    /// The retained suffix may itself BEGIN at a real packet start that
    /// only a later feed can confirm — the hunt never runs for it, so the
    /// recovery has to be recorded where the packet is accepted. Feeding
    /// exactly `n` garbage bytes plus the packet's first 187 puts the
    /// boundary at `carry.len() - 187` on the nose.
    #[test]
    fn garbage_dropped_before_a_suffix_that_turns_out_to_be_a_packet_is_still_recorded() {
        let clean = bytes_of("baseline", 1.0, "suffix-start");
        let mut r = Reader::new();
        r.set_resync_mode(true);
        let mut first = vec![0x55u8; 1000];
        first.extend_from_slice(&clean[..PKT - 1]);
        r.feed(&first).unwrap();
        assert_eq!(r.carry.len(), PKT - 1, "the clean prefix is what survived");
        assert_eq!(r.carry[0], SYNC, "…and it starts at a packet boundary");
        assert_eq!(r.resync_count(), 0, "nothing is confirmed yet");

        r.feed(&clean[PKT - 1..]).unwrap();
        let got = r.take_resyncs();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].at_packets, 0);
        assert_eq!(got[0].skipped_bytes, 1000);
        assert_eq!(r.packets() as usize, clean.len() / PKT);
    }

    /// SIMP-11(a) / X-CORR-06B: recoveries are DRAINED per batch, like
    /// PCR events, so a multi-day capture never holds (or re-copies) its
    /// whole recovery history. Total events still equals the number of
    /// recoveries; the lifetime count survives the drains.
    #[test]
    fn resyncs_are_drained_in_batches_and_counted_for_life() {
        let bytes = bytes_of("baseline", 3.0, "drain");
        // Truncate every 50th packet to 100 bytes: one recovery each.
        let mut bad = Vec::with_capacity(bytes.len());
        let mut expected = 0u64;
        for (i, pkt) in bytes.chunks_exact(PKT).enumerate() {
            if i % 50 == 49 {
                bad.extend_from_slice(&pkt[..100]);
                expected += 1;
            } else {
                bad.extend_from_slice(pkt);
            }
        }
        let mut r = Reader::with_retention(Retention::Bounded);
        r.set_resync_mode(true);
        let mut total = 0u64;
        for chunk in bad.chunks(1316) {
            r.feed(chunk).unwrap();
            let batch = r.take_resyncs();
            total += batch.len() as u64;
            // A 1316-byte feed spans 7 packets: at most one recovery.
            assert!(batch.len() <= 1, "{batch:?}");
        }
        assert_eq!(total, expected);
        assert_eq!(r.resync_count(), expected);
        assert!(r.take_resyncs().is_empty(), "already drained");
    }

    /// The trailing recovery counts discarded garbage too.
    #[test]
    fn trailing_resync_includes_garbage_already_discarded() {
        let mut r = Reader::new();
        r.set_resync_mode(true);
        r.feed(&[0x55u8; 1316]).unwrap();
        r.feed(&[0x55u8; 100]).unwrap();
        let t = r
            .trailing_resync()
            .expect("garbage is a trailing fragment in resync mode");
        assert_eq!(t.skipped_bytes, 1416);
        assert!(r.carry.len() < PKT);
    }
}
