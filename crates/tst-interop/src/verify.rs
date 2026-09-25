//! Verification core: replay a captured MPEG-TS/KLV cell through the
//! offline [`Demuxer`], tally wire-format facts, and check them against a
//! [`Profile`]'s [`profiles::Invariants`] (see [`crate::profiles`]).
//!
//! **Classify by event kind, not by PMT `stream_type`.** The `av1-klv-*`
//! profiles mux AV1 video and async KLV onto the *same* PMT
//! `stream_type` byte (`0x06` — see `Invariants::video_stream_type` /
//! `klv_stream_type` for those profiles). [`Tally`] never reads
//! `stream_type` at all: it dispatches on which [`DemuxEvent`] variant
//! (and, for `Sample`, which [`SamplePayload`] variant) arrived, so two
//! streams sharing a `stream_type` byte never conflate their counts.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use tst_core::codec::misp_time;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DiscontinuityKind, MetadataKind, NonConformantIssue, SamplePayload,
    VideoCodec as DemuxVideoCodec,
};

use crate::corrupt::{self, AttributionReport, Injection, LogHeader};
use crate::fixtures::{self, KlvSet};
use crate::oracles::{self, Explained};
use crate::profiles::{self, Profile};
use crate::rawts::{self, WireSummary};
use crate::report_types::{CellMetrics, KlvRichMetrics, VerifyReport};

/// Whether a captured cell may carry `Discontinuity` events and still
/// pass. Both modes fail on `NonConformant` — that always indicates the
/// demuxer rejected something as spec-non-compliant, never merely
/// "traffic was interrupted." `Lossy` is for cells expected to survive a
/// scheduled outage/impairment (a discontinuity there is normal, expected
/// noise); `Strict` is for cells that must be lossless end to end (e.g.
/// the byte-transparent tier — see `run-matrix.sh`'s `run_peer_send_recv`,
/// which passes `recv --strict` there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyMode {
    Strict,
    Lossy,
}

/// Which KLV record set the capture under judgement was generated with,
/// and — for [`KlvSet::Rich`] — the seed its presence schedule was drawn
/// from. A receiver cannot infer either from the wire: the rich census
/// oracle checks a decoded record's tag set against
/// `fixtures::rich_presence(seed, seq)`, which is only computable by
/// someone told the same `(set, seed)` the sender used.
///
/// [`KlvExpect::compact`] is the default everywhere — the 157-cell
/// interop matrix generates and judges compact records, and a compact
/// capture has no presence schedule to check at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KlvExpect {
    pub set: KlvSet,
    pub seed: u64,
}

impl KlvExpect {
    /// The default expectation: compact records, no seeded schedule.
    #[must_use]
    pub fn compact() -> Self {
        KlvExpect {
            set: KlvSet::Compact,
            seed: 0,
        }
    }
}

impl Default for KlvExpect {
    fn default() -> Self {
        Self::compact()
    }
}

/// Map `profiles::VideoCodec` (this crate's own profile-shape enum) to
/// `tst_core`'s demux-side codec enum, for comparing a profile's expected
/// video codec against what a capture actually carried. A distinct,
/// hand-written mapping from `Invariants::video_stream_type` (which maps
/// codec to PMT byte) — this one maps codec to codec, so it can't be
/// fooled by a stream_type-byte-level bug the same way a byte-based check
/// could be.
fn expected_demux_video_codec(c: profiles::VideoCodec) -> DemuxVideoCodec {
    match c {
        profiles::VideoCodec::H264 => DemuxVideoCodec::H264,
        profiles::VideoCodec::H265 => DemuxVideoCodec::H265,
        profiles::VideoCodec::H266 => DemuxVideoCodec::H266,
        profiles::VideoCodec::Av1 => DemuxVideoCodec::Av1,
    }
}

/// Payload-free classification of a demuxed [`MetadataKind`] — sync
/// (`KlvSyncAuCell`, regardless of its struct-variant fields) vs. async
/// (`KlvAsync`) vs. an unrecognized metadata `stream_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum KlvCarriage {
    Sync,
    Async,
    Unknown,
}

fn klv_carriage_of(kind: &MetadataKind) -> KlvCarriage {
    match kind {
        MetadataKind::KlvSyncAuCell { .. } => KlvCarriage::Sync,
        MetadataKind::KlvAsync => KlvCarriage::Async,
        MetadataKind::Unknown(_) => KlvCarriage::Unknown,
    }
}

/// Map `profiles::KlvMode` to the [`KlvCarriage`] a conformant capture must
/// carry. `Async` and `AsyncWithMisp` both ride bare KLV LS (no AU cell
/// wrap) — MISP only changes what rides inside the *video* SEI, not the
/// KLV PID's carriage — so both map to `Async`.
fn expected_klv_carriage(m: profiles::KlvMode) -> KlvCarriage {
    match m {
        profiles::KlvMode::Sync => KlvCarriage::Sync,
        profiles::KlvMode::Async | profiles::KlvMode::AsyncWithMisp => KlvCarriage::Async,
    }
}

/// PES PTS field width per ITU-T H.222.0 V9 §2.4.3.6 — the wire value
/// cycles every `PTS_WRAP_MODULUS` 90 kHz ticks (~26.5 hours).
const PTS_WRAP_MODULUS: u64 = 1u64 << 33;
/// Half of [`PTS_WRAP_MODULUS`]. See [`pts_is_monotonic_step`].
const PTS_WRAP_HALF: u64 = PTS_WRAP_MODULUS / 2;

/// Is the transition `last -> now` (both raw 90 kHz PES PTS ticks, already
/// masked to 33 bits by construction — every `Pts90khz` this module sees
/// came off the wire via `Demuxer`) a monotonic step?
///
/// Independent of `tst_core`'s own `pts_diff_33bit` — this crate's whole
/// purpose is to verify tst-core's wire output, so its checks are computed
/// from spec first principles rather than delegated to the code under
/// test (mirrors the independence discipline in `crate::profiles`).
///
/// The rule: the forward distance `(now - last) mod PTS_WRAP_MODULUS` is
/// always in `0..PTS_WRAP_MODULUS`. A normal forward step (with or without
/// crossing the wrap boundary) yields a small forward distance. A genuine
/// backwards jump makes that forward distance implausibly large — closer
/// to a full cycle than to zero. `PTS_WRAP_HALF` is the cutoff: distances
/// at or below it are read as forward progress (accepting the wrap);
/// distances above it are read as a backwards jump and flagged as a
/// violation.
fn pts_is_monotonic_step(now: u64, last: u64) -> bool {
    let forward = (now + PTS_WRAP_MODULUS - last) % PTS_WRAP_MODULUS;
    forward <= PTS_WRAP_HALF
}

/// Hex-encode `bytes` (lowercase, no separator).
///
/// `pub(crate)`: `send.rs` reuses this to build `klv_set_sha256` from
/// the records it pushes and `transport.rs`'s `Teeing` tap reuses it for
/// `stream_sha256` — one hex-encoding decision for the whole crate.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String never fails");
    }
    s
}

/// Order-insensitive fingerprint of a KLV record set: sort the per-record
/// hex digests, then sha256 the concatenation. See
/// [`CellMetrics::klv_set_sha256`].
///
/// `pub(crate)`: `send.rs` computes this same fingerprint over the
/// records it pushes (sent-side ground truth), reusing this function
/// rather than re-deciding the hash shape.
pub(crate) fn klv_set_hash(record_digests: &[String]) -> String {
    let mut sorted = record_digests.to_vec();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    for digest in &sorted {
        hasher.update(digest.as_bytes());
    }
    to_hex(&hasher.finalize())
}

/// Fraction of nominal per-second counts (video AUs, KLV records) a
/// capture must clear to pass. Real captures commonly truncate a fraction
/// of a second at either end (peer startup/teardown), so requiring the
/// exact nominal count would fail otherwise-healthy captures.
///
/// `pub(crate)`: `recv.rs` uses the same slack for live-capture
/// `Tally::finish` calls, so a live cell and an offline `verify_file`
/// run are held to the identical bar.
pub(crate) const NOMINAL_COUNT_SLACK: f64 = 0.7;

/// The minimum acceptable count for a `per_sec`-Hz signal over
/// `seconds`, requiring at least `slack` (e.g. `0.7` = 70%) of the
/// nominal total. Shared by `Tally::finish`'s whole-capture floors
/// (video AUs, KLV records) and the private `oracles::program_accounting`'s
/// per-program floors — one formula, not two copies that could drift.
pub(crate) fn min_count(per_sec: u32, seconds: f64, slack: f64) -> u64 {
    (per_sec as f64 * seconds * slack).floor() as u64
}

/// Per-program media counts — [`Tally::feed`] tallies these off each
/// event's `StreamId::program_number`, alongside (not instead of) the
/// whole-capture totals, so [`crate::oracles::check`]'s per-program
/// accounting oracle can catch a program whose media is missing even
/// when the whole-capture totals still clear their floor (a two-program
/// capture where only program 1's media ever arrived).
#[derive(Clone, Copy, Default)]
pub struct ProgramCounts {
    pub video_aus: u64,
    pub klv_records: u64,
    pub audio_frames: u64,
}

/// Accumulates wire-format facts from a stream of [`DemuxEvent`]s.
pub struct Tally {
    video_aus: u64,
    keyframes: u64,
    klv_records: u64,
    /// Hex sha256 digest of each KLV record payload, in arrival order.
    /// Never pushed to when `track_klv_digests` is `false` — see that
    /// field's own doc comment.
    klv_digests: Vec<String>,
    /// When `false`, `feed` skips accumulating `klv_digests` entirely
    /// and `finish` reports `CellMetrics::klv_set_sha256: None` instead
    /// of computing the hash. `Tally::new()` defaults this to `true`
    /// (unchanged behavior); `recv --no-klv-digest` is the only caller
    /// that flips it off, via `disable_klv_digest_tracking` — a
    /// multi-day soak run would otherwise accumulate one digest string
    /// per KLV record for the ENTIRE run (never cleared until `finish`
    /// consumes it), an unbounded, harness-only allocation confirmed
    /// during Task 14's smoke run to be the dominant contributor to
    /// several MiB/hour of RSS growth that has nothing to do with the
    /// library code the soak means to measure.
    track_klv_digests: bool,
    audio_frames: u64,
    /// Per-`program_number` media counts — see [`ProgramCounts`].
    per_program: BTreeMap<u16, ProgramCounts>,
    programs_seen: BTreeSet<u16>,
    /// Distinct video codecs observed across all `Sample` events. Normally
    /// a singleton (one codec per profile); tracked as a set so an
    /// unexpected second codec is visible too.
    video_codecs_seen: HashSet<DemuxVideoCodec>,
    /// Distinct KLV carriage shapes (sync AU-cell vs. async) observed
    /// across all `Metadata` events. Normally a singleton per profile.
    klv_carriage_seen: HashSet<KlvCarriage>,
    misp_sei_seen: bool,
    pts_monotonic: bool,
    /// Last observed raw PTS tick per elementary-stream PID — PTS
    /// monotonicity is a per-PID invariant (ITU-T H.222.0 V9 §2.4.3.6).
    last_pts_by_pid: BTreeMap<u16, u64>,
    bytes: u64,
    stream_hasher: Sha256,
    discontinuities: u64,
    nonconformant: u64,
    /// The same two counts, split by the PID each event named. The
    /// wire-vs-demux floor is per PID, and so is the allowance it
    /// subtracts — see [`oracles::Explained`]. Under corruption the
    /// demuxer can surface an event on a PID the profile never muxed
    /// (a damaged PID field can name any 13-bit value), so the real
    /// bound is the PID space (8192), not the multiplex's own PID
    /// count — still hard-bounded and O(1), so a multi-day soak
    /// accumulates nothing here.
    discontinuities_by_pid: BTreeMap<u16, u64>,
    nonconformant_by_pid: BTreeMap<u16, u64>,
    /// `Debug`-formatted `DiscontinuityKind` of the first `Discontinuity`
    /// event fed, if any.
    first_discontinuity: Option<String>,
    /// `Display`-formatted `NonConformantIssue` of the first
    /// `NonConformant` event fed, if any.
    first_nonconformant: Option<String>,
    /// Judge for a capture whose sender ran a corruption tap — `None`
    /// (the default) for every other capture, and then nothing about
    /// this tally's behaviour changes. See [`Tally::attach_attribution`].
    attribution: Option<corrupt::Attribution>,
    /// What KLV record set this capture is expected to carry. Compact
    /// (the default) means `CellMetrics::klv_rich` comes back `None`.
    klv_expect: KlvExpect,
    /// What the rich oracles made of the records so far — see
    /// [`KlvRichMetrics`]. Untouched unless `klv_expect.set` is
    /// [`KlvSet::Rich`].
    rich: KlvRichMetrics,
    /// The first offender of EACH rich oracle, kept separately from
    /// `KlvRichMetrics::first_problem` (which holds the first problem of
    /// any kind) so each of the three failure strings quotes an example
    /// of its own verdict. A shared "first problem" would have a
    /// `klv_rich_census` failure quoting a decode error — the same
    /// wrong-evidence trap `finish`'s `first_unexplained` closure exists
    /// to avoid for the corruption verdicts.
    rich_first_decode: Option<String>,
    rich_first_census: Option<String>,
    rich_first_security: Option<String>,
}

/// Which of the three rich oracles a problem belongs to — see
/// [`Tally::note_rich_problem`].
#[derive(Clone, Copy)]
enum RichOracle {
    Decode,
    Census,
    Security,
}

/// ST 0601 Tag 48, the nested ST 0102 security local set.
const SECURITY_TAG: u32 = 48;

impl Default for Tally {
    fn default() -> Self {
        Self::new()
    }
}

impl Tally {
    pub fn new() -> Self {
        Self {
            video_aus: 0,
            keyframes: 0,
            klv_records: 0,
            klv_digests: Vec::new(),
            track_klv_digests: true,
            audio_frames: 0,
            per_program: BTreeMap::new(),
            programs_seen: BTreeSet::new(),
            video_codecs_seen: HashSet::new(),
            klv_carriage_seen: HashSet::new(),
            misp_sei_seen: false,
            pts_monotonic: true,
            last_pts_by_pid: BTreeMap::new(),
            bytes: 0,
            stream_hasher: Sha256::new(),
            discontinuities: 0,
            nonconformant: 0,
            discontinuities_by_pid: BTreeMap::new(),
            nonconformant_by_pid: BTreeMap::new(),
            first_discontinuity: None,
            first_nonconformant: None,
            attribution: None,
            klv_expect: KlvExpect::compact(),
            rich: KlvRichMetrics::default(),
            rich_first_decode: None,
            rich_first_census: None,
            rich_first_security: None,
        }
    }

    /// Tell this tally which KLV record set the capture carries. Call
    /// before the first event — a rich expectation set partway through
    /// would judge only the records that arrived after it, while
    /// `KlvRichMetrics::records` implies it saw them all.
    pub fn set_klv_expect(&mut self, expect: KlvExpect) {
        self.klv_expect = expect;
    }

    /// Stop accumulating per-record KLV digests — see
    /// `track_klv_digests`'s own doc comment for why. Only meaningful
    /// called before any `Metadata` events have been `feed`-ed (this
    /// crate's callers all call it immediately after `Tally::new()`);
    /// calling it later just stops FURTHER accumulation; it doesn't
    /// retroactively clear what's already there and would leave
    /// `finish` reporting `None` for a partially-built list, which is
    /// misleading — not a scenario this crate's real call sites hit.
    pub fn disable_klv_digest_tracking(&mut self) {
        self.track_klv_digests = false;
    }

    /// Fold one demuxed event into the tally, at no particular position
    /// in the capture — [`Tally::feed_at`] with a coordinate of 0.
    ///
    /// Every caller that has no corruption log to judge against uses
    /// this: the coordinate is only ever read by an attached
    /// [`corrupt::Attribution`], so feeding a constant is not a
    /// degradation, it is the absence of a question.
    pub fn feed(&mut self, ev: &DemuxEvent) {
        self.feed_at(ev, 0);
    }

    /// Fold one demuxed event into the tally, recording that it surfaced
    /// at receiver packet ordinal `at`.
    ///
    /// `at` matters only when an [`corrupt::Attribution`] is attached, in
    /// which case error-class events are offered to it as signals and
    /// media events as evidence of recovery. Callers must pass
    /// non-decreasing `at` values (the attribution engine keeps a sliding
    /// cursor rather than rescanning), which is what a receiver
    /// stamping events with a monotonically growing packet count
    /// naturally produces.
    pub fn feed_at(&mut self, ev: &DemuxEvent, at: u64) {
        if self.attribution.is_some() {
            self.route_to_attribution(ev, at);
        }
        match ev {
            DemuxEvent::ProgramMap(m) => {
                self.programs_seen.insert(m.program_number);
            }
            DemuxEvent::Sample {
                stream,
                pts,
                payload,
                ..
            } => {
                self.record_pts(stream.pid, *pts, at);
                match payload {
                    SamplePayload::Video {
                        codec,
                        raw,
                        random_access_indicator,
                        ..
                    } => {
                        self.video_aus += 1;
                        self.per_program
                            .entry(stream.program_number)
                            .or_default()
                            .video_aus += 1;
                        self.video_codecs_seen.insert(*codec);
                        if *random_access_indicator {
                            self.keyframes += 1;
                        }
                        // `extract` errors (Err) mean either an
                        // unsupported codec (H.266/AV1 — ST 0604 defines
                        // no carriage for them) or a malformed MISP SEI;
                        // either way there is no *valid* MISP timestamp to
                        // count, so both collapse to "not seen" here.
                        if let Ok(Some(_)) = misp_time::extract(raw, (*codec).into()) {
                            self.misp_sei_seen = true;
                        }
                    }
                    SamplePayload::Audio { .. } => {
                        self.audio_frames += 1;
                        self.per_program
                            .entry(stream.program_number)
                            .or_default()
                            .audio_frames += 1;
                    }
                    SamplePayload::Subtitle { .. } | SamplePayload::Unknown { .. } => {}
                }
            }
            DemuxEvent::Metadata {
                stream,
                pts,
                kind,
                payload,
            } => {
                self.record_pts(stream.pid, *pts, at);
                self.klv_records += 1;
                self.per_program
                    .entry(stream.program_number)
                    .or_default()
                    .klv_records += 1;
                if self.track_klv_digests {
                    self.klv_digests.push(to_hex(&Sha256::digest(payload)));
                }
                self.klv_carriage_seen.insert(klv_carriage_of(kind));
                if self.klv_expect.set == KlvSet::Rich {
                    // Ask BEFORE decoding whether the sender's own tap
                    // damaged this record: the three rich oracles judge
                    // what a conformant PRODUCER must emit, and bytes the
                    // tap deliberately rewrote are not the producer's
                    // doing. See `Attribution::explains_damage` for what
                    // counts as damage and why it takes `&mut self`.
                    let damaged = self
                        .attribution
                        .as_mut()
                        .is_some_and(|a| a.explains_damage(at, stream.pid));
                    // `payload` is the bare KLV LS in BOTH carriages: the
                    // demuxer peels the 5-byte Metadata_AU_cell header off
                    // a sync-carriage record before emitting it (see
                    // `MetadataKind::KlvSyncAuCell`'s doc comment, and
                    // `tests/klv_rich.rs`'s sync round trip, which is what
                    // holds that contract in place).
                    self.judge_rich(payload, damaged);
                }
            }
            DemuxEvent::Discontinuity { stream, kind } => {
                self.discontinuities += 1;
                *self.discontinuities_by_pid.entry(stream.pid).or_insert(0) += 1;
                self.first_discontinuity
                    .get_or_insert_with(|| format!("{kind:?}"));
            }
            DemuxEvent::NonConformant { stream, issue } => {
                self.nonconformant += 1;
                *self.nonconformant_by_pid.entry(stream.pid).or_insert(0) += 1;
                self.first_nonconformant
                    .get_or_insert_with(|| issue.to_string());
            }
            DemuxEvent::ReconnectDiscontinuity => {}
        }
    }

    // ------------------------------------------------------------
    // Rich-KLV decode oracles (spec §5.5). Inert unless
    // `set_klv_expect` was called with `KlvSet::Rich`.
    // ------------------------------------------------------------

    /// Judge one demuxed KLV record (bare LS bytes) against what the
    /// rich generator must have produced.
    ///
    /// Three independent questions, one pass over the record:
    /// 1. does it decode cleanly (`klv_rich_decode_clean`),
    /// 2. does its tag set equal `rich_presence(seed, seq)` for the
    ///    `seq` its own timestamp names (`klv_rich_census`),
    /// 3. and where the schedule demanded Tag 48, does the nested
    ///    ST 0102 set decode into a real security classification
    ///    (`klv_rich_security_nested`).
    ///
    /// The census is what makes this more than a decoder smoke test: a
    /// producer that dropped a whole tag group, or shipped one record's
    /// tags under another record's timestamp, still decodes cleanly.
    /// Checking against a schedule the receiver computes INDEPENDENTLY
    /// from `(seed, seq)` is what catches that.
    ///
    /// `damaged` says an injection's window covers this record (see
    /// [`corrupt::Attribution::explains_damage`]). Such a record still
    /// counts as delivered, and all three oracles skip it: they ask what a
    /// conformant producer emitted, and these bytes are not what the
    /// producer emitted. Skipping only the decode oracle would be worse
    /// than useless — damage that leaves a record decodable but a tag
    /// short would then fail the census oracle instead, which is the same
    /// false positive wearing a different name.
    fn judge_rich(&mut self, payload: &[u8], damaged: bool) {
        use tst_core::klv::{st0102, st0601};

        self.rich.records += 1;
        if damaged {
            self.rich.damaged_by_injection += 1;
            return;
        }
        let n = self.rich.records;

        let rec = match st0601::decode(payload) {
            Ok(rec) => rec,
            Err(e) => {
                self.rich.decode_errors += 1;
                self.note_rich_problem(RichOracle::Decode, format!("record {n}: {e}"));
                return;
            }
        };
        if let Some(first) = rec.field_errors.first() {
            self.rich.field_error_records += 1;
            self.note_rich_problem(
                RichOracle::Decode,
                format!(
                    "record {n}: {} field error(s), first: {first}",
                    rec.field_errors.len()
                ),
            );
        }

        // Rich timestamps are on a fixed cadence off a fixed epoch
        // precisely so a receiver can invert one back to the sender's
        // `seq` — without that there is no schedule to check against, so
        // a stamp that names no `seq` IS a census mismatch, not a reason
        // to skip the check.
        let Some(seq) = rec.timestamp_us.and_then(fixtures::rich_seq_of_timestamp) else {
            self.rich.census_mismatches += 1;
            self.note_rich_problem(
                RichOracle::Census,
                format!(
                    "record {n}: timestamp {:?} names no rich seq (missing, pre-epoch, or off the \
                     {}us grid)",
                    rec.timestamp_us,
                    fixtures::RICH_TS_STEP_US
                ),
            );
            return;
        };

        let expected = fixtures::rich_presence(self.klv_expect.seed, seq);
        let observed = fixtures::observed_tags(&rec);
        if observed != expected {
            self.rich.census_mismatches += 1;
            let missing: Vec<u32> = expected.difference(&observed).copied().collect();
            let extra: Vec<u32> = observed.difference(&expected).copied().collect();
            self.note_rich_problem(
                RichOracle::Census,
                format!(
                    "record {n} (seq {seq}): missing tags {missing:?}, unexpected tags {extra:?}"
                ),
            );
        }

        if expected.contains(&SECURITY_TAG) {
            self.rich.security_expected += 1;
            // A nested set is only "there" if it decodes into something
            // usable — a Tag 48 carrying bytes no ST 0102 decoder
            // accepts is worse than a missing one, not better, so both
            // land in the same verdict.
            let problem = match rec.security_local_set.as_deref() {
                None => Some(format!("no Tag {SECURITY_TAG} on the record")),
                Some(bytes) => match st0102::decode(bytes) {
                    Err(e) => Some(format!("nested ST 0102 set does not decode: {e}")),
                    Ok(s) => match (s.field_errors.first(), s.security_classification) {
                        (Some(first), _) => Some(format!(
                            "nested ST 0102 set carries {} field error(s), first: {first}",
                            s.field_errors.len()
                        )),
                        (None, None) => Some(
                            "nested ST 0102 set carries no security classification".to_string(),
                        ),
                        (None, Some(_)) => None,
                    },
                },
            };
            match problem {
                None => self.rich.security_ok += 1,
                Some(detail) => self.note_rich_problem(
                    RichOracle::Security,
                    format!("record {n} (seq {seq}): {detail}"),
                ),
            }
        }
    }

    /// Record `detail` as the first problem of `which` (if it is), and as
    /// the metrics' first problem of any kind (if it is).
    fn note_rich_problem(&mut self, which: RichOracle, detail: String) {
        let slot = match which {
            RichOracle::Decode => &mut self.rich_first_decode,
            RichOracle::Census => &mut self.rich_first_census,
            RichOracle::Security => &mut self.rich_first_security,
        };
        if slot.is_none() {
            *slot = Some(detail.clone());
        }
        if self.rich.first_problem.is_none() {
            self.rich.first_problem = Some(detail);
        }
    }

    // ------------------------------------------------------------
    // Corruption attribution (spec §4.3). Everything in this section
    // is inert unless `attach_attribution` was called.
    // ------------------------------------------------------------

    /// Judge this capture against a sender-side corruption log.
    ///
    /// From here on, every event fed through [`Tally::feed_at`] is also
    /// offered to `a`, and [`Tally::finish`] turns its verdict into the
    /// `corruption_attributed`/`corruption_detected`/`corruption_recovered`
    /// failures plus `CellMetrics::corruption_attribution`. Call before
    /// the first event: an attribution that missed the start of the
    /// capture would report the injections it never saw evidence for as
    /// undetected.
    pub fn attach_attribution(&mut self, a: corrupt::Attribution) {
        self.attribution = Some(a);
    }

    /// Add injections the sender logged after [`Tally::attach_attribution`]
    /// — a live receiver polls its [`corrupt::LogTail`] as the run
    /// proceeds and hands the new ones here. Inert without an attribution.
    pub fn append_injections(&mut self, injections: Vec<Injection>) {
        if let Some(a) = self.attribution.as_mut() {
            a.append(injections);
        }
    }

    /// Hand a DRAINED batch of the raw reader's sync recoveries
    /// (`rawts::Reader::take_resyncs`) to the attribution. Every element
    /// is fed; call it once per batch.
    pub fn note_resyncs(&mut self, batch: &[rawts::Resync]) {
        if let Some(a) = self.attribution.as_mut() {
            for r in batch {
                // A resync is not attributable to a PID: sync was lost for
                // the whole multiplex, not for one stream in it.
                a.on_signal(r.at_packets, None, corrupt::Signal::Resync);
            }
        }
    }

    /// Hand the raw reader's FINAL recovery — the trailing partial packet
    /// `rawts::Reader::trailing_resync` describes — to the attribution.
    /// Separate from [`Tally::note_resyncs`] because the reader's own
    /// batches deliberately never hold it: callers feed it exactly once,
    /// at the end of the capture.
    pub fn note_trailing_resync(&mut self, r: &rawts::Resync) {
        if let Some(a) = self.attribution.as_mut() {
            a.on_signal(r.at_packets, None, corrupt::Signal::Resync);
        }
    }

    /// Hand the raw reader's freshly-decoded `(pcr_base, at)` pairs to the
    /// attribution, which is how a logged coordinate becomes a receiver
    /// position at all (see `corrupt::Attribution::on_pcr`). Already
    /// drained by the caller, so unlike [`Tally::note_resyncs`] this must
    /// be called with each batch exactly once.
    pub fn note_pcrs(&mut self, pcrs: &[(u64, u64)]) {
        if let Some(a) = self.attribution.as_mut() {
            for &(base, at) in pcrs {
                a.on_pcr(base, at);
            }
        }
    }

    /// Offer one event to the attached attribution: an error-class event
    /// as a signal, a media event as evidence of recovery, anything else
    /// (a PMT, a reconnect marker) not at all.
    fn route_to_attribution(&mut self, ev: &DemuxEvent, at: u64) {
        let Some(a) = self.attribution.as_mut() else {
            return;
        };
        match ev {
            DemuxEvent::Sample { stream, .. } | DemuxEvent::Metadata { stream, .. } => {
                a.on_media(at, stream.pid);
            }
            DemuxEvent::Discontinuity { stream, kind } => {
                let sig = match kind {
                    DiscontinuityKind::ContinuityJump { .. } => corrupt::Signal::ContinuityJump,
                    _ => corrupt::Signal::OtherDiscontinuity,
                };
                a.on_signal(at, Some(stream.pid), sig);
            }
            DemuxEvent::NonConformant { stream, issue } => {
                let sig = match issue {
                    NonConformantIssue::PsiChecksumMismatch { .. } => corrupt::Signal::PsiChecksum,
                    NonConformantIssue::PcrAnomaly { .. } => corrupt::Signal::PcrAnomaly,
                    NonConformantIssue::MalformedPes { .. } | NonConformantIssue::PusiMidPes => {
                        corrupt::Signal::MalformedPes
                    }
                    _ => corrupt::Signal::OtherNonConformant,
                };
                a.on_signal(at, Some(stream.pid), sig);
            }
            DemuxEvent::ReconnectDiscontinuity => a.on_reconnect(at),
            DemuxEvent::ProgramMap(_) => {}
        }
    }

    /// Fold one sample's PTS into the per-PID monotonicity check. `at` is
    /// the receiver packet ordinal the sample surfaced at, used only to
    /// ask an attached attribution whether a TRUNCATION explains a break.
    ///
    /// Nothing else excuses one. A PTS that steps backwards is a real
    /// defect wherever it comes from, and a body flip can no longer
    /// manufacture a fake one: `Class::BodyFlip` never targets a PES
    /// header (see `corrupt.rs`'s comment on the draw range). Truncation
    /// is the single exception because it is the single class that
    /// removes a non-multiple of 188 bytes, leaving the stream misaligned
    /// until a parser re-locks — and a parser re-locking on a false 0x47
    /// inside live media reads bytes that were never a timestamp. See
    /// `Attribution::truncation_explains` for the measurement that scoped
    /// this to one class.
    fn record_pts(&mut self, pid: u16, pts: Pts90khz, at: u64) {
        let now = pts.as_ticks() as u64;
        if let Some(&last) = self.last_pts_by_pid.get(&pid) {
            if !pts_is_monotonic_step(now, last)
                && !self
                    .attribution
                    .as_mut()
                    .is_some_and(|a| a.truncation_explains(at))
            {
                self.pts_monotonic = false;
            }
        }
        self.last_pts_by_pid.insert(pid, now);
    }

    /// Fold `chunk` into the running byte count and whole-stream hash.
    /// Independent of `feed` — callers pass the exact bytes handed to the
    /// `Demuxer`, in the same order, so `stream_sha256` is a
    /// byte-transparent fingerprint of the capture regardless of how the
    /// demuxer parsed it.
    pub fn note_bytes(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        self.stream_hasher.update(chunk);
    }

    /// What the wire-vs-demux oracle may subtract from each PID's floor
    /// — see [`oracles::Explained`] for why the answer is per PID and
    /// `oracles::wire_vs_demux` for what the term means.
    ///
    /// Under `Strict` (offline `verify`, `recv --strict` on a transparent
    /// cell) only the corruption log speaks: the capture's own
    /// discontinuity/non-conformance events are failures in their own
    /// right there, not an excuse for a missing access unit.
    ///
    /// Under `Lossy` those events join in — each is a PES the demuxer
    /// legitimately abandoned — and they are the WHOLE per-PID term.
    /// Adding the attribution's per-PID counts on top would count every
    /// attributed event twice: `feed_at` routes each error-class event to
    /// the attribution and then tallies that same event here, so an
    /// attributed jump is already in `discontinuities_by_pid`.
    ///
    /// A resync is the one signal that belongs to no PID (the raw reader
    /// lost packet sync for the whole multiplex), and it is not a
    /// `DemuxEvent` either — so it reaches this only through the
    /// attribution, in both tiers.
    fn explained(&self, mode: VerifyMode, attribution: Option<&AttributionReport>) -> Explained {
        let by_pid = match mode {
            VerifyMode::Strict => attribution
                .map(|rep| rep.attributed_events_by_pid.clone())
                .unwrap_or_default(),
            VerifyMode::Lossy => {
                let mut m = self.discontinuities_by_pid.clone();
                for (&pid, &n) in &self.nonconformant_by_pid {
                    *m.entry(pid).or_insert(0) += n;
                }
                m
            }
        };
        Explained {
            by_pid,
            multiplex_wide: attribution.map_or(0, |rep| rep.attributed_events_unpinned),
        }
    }

    /// Check the tally against `p`'s invariants for a `seconds`-long
    /// capture, requiring at least `slack` (e.g. `0.7` = 70%) of each
    /// nominal per-second count. `mode` governs whether a `Discontinuity`
    /// event fails the check (`Strict`) or is merely counted (`Lossy`);
    /// `NonConformant` always fails, in either mode.
    ///
    /// `wire` is the independent [`rawts`] reader's summary of the same
    /// bytes, alongside the tst-core-demuxed `Tally` — checked by
    /// [`oracles::check`]'s seven wire-level oracles, parameterized by
    /// `p`'s [`profiles::Invariants`].
    pub fn finish(
        mut self,
        p: &Profile,
        seconds: f64,
        slack: f64,
        mode: VerifyMode,
        wire: &WireSummary,
    ) -> VerifyReport {
        let inv = profiles::invariants(p);
        let mut failures = Vec::new();

        // Corruption verdicts first: they also SCALE the count floors
        // below. A capture whose sender deliberately destroyed 0.5% of
        // its packets cannot be held to the same "70% of nominal"
        // arithmetic as a clean one — the missing media is the point of
        // the run, not a regression — so the injected fraction is
        // discounted from the slack before any floor is computed.
        // `Lossy` means the capture crossed a real impaired transport, so
        // packets go missing for reasons the sender never logged and the
        // attribution must not read those gaps as corruption findings —
        // see `Attribution::lossy`. `Strict` judges a file, where
        // there is no transport to blame and every finding stands.
        // The tier is chosen when the attribution is BUILT (the excusal
        // has to be applied as each event arrives — see
        // `Attribution::lossy`), so the only thing left to do here is
        // confirm that the capture is being judged in the tier it was
        // built for. A mismatch would mean a Lossy run judged by a strict
        // attribution, or the reverse; both are wiring mistakes, and both
        // would otherwise pass quietly.
        //
        // Recorded as a failure, never asserted: this runs at the end of
        // a capture that may have taken three days, and a panic there
        // throws away every other verdict the run earned — including the
        // evidence a reader would need to see the wiring bug for what it
        // is. Failing the report says the same thing and keeps the
        // artifact.
        if let Some(a) = self.attribution.as_ref() {
            let want_lossy = mode == VerifyMode::Lossy;
            if a.excuses_transport_loss() != want_lossy {
                failures.push(format!(
                    "corruption_tier: capture judged in {mode:?} but its attribution was built \
                     for {} — transport-loss excusal is applied as each event arrives, so the \
                     two cannot be reconciled after the fact",
                    if a.excuses_transport_loss() {
                        "Lossy"
                    } else {
                        "Strict"
                    }
                ));
            }
        }
        let attribution = self
            .attribution
            .take()
            .map(|a| a.finish(wire.packets))
            .inspect(|rep| {
                // Every count here is the UNCAPPED one. The lists beside
                // them are bounded samples, so gating on a list's length
                // would read a run with ten thousand findings as one with
                // sixty-four.
                if rep.unexplained_total() > 0 {
                    let excused = if rep.unexplained_transport_loss > 0 {
                        format!(
                            " ({} excused as transport loss)",
                            rep.unexplained_transport_loss
                        )
                    } else {
                        String::new()
                    };
                    failures.push(format!(
                        "corruption_attributed: {} unexplained event(s){excused}, first: {:?}",
                        rep.unexplained_total(),
                        rep.unexplained_events.first()
                    ));
                }
                if rep.undetected_total() > 0 {
                    failures.push(format!(
                        "corruption_detected: {} detectable injection(s) produced no event, \
                         first: {:?}",
                        rep.undetected_total(),
                        rep.undetected.first()
                    ));
                }
                if rep.unrecovered_total() > 0 {
                    failures.push(format!(
                        "corruption_recovered: {} injection(s) with no media within {} packets, \
                         first: {:?}",
                        rep.unrecovered_total(),
                        rep.recovery_bound,
                        rep.unrecovered.first()
                    ));
                }
            });
        let explained = self.explained(mode, attribution.as_ref());
        let slack = match &attribution {
            Some(rep) => slack * (1.0 - rep.injected_fraction),
            None => slack,
        };

        let min_video_aus = min_count(inv.min_video_aus_per_sec, seconds, slack);
        if self.video_aus < min_video_aus {
            failures.push(format!(
                "video AUs: got {}, want >= {min_video_aus} ({} fps x {seconds}s x {:.0}% slack)",
                self.video_aus,
                inv.min_video_aus_per_sec,
                slack * 100.0
            ));
        }
        if self.video_aus > 0 && self.keyframes == 0 {
            failures.push("no keyframes observed among the video AUs".to_string());
        }
        if self.video_aus > 0 {
            let expected_codec = expected_demux_video_codec(p.video);
            if self.video_codecs_seen != HashSet::from([expected_codec]) {
                failures.push(format!(
                    "video codec: expected {expected_codec:?}, observed {:?}",
                    self.video_codecs_seen
                ));
            }
        }

        let min_klv_records = min_count(inv.min_klv_per_sec, seconds, slack);
        if self.klv_records < min_klv_records {
            failures.push(format!(
                "KLV records: got {}, want >= {min_klv_records} ({} Hz x {seconds}s x {:.0}% slack)",
                self.klv_records, inv.min_klv_per_sec, slack * 100.0
            ));
        }
        if self.klv_records > 0 {
            let expected_carriage = expected_klv_carriage(p.klv);
            if self.klv_carriage_seen != HashSet::from([expected_carriage]) {
                failures.push(format!(
                    "KLV carriage: expected {expected_carriage:?}, observed {:?}",
                    self.klv_carriage_seen
                ));
            }
        }

        // Rich-KLV verdicts (spec §5.5) — see `Tally::judge_rich`.
        //
        // Gated on the MODE, not on the counters being zero. The counters
        // can only move in rich mode, so the two conditions agree for any
        // caller that honours `set_klv_expect`'s "call before the first
        // event" contract — but the metrics block below keys off the mode,
        // and a report carrying a `klv_rich_*` failure with no
        // `metrics.klv_rich` beside it would be unreadable. One condition
        // decides both.
        //
        // Every rich failure says how many records the oracles never
        // judged, so a reader can tell "3 of 6000 records are wrong" from
        // "3 are wrong and 200 more were never examined".
        if self.klv_expect.set == KlvSet::Rich {
            let skipped = self.rich.damaged_by_injection;
            let damaged_note = if skipped > 0 {
                format!(" ({skipped} record(s) skipped as injection-damaged)")
            } else {
                String::new()
            };
            let unclean = self.rich.decode_errors + self.rich.field_error_records;
            if unclean > 0 {
                failures.push(format!(
                    "klv_rich_decode_clean: {unclean} record(s) failed to decode or carried field \
                     errors{damaged_note}, first: {}",
                    self.rich_first_decode.as_deref().unwrap_or("?")
                ));
            }
            if self.rich.census_mismatches > 0 {
                failures.push(format!(
                    "klv_rich_census: {} record(s) whose tag set != rich_presence(seed, \
                     seq){damaged_note}, first: {}",
                    self.rich.census_mismatches,
                    self.rich_first_census.as_deref().unwrap_or("?")
                ));
            }
            let security_bad = self
                .rich
                .security_expected
                .saturating_sub(self.rich.security_ok);
            if security_bad > 0 {
                failures.push(format!(
                    "klv_rich_security_nested: {security_bad} record(s) expected a valid ST 0102 \
                     set{damaged_note}, first: {}",
                    self.rich_first_security.as_deref().unwrap_or("?")
                ));
            }
        }

        if inv.audio_expected && self.audio_frames == 0 {
            failures.push("expected audio frames, got none".to_string());
        } else if !inv.audio_expected && self.audio_frames > 0 {
            failures.push(format!(
                "unexpected audio frames: got {}, profile carries no audio",
                self.audio_frames
            ));
        }

        let programs_seen = self.programs_seen.len() as u8;
        if programs_seen != inv.program_count {
            failures.push(format!(
                "programs seen: got {programs_seen}, want {}",
                inv.program_count
            ));
        }

        if !self.pts_monotonic {
            failures
                .push("PTS non-monotonic on at least one PID (rollover-aware check)".to_string());
        }

        if inv.expect_misp_sei && !self.misp_sei_seen {
            failures.push("expected a MISP ST 0604 SEI timestamp, none observed".to_string());
        }

        // Events a corruption log EXPLAINS are subtracted before the
        // existing fatality rules apply: the deliberate PSI flip a
        // `corruption_attributed` verdict already accounts for must not
        // also fail the report as a library non-conformance, or no run
        // with the tap enabled could ever pass. Everything the log does
        // not explain keeps failing exactly as before (`saturating_sub`
        // because the two counts come from independent paths — an
        // attribution driven directly in a unit test can legitimately
        // hold more attributed events than this tally ever saw).
        let (attributed_nc, attributed_disc, pcr_excused) = match &attribution {
            Some(rep) => (
                rep.attributed_nonconformant,
                rep.attributed_discontinuities,
                rep.pcr_anomalies_excused,
            ),
            None => (0, 0, 0),
        };
        // A PCR jump the attribution excused as a continuity gap's
        // timestamp signature is UNATTRIBUTED but not a finding, so it
        // has to come off here too — otherwise a lossy leg passes
        // `corruption_attributed` and fails `nonconformant_event` for the
        // very same event. `metrics.nonconformant` itself stays raw: it
        // is documented as always counted. Zero under Strict, which
        // excuses nothing, so this needs no mode branch.
        let unexplained_nc = self
            .nonconformant
            .saturating_sub(attributed_nc)
            .saturating_sub(pcr_excused);
        let unexplained_disc = self.discontinuities.saturating_sub(attributed_disc);
        // And the event these failures QUOTE must be an unexplained one.
        // `first_nonconformant`/`first_discontinuity` record the first
        // event of their class whether or not the log explains it, so on
        // an attributed run they routinely name an event the report has
        // already accounted for — pointing a reader at the wrong evidence
        // while the count says something is still wrong. The attribution
        // records the first UNEXPLAINED event of each family for exactly
        // this; the tally's own field is the fallback when there is no
        // attribution (or, defensively, when the two disagree).
        let first_unexplained = |from_attribution: Option<&String>, from_tally: Option<&str>| {
            from_attribution
                .map(String::as_str)
                .or(from_tally)
                .unwrap_or("?")
                .to_string()
        };
        if unexplained_nc > 0 {
            failures.push(format!(
                "nonconformant_event: {} event(s), first: {}",
                unexplained_nc,
                first_unexplained(
                    attribution
                        .as_ref()
                        .and_then(|r| r.first_unexplained_nonconformant.as_ref()),
                    self.first_nonconformant.as_deref(),
                )
            ));
        }
        if mode == VerifyMode::Strict && unexplained_disc > 0 {
            failures.push(format!(
                "discontinuity_event: {} event(s), first: {}",
                unexplained_disc,
                first_unexplained(
                    attribution
                        .as_ref()
                        .and_then(|r| r.first_unexplained_discontinuity.as_ref()),
                    self.first_discontinuity.as_deref(),
                )
            ));
        }

        failures.extend(oracles::check(
            p,
            &inv,
            wire,
            &self.per_program,
            seconds,
            slack,
            mode,
            &explained,
        ));

        // A compact capture has no presence schedule to judge, so it
        // carries no rich block at all — `Some(..)` is the report's own
        // record that the rich oracles ran over these records.
        let klv_rich = match self.klv_expect.set {
            KlvSet::Rich => Some(std::mem::take(&mut self.rich)),
            KlvSet::Compact => None,
        };

        let metrics = CellMetrics {
            video_aus: self.video_aus,
            keyframes: self.keyframes,
            klv_records: self.klv_records,
            klv_set_sha256: self
                .track_klv_digests
                .then(|| klv_set_hash(&self.klv_digests)),
            audio_frames: self.audio_frames,
            programs_seen,
            pts_monotonic: self.pts_monotonic,
            misp_sei_seen: self.misp_sei_seen,
            bytes: self.bytes,
            stream_sha256: to_hex(&self.stream_hasher.finalize()),
            discontinuities: self.discontinuities,
            nonconformant: self.nonconformant,
            // A verifier never runs the tap; it only judges its log.
            corruption: None,
            corruption_attribution: attribution,
            klv_rich,
        };

        VerifyReport {
            pass: failures.is_empty(),
            failures,
            metrics,
            // `Tally` is transport-agnostic (fed events, not a live
            // transport) and has no notion of reconnects — `recv.rs`'s
            // `run_managed` overwrites this with `Some(n)` afterward,
            // the same way it patches `metrics.bytes`/`stream_sha256`
            // in from the `Teeing` tap post-hoc.
            reconnects: None,
            // Same post-hoc shape: `recv` stamps its `--expect` here, an
            // offline `verify` leaves it unset (see the field's own doc).
            profile: None,
        }
    }
}

/// Demux `bytes` and check them against `p`'s invariants for a
/// `seconds`-long capture under `mode`, without touching the filesystem.
///
/// Never fails via `Result` — mutation tests deliberately feed corrupted
/// bytes (see `tests/mutations.rs`), and a `Result`-returning verifier
/// would be awkward for that call shape: every mutation-test call site
/// would need to unwrap/expect on a value that's *expected* to report
/// failure, not to error. A `Demuxer`/`rawts::Reader` feed error
/// (`DemuxError::Unrecoverable` and friends — see
/// `tst_core::error::DemuxError`) instead turns into a `pass: false`
/// report carrying a `demux_error`/`rawts_sync_loss` failure string,
/// alongside whatever `Tally`/`WireSummary` state accumulated before the
/// error. `verify_file` is the thin file-reading wrapper around this.
pub fn verify_bytes_with_mode(
    bytes: &[u8],
    p: &Profile,
    seconds: f64,
    mode: VerifyMode,
) -> VerifyReport {
    verify_bytes_with_corruption(bytes, p, seconds, mode, KlvExpect::compact(), None)
}

/// How many bytes of a capture [`verify_bytes_with_corruption`] hands to
/// the demuxer at a time. 1316 = 7 × 188, the classic UDP/SRT transport
/// payload — the same granularity a live receiver's coordinates are
/// stamped at, so an offline judgement of a captured file and a live
/// judgement of the same stream place their events identically rather
/// than differing by whatever chunking the file reader happened to use.
const VERIFY_CHUNK: usize = 7 * 188;

/// [`verify_bytes_with_mode`], additionally judging the capture against a
/// sender-side corruption log (`log`, as returned by
/// [`corrupt::read_log`]).
///
/// With a log, two things change. The wire reader runs in sync-recovery
/// mode — deliberately destroyed packets would otherwise latch a
/// `rawts_sync_loss` failure that says nothing more than "the tap did its
/// job" — and every demuxed event is stamped with the reader's packet
/// ordinal, which is what lets an error event be matched to the injection
/// that caused it (see [`Tally::attach_attribution`] and `corrupt`'s
/// module doc). Without one, this is byte-for-byte the old behaviour: the
/// chunking below changes nothing a `Demuxer` observes, since it buffers
/// across feeds.
///
/// `klv` says which KLV record set the capture was generated with — see
/// [`KlvExpect`]. [`KlvExpect::compact`] leaves the rich oracles off,
/// which is every caller that has not been told otherwise.
pub fn verify_bytes_with_corruption(
    bytes: &[u8],
    p: &Profile,
    seconds: f64,
    mode: VerifyMode,
    klv: KlvExpect,
    log: Option<&(LogHeader, Vec<Injection>)>,
) -> VerifyReport {
    // Built per-profile (never `Demuxer::new()`/`DemuxerConfig::default()`)
    // — see `profiles::demuxer_config`'s doc comment for the av1-klv-a
    // finding this closes.
    let mut demux = Demuxer::with_config(profiles::demuxer_config(p));
    let mut tally = Tally::new();
    tally.set_klv_expect(klv);
    // Independent wire-level reader, fed the exact same bytes as the
    // demuxer — see `rawts`'s module doc for why it shares no code with
    // `Demuxer`.
    let mut wire_reader = rawts::Reader::new();
    if let Some((hdr, injections)) = log {
        // Built for the tier this capture is judged in — the excusal is
        // applied per event, not at `finish` (see `Attribution::lossy`).
        tally.attach_attribution(match mode {
            VerifyMode::Strict => corrupt::Attribution::strict(injections.clone(), hdr),
            VerifyMode::Lossy => corrupt::Attribution::lossy(injections.clone(), hdr),
        });
        wire_reader.set_resync_mode(true);
    }

    tally.note_bytes(bytes);
    let mut demux_err = None;
    let mut wire_err = None;
    for chunk in bytes.chunks(VERIFY_CHUNK) {
        if demux_err.is_none() {
            demux_err = demux.feed(chunk).err();
        }
        if wire_err.is_none() {
            wire_err = wire_reader.feed(chunk).err();
        }
        // Drain as we go, so each event carries the reader's position at
        // the time it surfaced rather than the position at end-of-file.
        tally.note_pcrs(&wire_reader.take_pcr_events());
        tally.note_resyncs(&wire_reader.take_resyncs());
        while let Some(ev) = demux.next_event() {
            tally.feed_at(&ev, wire_reader.packets());
        }
    }

    // Canonical end-of-stream signal — see `demux_to_events.rs`'s doc
    // comment: without this the last access unit of every stream is left
    // sitting in the reassembler and silently dropped. Drained even after
    // a feed error above: whatever the demuxer managed to reassemble
    // before losing sync is still real signal for the tally.
    demux.flush();
    while let Some(ev) = demux.next_event() {
        tally.feed_at(&ev, wire_reader.packets());
    }
    tally.note_resyncs(&wire_reader.take_resyncs());
    // A trailing partial packet is a recovery event too, and the only
    // one `take_resyncs()` never holds — see `Reader::trailing_resync`.
    if let Some(t) = wire_reader.trailing_resync() {
        tally.note_trailing_resync(&t);
    }

    let (wire, trailing_bytes_err) = match wire_reader.finish() {
        Ok(w) => (w, None),
        Err(e) => (WireSummary::default(), Some(e)),
    };
    let mut report = tally.finish(p, seconds, NOMINAL_COUNT_SLACK, mode, &wire);
    if let Some(e) = demux_err {
        report.pass = false;
        report.failures.push(format!("demux_error: {e}"));
    }
    if let Some(e) = wire_err {
        report.pass = false;
        report.failures.push(format!("rawts_sync_loss: {e}"));
    }
    if let Some(e) = trailing_bytes_err {
        report.pass = false;
        report.failures.push(format!("rawts_trailing_bytes: {e}"));
    }
    report
}

/// Demux `path` and check it against `p`'s invariants for a
/// `seconds`-long capture. Thin wrapper: reads the whole file, then
/// delegates to [`verify_bytes_with_mode`] in [`VerifyMode::Strict`]. The
/// `io::Result` here covers only the file read itself — a demux/wire-
/// reader error surfaces inside the returned `VerifyReport` instead (see
/// `verify_bytes_with_mode`'s doc comment).
pub fn verify_file(path: &Path, p: &Profile, seconds: f64) -> io::Result<VerifyReport> {
    verify_file_with(path, p, seconds, KlvExpect::compact())
}

/// [`verify_file`], for a capture generated with a non-default KLV record
/// set — see [`KlvExpect`]. Same `VerifyMode::Strict` offline check;
/// `verify_file` is this with [`KlvExpect::compact`].
pub fn verify_file_with(
    path: &Path,
    p: &Profile,
    seconds: f64,
    klv: KlvExpect,
) -> io::Result<VerifyReport> {
    let bytes = std::fs::read(path)?;
    Ok(verify_bytes_with_corruption(
        &bytes,
        p,
        seconds,
        VerifyMode::Strict,
        klv,
        None,
    ))
}

/// Entry points into the verification core that exist only so tests can
/// reach a judgement no real capture can be made to produce. Not part of
/// this crate's CLI surface; nothing outside a test calls any of it.
#[doc(hidden)]
pub mod testing {
    use super::*;
    use tst_core::mpegts::demux::{StreamId, StreamKind};

    /// PTS step between synthetic records — 10 Hz, the rich generator's
    /// own cadence. Any monotonic step would do; matching the generator
    /// keeps a reader from wondering whether the value is load-bearing.
    const KLV_STEP_TICKS: i64 = 9_000;

    /// A bare-LS (async-carriage) KLV `Metadata` event on `pid` carrying
    /// `payload`, stamped at `pts_ticks` in program 1.
    pub fn klv_event_on(pid: u16, pts_ticks: i64, payload: Vec<u8>) -> DemuxEvent {
        DemuxEvent::Metadata {
            stream: StreamId {
                pid,
                kind: StreamKind::KlvAsync,
                program_number: 1,
            },
            pts: Pts90khz::new(pts_ticks),
            kind: MetadataKind::KlvAsync,
            payload,
        }
    }

    /// Judge `records` (raw ST 0601 LS bytes, exactly as a demuxer hands
    /// them over) against `expect`'s rich oracles, with no capture
    /// involved.
    ///
    /// **Only the three `klv_rich_*` verdicts are meaningful here, and
    /// only they can fail.** The synthetic events carry no video, no
    /// audio, no PMT and no wire bytes, so every other check
    /// [`Tally::finish`] runs would fail for reasons that have nothing to
    /// do with the records under test; those failures are dropped and
    /// `pass` is recomputed from what survives. Feeding a fabricated
    /// `WireSummary` instead would mean inventing a whole conformant
    /// capture to get the same three answers.
    ///
    /// This exists because two of the three oracles cannot be provoked by
    /// editing a muxed capture: re-encoding one record changes its
    /// length, so PES lengths, continuity counters and the PCR schedule
    /// all move with it, and the result is no longer the stream whose
    /// records were under test. Mutations that CAN be made on the wire
    /// (a flipped payload byte, a mismatched seed) go through the real
    /// path in `tests/klv_rich.rs`, not through here.
    pub fn judge_records(records: &[Vec<u8>], expect: KlvExpect) -> VerifyReport {
        const PID: u16 = 0x0101;

        let p = profiles::by_name("baseline").expect("the baseline profile must exist");
        let mut tally = Tally::new();
        tally.set_klv_expect(expect);
        for (i, record) in records.iter().enumerate() {
            tally.feed(&klv_event_on(
                PID,
                i as i64 * KLV_STEP_TICKS,
                record.clone(),
            ));
        }
        // 0.1s so the count floors round down to zero — belt and braces
        // next to the filter below, which is what actually guarantees a
        // caller sees nothing but rich verdicts.
        let mut report = tally.finish(
            p,
            0.1,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Strict,
            &WireSummary::default(),
        );
        report.failures.retain(|f| f.starts_with("klv_rich_"));
        report.pass = report.failures.is_empty();
        report
    }
}

#[cfg(test)]
mod tests {
    /// A `klv_rich_*` failure with no `metrics.klv_rich` block beside it
    /// would be unreadable, so `finish()` keys both off ONE condition —
    /// the mode, not whether the counters happen to be non-zero.
    ///
    /// Pinned through the only sequence that can make the two disagree:
    /// judge a bad rich record, then set the expectation back to compact
    /// before finishing. `set_klv_expect` documents against that call
    /// order and nothing in-tree does it; the point is that the report
    /// stays self-consistent even when a caller gets it wrong. The
    /// counter assert is what keeps the test honest — without it, this
    /// would pass just as well if the record had been judged as nothing
    /// at all.
    #[test]
    fn a_compact_finish_emits_no_rich_verdict_even_with_rich_counters_set() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let mut t = Tally::new();
        t.set_klv_expect(KlvExpect {
            set: KlvSet::Rich,
            seed: 5,
        });
        // Eight zero bytes are not an ST 0601 record: no UL, no checksum.
        t.feed(&testing::klv_event_on(KLV_PID, 0, vec![0u8; 8]));
        assert_eq!(t.rich.decode_errors, 1, "the record must have been judged");

        t.set_klv_expect(KlvExpect::compact());
        let report = t.finish(
            p,
            0.1,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Strict,
            &WireSummary::default(),
        );

        assert!(
            !report.failures.iter().any(|f| f.starts_with("klv_rich_")),
            "compact finish must emit no rich verdict: {:?}",
            report.failures
        );
        assert!(
            report.metrics.klv_rich.is_none(),
            "and no rich metrics block either: {:?}",
            report.metrics.klv_rich
        );
    }

    /// The other half of the pairing above: in rich mode the same bad
    /// record produces BOTH the verdict and the metrics block.
    #[test]
    fn a_rich_finish_emits_the_verdict_and_the_metrics_together() {
        let r = testing::judge_records(
            &[vec![0u8; 8]],
            KlvExpect {
                set: KlvSet::Rich,
                seed: 5,
            },
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("klv_rich_decode_clean")),
            "{:?}",
            r.failures
        );
        assert_eq!(
            r.metrics
                .klv_rich
                .as_ref()
                .map(|m| m.decode_errors)
                .unwrap_or_default(),
            1,
            "the metrics block must be there and must count the record"
        );
    }

    use super::*;
    use crate::fixtures;
    use tst_core::mpegts::au_cell::CellFragmentIndication;
    use tst_core::mpegts::demux::{
        DiscontinuityKind, MetadataKind, NonConformantIssue, ProgramMap, StreamId, StreamKind,
        VideoCodec,
    };
    use tst_core::shared::SharedBytes;

    const VIDEO_PID: u16 = 0x0100;
    const KLV_PID: u16 = 0x0101;
    const PROGRAM: u16 = 1;
    const FPS_STEP_TICKS: i64 = 3_000; // 90_000 / 30 fps
    const KLV_STEP_TICKS: i64 = 9_000; // 90_000 / 10 Hz

    /// A real `WireSummary` for profile `name`/`seconds`, built via
    /// `gen::run` and `rawts::summarize_file`. The `Tally`-level tests
    /// below feed hand-built `DemuxEvent`s (deliberately not a real
    /// captured stream: custom PIDs, injected discontinuities/
    /// mismatches) into a `Tally`, but `Tally::finish` also runs
    /// `oracles::check` against the `WireSummary` passed in — an empty
    /// `WireSummary::default()` would trip the wire-level oracles (e.g.
    /// a missing-PMT failure) regardless of what a given test actually
    /// means to exercise, so this builds a genuinely conformant capture
    /// for the test's profile/duration instead.
    fn wire_for(name: &str, seconds: f64) -> WireSummary {
        let p = profiles::by_name(name).unwrap_or_else(|| panic!("profile {name} must exist"));
        // Six call sites share this helper and two use a different
        // `seconds` for the same profile name — naming by profile + pid
        // alone collided under plain `cargo test` (single process, all
        // tests share one pid). Profile + seconds + pid + wall-clock
        // nanos (report.rs's tempdir-test convention) makes every call a
        // distinct file regardless of process model.
        let path = std::env::temp_dir().join(format!(
            "tst-interop-verify-wire-{name}-{seconds}-{}-{}.ts",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        crate::r#gen::run(
            p,
            seconds,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .expect("gen::run must succeed");
        let wire = rawts::summarize_file(&path).expect("summarize_file must succeed");
        let _ = std::fs::remove_file(&path);
        wire
    }

    fn program_map_event() -> DemuxEvent {
        DemuxEvent::ProgramMap(ProgramMap {
            program_number: PROGRAM,
            pcr_pid: VIDEO_PID,
            pmt_pid: 0x1000,
            streams: Vec::new(),
            klv_links: Vec::new(),
        })
    }

    fn video_event_on(pid: u16, codec: VideoCodec, pts_ticks: i64, keyframe: bool) -> DemuxEvent {
        DemuxEvent::Sample {
            stream: StreamId {
                pid,
                kind: StreamKind::Video(codec),
                program_number: PROGRAM,
            },
            pts: Pts90khz::new(pts_ticks),
            dts: None,
            payload: SamplePayload::Video {
                codec,
                raw: SharedBytes::from(vec![0xA5u8; 8]),
                random_access_indicator: keyframe,
                av1_carriage: None,
            },
        }
    }

    fn video_event(pts_ticks: i64, keyframe: bool) -> DemuxEvent {
        video_event_on(VIDEO_PID, VideoCodec::H264, pts_ticks, keyframe)
    }

    fn klv_event_on(pid: u16, pts_ticks: i64, seq: u32) -> DemuxEvent {
        // Same builder `testing::judge_records` feeds — one definition of
        // "an async-carriage KLV event", so a change to that shape can't
        // leave the two halves of this module disagreeing.
        testing::klv_event_on(pid, pts_ticks, fixtures::klv_record(seq))
    }

    fn klv_event(pts_ticks: i64, seq: u32) -> DemuxEvent {
        klv_event_on(KLV_PID, pts_ticks, seq)
    }

    /// A sync-carriage (AU-cell-wrapped) KLV `Metadata` event — the shape
    /// `klv-sync` profiles carry, as opposed to `klv_event`'s bare-LS async
    /// shape.
    fn klv_sync_event_on(pid: u16, pts_ticks: i64, seq: u32) -> DemuxEvent {
        DemuxEvent::Metadata {
            stream: StreamId {
                pid,
                kind: StreamKind::KlvSync {
                    declared_link: None,
                },
                program_number: PROGRAM,
            },
            pts: Pts90khz::new(pts_ticks),
            kind: MetadataKind::KlvSyncAuCell {
                metadata_service_id: 0,
                sequence_number: (seq % 256) as u8,
                cell_fragment_indication: CellFragmentIndication::Complete,
                decoder_config_flag: false,
                random_access_indicator: true,
                was_reassembled: false,
                cell_count: 1,
            },
            payload: fixtures::klv_record(seq),
        }
    }

    fn klv_sync_event(pts_ticks: i64, seq: u32) -> DemuxEvent {
        klv_sync_event_on(KLV_PID, pts_ticks, seq)
    }

    /// Feed 2 seconds of baseline-shaped traffic (60 video AUs @ 30fps, 20
    /// KLV records @ 10Hz, one program) into `t`.
    fn feed_two_seconds_baseline(t: &mut Tally) {
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event(i as i64 * FPS_STEP_TICKS, i % 30 == 0));
        }
        for i in 0..20u32 {
            t.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i));
        }
    }

    /// A `Tally` fed 3 seconds of clean baseline-shaped traffic (90 video
    /// AUs @ 30fps with a keyframe on the first, 30 KLV records @ 10Hz,
    /// one program) — passes `baseline`'s invariants outright, with zero
    /// `Discontinuity`/`NonConformant` events, so a test can `feed` one
    /// more event on top and attribute any resulting failure to exactly
    /// that event.
    fn healthy_baseline_tally() -> Tally {
        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..90u32 {
            t.feed(&video_event(i as i64 * FPS_STEP_TICKS, i == 0));
        }
        for i in 0..30u32 {
            t.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i));
        }
        t
    }

    fn nonconformant_event() -> DemuxEvent {
        DemuxEvent::NonConformant {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: PROGRAM,
            },
            issue: NonConformantIssue::Av1WrongStreamId {
                pid: VIDEO_PID,
                observed: 0xE0,
            },
        }
    }

    /// A PCR jump on the video PID — `Signal::PcrAnomaly`, the one
    /// non-conformance lossy judgement can excuse (beside a gap).
    fn pcr_anomaly_event() -> DemuxEvent {
        DemuxEvent::NonConformant {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: PROGRAM,
            },
            issue: NonConformantIssue::PcrAnomaly { delta: 54_000_000 },
        }
    }

    fn discontinuity_event() -> DemuxEvent {
        DemuxEvent::Discontinuity {
            stream: StreamId {
                pid: VIDEO_PID,
                kind: StreamKind::Video(VideoCodec::H264),
                program_number: PROGRAM,
            },
            kind: DiscontinuityKind::ContinuityJump {
                expected: 3,
                observed: 5,
            },
        }
    }

    /// A malformed-PES non-conformance on the KLV PID — the
    /// `Signal::MalformedPes` family, so unlike a continuity jump it is
    /// never written off as transport loss.
    fn klv_nonconformant_event() -> DemuxEvent {
        DemuxEvent::NonConformant {
            stream: StreamId {
                pid: KLV_PID,
                kind: StreamKind::KlvAsync,
                program_number: PROGRAM,
            },
            issue: NonConformantIssue::MalformedPes {
                pid: KLV_PID,
                reason: "PES packet length overruns the payload",
            },
        }
    }

    /// A 2-second `baseline` capture is ~120 TS packets (compact AU
    /// fixtures — see `rawts`'s own tests), so the production
    /// `RECOVERY_BOUND` of 600 packets would run past the end of every
    /// capture in this module and `Attribution::finish` would (correctly)
    /// decline to judge recovery at all. The bound is a header field, not
    /// a constant, precisely so a judge reads the value the tap recorded;
    /// these tests use a short one so the recovery verdict is exercised.
    const TEST_RECOVERY_BOUND: u64 = 50;

    /// An attribution built for `mode`'s tier — the pairing
    /// `Tally::finish` asserts, since a lossy capture's transport-loss
    /// excusal is applied as each event arrives rather than at the end.
    fn attribution_for(
        mode: VerifyMode,
        log: Vec<crate::corrupt::Injection>,
        hdr: &crate::corrupt::LogHeader,
    ) -> crate::corrupt::Attribution {
        match mode {
            VerifyMode::Strict => crate::corrupt::Attribution::strict(log, hdr),
            VerifyMode::Lossy => crate::corrupt::Attribution::lossy(log, hdr),
        }
    }

    fn corruption_header() -> crate::corrupt::LogHeader {
        crate::corrupt::LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: crate::corrupt::Class::ALL.to_vec(),
            attribution_window: crate::corrupt::ATTRIBUTION_WINDOW,
            recovery_bound: TEST_RECOVERY_BOUND,
        }
    }

    /// One logged injection of `class` on `pid`, landing `since_pcr`
    /// packets into the stream (before its first PCR, so it resolves at
    /// construction against receiver ordinal `since_pcr` — no `on_pcr`
    /// call needed to place it).
    fn injection_at(
        class: crate::corrupt::Class,
        pid: u16,
        since_pcr: u64,
    ) -> crate::corrupt::Injection {
        crate::corrupt::Injection {
            ordinal: 0,
            coord: crate::corrupt::Coord {
                pcr_base: None,
                since_pcr,
            },
            class,
            pid,
            offsets: vec![0],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        }
    }

    /// A PTS that steps backwards fails the capture — unless a
    /// TRUNCATION explains it. Truncation drops a non-multiple of 188
    /// bytes, so a parser re-locking afterwards can read live media as a
    /// PES header and produce a "PTS" that was never a timestamp. No
    /// other class misaligns the stream, so no other class excuses one,
    /// and a capture with no corruption log at all is unchanged.
    #[test]
    fn a_backwards_pts_step_is_excused_only_by_a_truncation() {
        use crate::corrupt::{Attribution, Class};
        let wire = wire_for("baseline", 3.0);
        let p = profiles::by_name("baseline").unwrap();

        // 3 s of clean baseline traffic at receiver ordinal 0, then one
        // video AU whose PTS goes backwards, at ordinal `break_at`.
        let run = |attribution: Option<Attribution>, break_at: u64| {
            let mut t = Tally::new();
            if let Some(a) = attribution {
                t.attach_attribution(a);
            }
            t.feed_at(&program_map_event(), 0);
            for i in 0..90u32 {
                t.feed_at(&video_event(i as i64 * FPS_STEP_TICKS, i == 0), 0);
            }
            for i in 0..30u32 {
                t.feed_at(&klv_event(i as i64 * KLV_STEP_TICKS, i), 0);
            }
            t.feed_at(&video_event(0, false), break_at);
            t.finish(p, 3.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire)
        };
        let pts_failed = |r: &VerifyReport| {
            r.failures
                .iter()
                .any(|f| f.starts_with("PTS non-monotonic"))
        };
        let logged = |class: Class| {
            let mut i = injection_at(class, VIDEO_PID, 0);
            i.detectable = false;
            Attribution::lossy(vec![i], &corruption_header())
        };

        // No corruption log: unchanged, the break fails the capture.
        assert!(pts_failed(&run(None, 10)));

        // A truncation resolving at receiver ordinal 0 explains a break
        // 10 packets later.
        let r = run(Some(logged(Class::Truncate)), 10);
        assert!(
            r.failures.is_empty(),
            "an explained break fails nothing: {:?}",
            r.failures
        );

        // Past its window the same break fails again.
        assert!(pts_failed(&run(
            Some(logged(Class::Truncate)),
            crate::corrupt::ATTRIBUTION_WINDOW + 1
        )));

        // And a body flip in the same position does NOT excuse it: the
        // flip cannot reach a PES header, so a backwards PTS inside its
        // window is a real defect.
        assert!(pts_failed(&run(Some(logged(Class::BodyFlip)), 10)));
    }

    #[test]
    fn tally_with_attribution_reports_the_three_verdict_strings() {
        use crate::corrupt::{Attribution, Class, Signal};
        let hdr = corruption_header();
        let inj = injection_at(Class::Header, VIDEO_PID, 3);
        let wire = wire_for("baseline", 2.0);

        // (a) explained + detected + recovered -> no corruption failures.
        let mut t = healthy_baseline_tally();
        let mut a = Attribution::lossy(vec![inj.clone()], &hdr);
        a.on_signal(10, Some(VIDEO_PID), Signal::ContinuityJump);
        a.on_media(50, VIDEO_PID);
        t.attach_attribution(a);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert!(
            !r.failures.iter().any(|f| f.starts_with("corruption_")),
            "{:?}",
            r.failures
        );
        assert!(r.metrics.corruption_attribution.is_some());

        // (b) an unexplained NON-CONFORMANCE -> corruption_attributed.
        // Deliberately not a discontinuity: in Lossy an unexplained
        // continuity jump is transport loss, not corruption evidence (see
        // the Strict/Lossy pair below). A bad CRC or a malformed PES
        // header is something no amount of packet loss can forge, so it
        // still fails here.
        let unexplained = |sig, mode| {
            let mut t = healthy_baseline_tally();
            let mut a = attribution_for(mode, vec![inj.clone()], &hdr);
            a.on_signal(10, Some(VIDEO_PID), Signal::ContinuityJump);
            a.on_media(50, VIDEO_PID);
            a.on_signal(5000, Some(VIDEO_PID), sig);
            t.attach_attribution(a);
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                2.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            r.failures
                .iter()
                .any(|f| f.starts_with("corruption_attributed"))
        };
        assert!(unexplained(Signal::OtherNonConformant, VerifyMode::Lossy));

        // And the discontinuity half of the same verdict: fatal in
        // Strict, where there is no transport to have lost the packet,
        // and excused in Lossy, where there is.
        assert!(unexplained(Signal::ContinuityJump, VerifyMode::Strict));
        assert!(!unexplained(Signal::ContinuityJump, VerifyMode::Lossy));

        // (c) no signal at all -> corruption_detected; no media ->
        // corruption_recovered.
        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::lossy(vec![inj], &hdr));
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("corruption_detected")),
            "{:?}",
            r.failures
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("corruption_recovered")),
            "{:?}",
            r.failures
        );
    }

    /// The Tally routes a `PcrAnomaly` as its own signal, in the order the
    /// demuxer emits it (jump first, gap second, same packet), and the
    /// tier decides: excused in Lossy, charged in Strict.
    #[test]
    fn a_pcr_jump_beside_a_gap_is_excused_in_lossy_and_charged_in_strict() {
        use crate::corrupt::Class;
        let hdr = corruption_header();
        let wire = wire_for("baseline", 2.0);
        let run = |mode: VerifyMode| {
            let mut t = healthy_baseline_tally();
            t.attach_attribution(attribution_for(
                mode,
                vec![injection_at(Class::Drop, KLV_PID, 0)],
                &hdr,
            ));
            t.feed_at(&pcr_anomaly_event(), 30);
            t.feed_at(&discontinuity_event(), 30);
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                2.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            let a = r
                .metrics
                .corruption_attribution
                .clone()
                .expect("attribution");
            (r, a)
        };
        let (r, a) = run(VerifyMode::Lossy);
        assert_eq!(a.pcr_anomalies_excused, 1, "{a:?}");
        assert!(
            !r.failures
                .iter()
                .any(|f| f.starts_with("corruption_attributed")),
            "{:?}",
            r.failures
        );
        // The OUTCOME, not just the mechanism: an excused anomaly is still
        // a `DemuxEvent::NonConformant` in the tally's own raw count, so
        // without the subtraction at `unexplained_nc` the leg would pass
        // `corruption_attributed` and fail `recv_invariants` instead.
        assert!(
            !r.failures
                .iter()
                .any(|f| f.starts_with("nonconformant_event")),
            "{:?}",
            r.failures
        );
        let (r, a) = run(VerifyMode::Strict);
        assert_eq!(a.pcr_anomalies_excused, 0, "{a:?}");
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("corruption_attributed")),
            "{:?}",
            r.failures
        );
    }

    #[test]
    fn feed_at_routes_error_events_and_media_into_the_attribution() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let inj = injection_at(Class::Drop, VIDEO_PID, 0);
        let mut t = Tally::new();
        t.attach_attribution(Attribution::lossy(vec![inj], &hdr));
        t.feed_at(&program_map_event(), 1);
        t.feed_at(&discontinuity_event(), 5); // ContinuityJump on VIDEO_PID
        t.feed_at(&video_event(3000, true), 40);
        feed_two_seconds_baseline(&mut t);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );
        let a = r.metrics.corruption_attribution.unwrap();
        assert_eq!(a.attributed_events, 1);
        assert_eq!(a.attributed_discontinuities, 1);
        assert!(a.undetected.is_empty() && a.unrecovered.is_empty(), "{a:?}");
        // The discontinuity the log explains must not ALSO fail the
        // report as an unexplained one.
        assert!(
            !r.failures.iter().any(|f| f.starts_with("discontinuity")),
            "{:?}",
            r.failures
        );
    }

    /// A `ReconnectDiscontinuity` reaches the attribution as a gap marker:
    /// an injection resolved inside the gap is lost in transit under Lossy
    /// and undetected under Strict.
    #[test]
    fn a_reconnect_marker_excuses_an_injection_inside_its_gap() {
        use crate::corrupt::Class;
        let hdr = corruption_header();
        let wire = wire_for("baseline", 2.0);
        let run = |mode: VerifyMode| {
            let mut t = healthy_baseline_tally();
            let mut inj = injection_at(Class::PsiFlip, 0, 50); // resolves at ordinal 50
            inj.psi = true;
            t.attach_attribution(attribution_for(mode, vec![inj], &hdr));
            t.feed_at(&video_event(90 * FPS_STEP_TICKS, true), 20); // last media before the gap
            t.feed_at(&DemuxEvent::ReconnectDiscontinuity, 60);
            t.feed_at(&video_event(91 * FPS_STEP_TICKS, true), 70);
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                2.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            let a = r
                .metrics
                .corruption_attribution
                .clone()
                .expect("attribution");
            (r, a)
        };
        let (r, a) = run(VerifyMode::Lossy);
        assert_eq!(
            (
                a.reconnects_seen,
                a.lost_in_reconnect_gap,
                a.undetected_lost
            ),
            (1, 1, 1),
            "{a:?}"
        );
        assert!(
            !r.failures
                .iter()
                .any(|f| f.starts_with("corruption_detected")),
            "{:?}",
            r.failures
        );
        let (r, a) = run(VerifyMode::Strict);
        assert_eq!(
            (a.reconnects_seen, a.lost_in_reconnect_gap),
            (1, 0),
            "{a:?}"
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("corruption_detected")),
            "{:?}",
            r.failures
        );
    }

    /// The `corruption_attributed` failure's COUNT must describe the list
    /// it quotes. `events - attributed_events` includes the
    /// discontinuity-family events Lossy excused and removed from that
    /// list, so without subtracting them a run reads "3 unexplained
    /// event(s)" while holding a list of one.
    #[test]
    fn the_unexplained_count_excludes_events_excused_as_transport_loss() {
        use crate::corrupt::{Attribution, Class, Signal};
        let hdr = corruption_header();
        let wire = wire_for("baseline", 2.0);

        let mut t = healthy_baseline_tally();
        let mut a = Attribution::lossy(vec![injection_at(Class::Header, VIDEO_PID, 3)], &hdr);
        // One unexplained non-conformance (survives the excusal) and two
        // unexplained continuity jumps (excused), all outside the window.
        a.on_signal(5000, Some(VIDEO_PID), Signal::OtherNonConformant);
        a.on_signal(6000, Some(VIDEO_PID), Signal::ContinuityJump);
        a.on_signal(7000, Some(VIDEO_PID), Signal::ContinuityJump);
        t.attach_attribution(a);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );

        let rep = r.metrics.corruption_attribution.as_ref().expect("report");
        assert_eq!(rep.unexplained_events.len(), 1, "{rep:?}");
        assert_eq!(rep.unexplained_transport_loss, 2, "{rep:?}");

        let f = r
            .failures
            .iter()
            .find(|f| f.starts_with("corruption_attributed"))
            .unwrap_or_else(|| panic!("expected the failure: {:?}", r.failures));
        assert!(
            f.contains("1 unexplained event(s)") && f.contains("(2 excused as transport loss)"),
            "{f}"
        );
    }

    /// A rich ST 0601 record the sender's own tap damaged must not be
    /// reported as a PRODUCER defect: the three rich oracles ask what a
    /// conformant generator emitted, and these are not the bytes it
    /// emitted. Such a record is counted in `damaged_by_injection`,
    /// skipped by all three oracles, and still counted in `records` —
    /// it was delivered, it just cannot be held to a content contract.
    ///
    /// The same damaged record with no injection anywhere near it still
    /// fails `klv_rich_decode_clean`, which is what keeps this from being
    /// a blanket excuse.
    #[test]
    fn a_rich_record_an_injection_damaged_is_skipped_not_failed() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let wire = wire_for("baseline", 2.0);
        let p = profiles::by_name("baseline").unwrap();
        let rich = KlvExpect {
            set: KlvSet::Rich,
            seed: 0,
        };

        // A real rich record, then truncated mid-field — the exact shape
        // the live soak hit ("buffer truncated at offset N").
        let mut damaged = fixtures::klv_record_rich(0, 0).expect("rich record encodes");
        damaged.truncate(damaged.len() / 2);

        // `since_pcr` places an injection at a receiver ordinal directly,
        // with no `on_pcr` needed; the event is fed inside its window.
        let judge = |inj: crate::corrupt::Injection, at: u64| {
            let mut t = Tally::new();
            t.set_klv_expect(rich);
            t.attach_attribution(Attribution::lossy(vec![inj], &hdr));
            t.feed_at(&testing::klv_event_on(KLV_PID, 9_000, damaged.clone()), at);
            t.finish(p, 2.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire)
        };
        let decode_failed = |r: &VerifyReport| {
            r.failures
                .iter()
                .any(|f| f.starts_with("klv_rich_decode_clean"))
        };

        // Rule one: same PID, ANY class.
        let r = judge(injection_at(Class::BodyFlip, KLV_PID, 0), 10);
        let m = r.metrics.klv_rich.clone().expect("rich block");
        assert_eq!(m.damaged_by_injection, 1, "{m:?}");
        assert_eq!(m.records, 1, "a skipped record is still a delivered one");
        assert_eq!(m.decode_errors, 0, "{m:?}");
        assert!(!decode_failed(&r), "{:?}", r.failures);

        // Rule two: a framing class on ANOTHER PID — truncation
        // misaligns the whole multiplex, so the damage is not confined to
        // the PID it hit.
        let r = judge(injection_at(Class::Truncate, VIDEO_PID, 0), 10);
        let m = r.metrics.klv_rich.clone().expect("rich block");
        assert_eq!(m.damaged_by_injection, 1, "{m:?}");
        assert!(!decode_failed(&r), "{:?}", r.failures);

        // Neither rule: a body flip on ANOTHER PID explains nothing here.
        let r = judge(injection_at(Class::BodyFlip, VIDEO_PID, 0), 10);
        let m = r.metrics.klv_rich.clone().expect("rich block");
        assert_eq!(m.damaged_by_injection, 0, "{m:?}");
        assert_eq!(m.decode_errors, 1, "{m:?}");
        assert!(decode_failed(&r), "{:?}", r.failures);

        // Out of window entirely: the same record, still fatal, and the
        // failure string says how many were skipped (none).
        let r = judge(injection_at(Class::Truncate, KLV_PID, 50_000), 10);
        let m = r.metrics.klv_rich.clone().expect("rich block");
        assert_eq!(m.damaged_by_injection, 0, "{m:?}");
        assert!(decode_failed(&r), "{:?}", r.failures);
    }

    /// The non-conformance an injection explains stops failing the
    /// report; an unexplained one still fails, in both modes.
    #[test]
    fn an_attributed_nonconformance_no_longer_fails_the_report() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let wire = wire_for("baseline", 2.0);

        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::lossy(
            vec![injection_at(Class::PsiFlip, VIDEO_PID, 0)],
            &hdr,
        ));
        t.feed_at(&nonconformant_event(), 5);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert_eq!(r.metrics.nonconformant, 1, "still COUNTED, just explained");
        assert!(
            !r.failures
                .iter()
                .any(|f| f.starts_with("nonconformant_event")),
            "{:?}",
            r.failures
        );

        // Same event, no injection anywhere near it -> still fatal.
        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::lossy(
            vec![injection_at(Class::PsiFlip, VIDEO_PID, 0)],
            &hdr,
        ));
        t.feed_at(&nonconformant_event(), 5000);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("nonconformant_event")),
            "{:?}",
            r.failures
        );
    }

    /// The surviving `nonconformant_event` failure must quote an
    /// UNEXPLAINED event. Two non-conformances, the first inside an
    /// injection's window and the second nowhere near one: the count says
    /// 1 and the `first:` text must describe the SECOND — quoting the
    /// explained one would send a reader to evidence the report has
    /// already accounted for.
    #[test]
    fn the_surviving_failure_quotes_an_unexplained_event_not_the_first_one() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::lossy(
            vec![injection_at(Class::PsiFlip, VIDEO_PID, 0)],
            &hdr,
        ));
        // Explained: inside the window of the injection at ordinal 0.
        t.feed_at(&nonconformant_event(), 5);
        // Unexplained: far past it.
        t.feed_at(&nonconformant_event(), 7000);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );
        assert_eq!(r.metrics.nonconformant, 2, "both are still COUNTED");
        let failure = r
            .failures
            .iter()
            .find(|f| f.starts_with("nonconformant_event"))
            .unwrap_or_else(|| panic!("expected the failure: {:?}", r.failures));
        assert!(failure.contains("1 event(s)"), "{failure}");
        assert!(
            failure.contains("at packet 7000"),
            "must quote the UNEXPLAINED event: {failure}"
        );
    }

    /// Same rule for the Strict-mode discontinuity failure.
    #[test]
    fn the_surviving_discontinuity_failure_also_quotes_an_unexplained_event() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::strict(
            vec![injection_at(Class::Drop, VIDEO_PID, 0)],
            &hdr,
        ));
        t.feed_at(&discontinuity_event(), 4);
        t.feed_at(&discontinuity_event(), 8000);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Strict,
            &wire_for("baseline", 2.0),
        );
        let failure = r
            .failures
            .iter()
            .find(|f| f.starts_with("discontinuity_event"))
            .unwrap_or_else(|| panic!("expected the failure: {:?}", r.failures));
        assert!(failure.contains("1 event(s)"), "{failure}");
        assert!(
            failure.contains("at packet 8000"),
            "must quote the UNEXPLAINED event: {failure}"
        );
    }

    /// An attribution built for the wrong tier is a wiring bug, and it
    /// FAILS the report rather than panicking. A panic at the end of a
    /// three-day capture would throw away every other verdict the run
    /// earned, including the evidence a reader needs to recognise the bug.
    #[test]
    fn a_capture_judged_in_the_wrong_tier_fails_instead_of_panicking() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let inj = injection_at(Class::Header, VIDEO_PID, 3);
        let wire = wire_for("baseline", 2.0);

        for (mode, attribution) in [
            (
                VerifyMode::Lossy,
                Attribution::strict(vec![inj.clone()], &hdr),
            ),
            (VerifyMode::Strict, Attribution::lossy(vec![inj], &hdr)),
        ] {
            let mut t = healthy_baseline_tally();
            t.attach_attribution(attribution);
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                2.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            let f = r
                .failures
                .iter()
                .find(|f| f.starts_with("corruption_tier"))
                .unwrap_or_else(|| {
                    panic!("{mode:?} must report a tier mismatch: {:?}", r.failures)
                });
            assert!(!r.pass, "{f}");
        }

        // …and the matched pairs stay clean, so the check is not simply
        // always-on.
        let mut t = healthy_baseline_tally();
        t.attach_attribution(Attribution::lossy(
            vec![injection_at(Class::Header, VIDEO_PID, 3)],
            &hdr,
        ));
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert!(
            !r.failures.iter().any(|f| f.starts_with("corruption_tier")),
            "{:?}",
            r.failures
        );
    }

    /// Without an attribution nothing about the existing fatality
    /// changes — the corruption verdicts are opt-in evidence, not a new
    /// default leniency.
    #[test]
    fn no_attribution_leaves_every_existing_verdict_alone() {
        let wire = wire_for("baseline", 2.0);
        let mut t = healthy_baseline_tally();
        t.feed_at(&nonconformant_event(), 7);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire,
        );
        assert!(r.metrics.corruption_attribution.is_none());
        assert!(
            r.failures
                .iter()
                .any(|f| f.starts_with("nonconformant_event")),
            "{:?}",
            r.failures
        );
    }

    /// A resync recorded by the raw reader is a signal like any other —
    /// and every element of a DRAINED batch is fed, so the live loop's
    /// repeated polling neither double-counts nor drops one.
    #[test]
    fn note_resyncs_feeds_each_drained_batch_once() {
        use crate::corrupt::{Attribution, Class};
        let hdr = corruption_header();
        let mut t = Tally::new();
        t.attach_attribution(Attribution::lossy(
            vec![injection_at(Class::Truncate, VIDEO_PID, 2)],
            &hdr,
        ));
        let resyncs = vec![rawts::Resync {
            at_packets: 4,
            skipped_bytes: 188,
        }];
        t.note_resyncs(&resyncs);
        // A drained caller never hands the same batch twice; the next
        // poll is simply empty.
        t.note_resyncs(&[]);
        t.feed_at(&video_event(0, true), 10);
        feed_two_seconds_baseline(&mut t);
        let r = t.finish(
            profiles::by_name("baseline").unwrap(),
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );
        let a = r.metrics.corruption_attribution.unwrap();
        assert_eq!(a.resyncs, 1);
        assert_eq!(a.events, 1);
        assert_eq!(a.attributed_events, 1);
        assert!(a.undetected.is_empty(), "{a:?}");
    }

    /// End to end offline, over a genuinely corrupted stream: generate a
    /// clean capture, push it through the real tap, then judge the
    /// damaged bytes against the log the tap wrote.
    ///
    /// Two things must hold, and neither is visible in the `Tally`-level
    /// tests above. Sync loss caused by the tap must be attributed
    /// rather than reported as `rawts_sync_loss` (the reader is in
    /// resync mode) — and every injection must RESOLVE, which only works
    /// if the receive-side PCR ordinals line up with the coordinates the
    /// tap logged. An off-by-one in either direction leaves injections
    /// unresolved, so `resolved == injected` is the real assertion here.
    #[test]
    fn verify_with_a_corruption_log_resolves_and_attributes_real_damage() {
        use crate::corrupt::{self, Corrupter, parse_corrupt, testing};
        use std::sync::{Arc, Mutex};
        use tst_core::transport::Transport;

        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let path = std::env::temp_dir().join(format!(
            "tst-interop-verify-corrupt-{}-{}.ts",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        // 30s (~1800 packets) rather than the 3s the other tests use:
        // `min_gap`'s floor is 1000 packets (`CorruptConfig::validate`),
        // and a capture that fits only ONE injection would place it
        // before the stream's first PCR, where a coordinate resolves
        // trivially and proves nothing about PCR alignment. Offline
        // generation has no sleeps, so the longer window is still
        // milliseconds.
        crate::r#gen::run(
            p,
            30.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .expect("gen::run must succeed");
        let clean = std::fs::read(&path).expect("read the generated capture");
        let _ = std::fs::remove_file(&path);

        // `rate=10000` corrupts every eligible packet, so `min_gap`
        // alone decides the count. Truncation is the class that actually
        // breaks packet framing, which is what exercises resync mode.
        let cfg = parse_corrupt("rate=10000,min_gap=1000,classes=truncate", 7)
            .expect("the spec must parse");
        let wire = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut tap = Corrupter::new(
            testing::VecTransport(Arc::clone(&wire)),
            cfg,
            Box::new(testing::VecWriter(Arc::clone(&log))),
        )
        .expect("the tap must build");
        for chunk in clean.chunks(1316) {
            tap.send_bytes(chunk)
                .expect("the in-memory sink never fails");
        }
        drop(tap);
        let damaged = wire.lock().expect("wire mutex").clone();
        let log_text = String::from_utf8(log.lock().expect("log mutex").clone())
            .expect("the log is JSON text");
        let parsed = corrupt::parse_log(&log_text).expect("the tap's own log must parse");
        assert!(!parsed.1.is_empty(), "the tap must have injected something");
        assert_ne!(damaged, clean, "the tap must have changed the wire");

        assert!(
            parsed.1.iter().any(|i| i.coord.pcr_base.is_some()),
            "at least one injection must be anchored to a real PCR"
        );

        let r = verify_bytes_with_corruption(
            &damaged,
            p,
            30.0,
            VerifyMode::Lossy,
            KlvExpect::compact(),
            Some(&parsed),
        );
        let a = r
            .metrics
            .corruption_attribution
            .expect("a judged capture carries its attribution");
        assert_eq!(a.injected, parsed.1.len() as u64);
        assert_eq!(
            a.resolved, a.injected,
            "every injection's coordinate must resolve against the receive-side PCRs: {a:?}"
        );
        assert!(
            a.resyncs > 0,
            "truncation must cost the raw reader packet sync: {a:?}"
        );
        assert!(
            !r.failures.iter().any(|f| f.starts_with("rawts_sync_loss")),
            "sync loss the log explains must not fail the capture: {:?}",
            r.failures
        );
        assert!(
            a.attributed_events > 0,
            "the events this damage produced must be attributed to it: {a:?}"
        );

        // The same damaged bytes with NO log: the sync loss is
        // unexplained and fails the capture, as it always has.
        let blind = verify_bytes_with_mode(&damaged, p, 30.0, VerifyMode::Lossy);
        assert!(!blind.pass, "{:?}", blind.failures);
        assert!(blind.metrics.corruption_attribution.is_none());
    }

    #[test]
    fn strict_mode_fails_on_a_discontinuity_lossy_counts_it() {
        let wire = wire_for("baseline", 3.0);
        for (mode, expect_pass) in [(VerifyMode::Strict, false), (VerifyMode::Lossy, true)] {
            let mut t = healthy_baseline_tally();
            t.feed(&discontinuity_event());
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                3.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            assert_eq!(r.pass, expect_pass, "{mode:?}: {:?}", r.failures);
            assert_eq!(r.metrics.discontinuities, 1);
            if !expect_pass {
                assert!(
                    r.failures
                        .iter()
                        .any(|f| f.starts_with("discontinuity_event")),
                    "{:?}",
                    r.failures
                );
            }
        }
    }

    #[test]
    fn nonconformant_fails_in_both_modes() {
        let wire = wire_for("baseline", 3.0);
        for mode in [VerifyMode::Strict, VerifyMode::Lossy] {
            let mut t = healthy_baseline_tally();
            t.feed(&nonconformant_event());
            let r = t.finish(
                profiles::by_name("baseline").unwrap(),
                3.0,
                NOMINAL_COUNT_SLACK,
                mode,
                &wire,
            );
            assert!(!r.pass);
            assert_eq!(r.metrics.nonconformant, 1);
            assert!(
                r.failures
                    .iter()
                    .any(|f| f.starts_with("nonconformant_event")),
                "{:?}",
                r.failures
            );
        }
    }

    #[test]
    fn tally_passes_matching_profile() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let mut t = Tally::new();
        feed_two_seconds_baseline(&mut t);

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );

        assert!(
            report.pass,
            "expected pass, failures: {:?}",
            report.failures
        );
        assert_eq!(report.metrics.video_aus, 60);
        assert_eq!(report.metrics.keyframes, 2); // frames 0 and 30
        assert_eq!(report.metrics.klv_records, 20);
        assert_eq!(report.metrics.programs_seen, 1);
        assert!(report.metrics.pts_monotonic);
        assert!(!report.metrics.misp_sei_seen);
    }

    #[test]
    fn tally_fails_missing_klv() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event(i as i64 * FPS_STEP_TICKS, i % 30 == 0));
        }
        // No KLV events fed at all.

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );

        assert!(!report.pass);
        assert_eq!(report.metrics.klv_records, 0);
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("KLV") && f.contains('0')),
            "expected a failure naming the observed KLV count (0), got: {:?}",
            report.failures
        );
    }

    #[test]
    fn tally_fails_wrong_klv_bytes() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let wire = wire_for("baseline", 2.0);

        let mut expected = Tally::new();
        feed_two_seconds_baseline(&mut expected);
        let expected_report =
            expected.finish(p, 2.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire);

        let mut tampered = Tally::new();
        tampered.feed(&program_map_event());
        for i in 0..60u32 {
            tampered.feed(&video_event(i as i64 * FPS_STEP_TICKS, i % 30 == 0));
        }
        for i in 0..20u32 {
            // Same count, same cadence, but every record's payload is
            // built from a different seq (`i + 1000`) — same shape,
            // different bytes.
            tampered.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i + 1000));
        }
        let tampered_report =
            tampered.finish(p, 2.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire);

        // Counts/invariants alone can't see the swap (same cadence, same
        // record count) — the set fingerprint is what catches it. Actual
        // equality-checking across send/recv reports is the caller's job;
        // here we only need the hash to change when the bytes do.
        assert_eq!(
            expected_report.metrics.klv_records,
            tampered_report.metrics.klv_records
        );
        assert_ne!(
            expected_report.metrics.klv_set_sha256,
            tampered_report.metrics.klv_set_sha256
        );
    }

    #[test]
    fn tally_pts_rollover_aware() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let wire = wire_for("baseline", 2.0);

        // (a) Crossing the 2^33 wrap with per-frame deltas must NOT count
        // as a violation.
        let mut wrapping = Tally::new();
        const WRAP: i64 = 1i64 << 33;
        let start = WRAP - FPS_STEP_TICKS * 3; // 3 frames before the wrap
        for i in 0..6i64 {
            let raw = start + i * FPS_STEP_TICKS;
            let pts = raw.rem_euclid(WRAP); // the wire value wraps at 2^33
            wrapping.feed(&video_event(pts, i == 0));
        }
        let report = wrapping.finish(p, 2.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire);
        assert!(
            report.metrics.pts_monotonic,
            "small per-frame deltas across the 2^33 wrap must be accepted"
        );

        // (b) A genuine multi-second backwards jump on the same PID (no
        // wrap involved) must be flagged.
        let mut violated = Tally::new();
        violated.feed(&video_event(200_000, true));
        violated.feed(&video_event(20_000, false)); // ~2s backwards
        let report = violated.finish(p, 2.0, NOMINAL_COUNT_SLACK, VerifyMode::Lossy, &wire);
        assert!(
            !report.metrics.pts_monotonic,
            "a 2s backwards jump must be flagged as non-monotonic"
        );
    }

    #[test]
    fn tally_handles_av1_klv_profile_with_overlapping_pmt_stream_type() {
        // av1-klv-a: AV1 video and async KLV both ride PMT stream_type
        // 0x06 on the real wire (see `profiles::invariants`). Prove the
        // tally doesn't conflate the two streams' counts — it never looks
        // at `stream_type` at all, only at which PID/event carried what.
        let p = profiles::by_name("av1-klv-a").expect("av1-klv-a profile must exist");
        assert_eq!(profiles::invariants(p).video_stream_type, 0x06);
        assert_eq!(profiles::invariants(p).klv_stream_type, 0x06);

        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event_on(
                VIDEO_PID,
                VideoCodec::Av1,
                i as i64 * FPS_STEP_TICKS,
                i % 30 == 0,
            ));
        }
        for i in 0..20u32 {
            t.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i));
        }

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("av1-klv-a", 2.0),
        );

        assert!(report.pass, "failures: {:?}", report.failures);
        assert_eq!(report.metrics.video_aus, 60);
        assert_eq!(report.metrics.klv_records, 20);
    }

    #[test]
    fn tally_fails_wrong_video_codec() {
        // baseline expects H264; feed H265-coded video AUs instead. Counts
        // and every other invariant are otherwise satisfied, so only the
        // codec check can catch this.
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event_on(
                VIDEO_PID,
                VideoCodec::H265,
                i as i64 * FPS_STEP_TICKS,
                i % 30 == 0,
            ));
        }
        for i in 0..20u32 {
            t.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i));
        }

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("baseline", 2.0),
        );

        assert!(!report.pass);
        assert!(
            report.failures.iter().any(|f| f.contains("codec")),
            "expected a failure naming the codec mismatch, got: {:?}",
            report.failures
        );
    }

    #[test]
    fn tally_fails_wrong_klv_carriage() {
        // klv-sync expects AU-cell-wrapped (sync) KLV; feed bare-LS async
        // KLV events instead. Counts/cadence/video are otherwise correct,
        // so only the carriage check can catch this.
        let p = profiles::by_name("klv-sync").expect("klv-sync profile must exist");
        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event(i as i64 * FPS_STEP_TICKS, i % 30 == 0));
        }
        for i in 0..20u32 {
            t.feed(&klv_event(i as i64 * KLV_STEP_TICKS, i)); // async-shaped
        }

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("klv-sync", 2.0),
        );

        assert!(!report.pass);
        assert!(
            report.failures.iter().any(|f| f.contains("KLV carriage")),
            "expected a failure naming the KLV carriage mismatch, got: {:?}",
            report.failures
        );
    }

    #[test]
    fn tally_passes_klv_sync_profile_with_matching_carriage() {
        // Positive control for the check above: klv-sync fed genuinely
        // sync-shaped (AU-cell-wrapped) KLV events must pass.
        let p = profiles::by_name("klv-sync").expect("klv-sync profile must exist");
        let mut t = Tally::new();
        t.feed(&program_map_event());
        for i in 0..60u32 {
            t.feed(&video_event(i as i64 * FPS_STEP_TICKS, i % 30 == 0));
        }
        for i in 0..20u32 {
            t.feed(&klv_sync_event(i as i64 * KLV_STEP_TICKS, i));
        }

        let report = t.finish(
            p,
            2.0,
            NOMINAL_COUNT_SLACK,
            VerifyMode::Lossy,
            &wire_for("klv-sync", 2.0),
        );

        assert!(report.pass, "failures: {:?}", report.failures);
        assert_eq!(report.metrics.klv_records, 20);
    }

    /// The four `Tally`-level tests above never touch `verify_file`'s own
    /// file-reading/chunk-alignment/`Demuxer::feed`/`flush` plumbing (they
    /// call `Tally::feed` directly on hand-built events). Mux a small real
    /// TS file and drive it through `verify_file` end to end to cover that
    /// remaining path.
    ///
    /// Built from `mux_setup::build_config` (not a hand-rolled
    /// `MuxerConfig` on this module's own `VIDEO_PID`/`KLV_PID` sentinel
    /// PIDs) — `oracles::check`'s wire-level oracles now read `Invariants
    /// ::programs`, which is derived from `mux_setup`'s real PID
    /// constants, so a genuine `verify_file` pass needs a capture actually
    /// muxed onto those PIDs.
    #[test]
    fn verify_file_passes_a_real_muxed_capture() {
        use tst_core::mpegts::mux::Muxer;

        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let cfg = crate::mux_setup::build_config(p);
        let mut mux = Muxer::new(cfg).expect("muxer must construct");

        for i in 0..60u32 {
            let (au, keyframe) = fixtures::video_au(p.video, i);
            mux.push_video(&au, Pts90khz::new(i as i64 * FPS_STEP_TICKS), keyframe)
                .expect("push_video must succeed");
        }
        for i in 0..20u32 {
            let record = fixtures::klv_record(i);
            mux.push_klv(&record, Pts90khz::new(i as i64 * KLV_STEP_TICKS), 0)
                .expect("push_klv must succeed");
        }

        let mut ts_bytes = Vec::new();
        let mut buf = vec![0u8; 1316];
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            ts_bytes.extend_from_slice(&buf[..n]);
        }
        assert!(!ts_bytes.is_empty(), "muxer produced no TS bytes");

        let path = std::env::temp_dir().join(format!(
            "tst-interop-verify-smoke-{}.ts",
            std::process::id()
        ));
        std::fs::write(&path, &ts_bytes).expect("write temp TS file");

        let result = verify_file(&path, p, 2.0);
        let _ = std::fs::remove_file(&path);
        let report = result.expect("verify_file must succeed reading the file");

        assert!(report.pass, "failures: {:?}", report.failures);
        assert_eq!(report.metrics.video_aus, 60);
        assert_eq!(report.metrics.klv_records, 20);
        assert_eq!(report.metrics.bytes, ts_bytes.len() as u64);
        assert!(!report.metrics.stream_sha256.is_empty());
    }

    /// The wire-vs-demux loss allowance is keyed to the PID the event
    /// happened on, and an attributed event widens that PID's floor
    /// exactly ONCE.
    ///
    /// Both halves were wrong together. The allowance was a single
    /// capture-wide number, so a video event excused a missing KLV
    /// record; and under `Lossy` an attributed event was added twice —
    /// once as `attributed_events` and again through the
    /// discontinuity/non-conformance tally, which is fed from the same
    /// events (`feed_at` routes each one to the attribution and then
    /// tallies it). One attributed video jump plus one unattributed KLV
    /// non-conformance therefore bought EVERY PID an allowance of three.
    #[test]
    fn the_loss_allowance_is_per_pid_and_counts_an_attributed_event_once() {
        use crate::corrupt::Class;
        let hdr = corruption_header();
        // Logged damage to the VIDEO PID, and nothing else.
        let inj = injection_at(Class::Header, VIDEO_PID, 3);

        let mut t = Tally::new();
        t.attach_attribution(attribution_for(VerifyMode::Lossy, vec![inj], &hdr));
        // Attributed: inside that injection's window, on the PID it damaged.
        t.feed_at(&discontinuity_event(), 10);
        // Unattributed, and on a PID no injection ever touched.
        t.feed_at(&klv_nonconformant_event(), 9_000);

        let rep = t
            .attribution
            .take()
            .expect("attached just above")
            .finish(10_000);
        assert_eq!(rep.attributed_events, 1, "{rep:?}");
        assert_eq!(t.discontinuities, 1);
        assert_eq!(t.nonconformant, 1);

        // Lossy: each PID is owed exactly the one event it saw. The
        // attributed video jump is ALREADY in the discontinuity tally, so
        // adding `attributed_events` on top would count it twice.
        let lossy = t.explained(VerifyMode::Lossy, Some(&rep));
        assert_eq!(lossy.for_pid(VIDEO_PID), 1, "{lossy:?}");
        assert_eq!(lossy.for_pid(KLV_PID), 1, "{lossy:?}");
        assert_eq!(
            lossy.multiplex_wide, 0,
            "neither event is a resync, so nothing is multiplex-wide: {lossy:?}"
        );

        // Strict: only what the corruption log explains counts at all —
        // the capture's own events are failures in their own right there,
        // so the untouched KLV PID is owed nothing.
        let strict = t.explained(VerifyMode::Strict, Some(&rep));
        assert_eq!(strict.for_pid(VIDEO_PID), 1, "{strict:?}");
        assert_eq!(strict.for_pid(KLV_PID), 0, "{strict:?}");

        // With no corruption log at all, only the capture's own events
        // count — and only under `Lossy`.
        let none_strict = t.explained(VerifyMode::Strict, None);
        assert_eq!(none_strict, Explained::default(), "{none_strict:?}");
        let none_lossy = t.explained(VerifyMode::Lossy, None);
        assert_eq!(none_lossy.for_pid(VIDEO_PID), 1, "{none_lossy:?}");
        assert_eq!(none_lossy.for_pid(KLV_PID), 1, "{none_lossy:?}");
    }
}
