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
    DemuxEvent, Demuxer, MetadataKind, SamplePayload, VideoCodec as DemuxVideoCodec,
};

use crate::oracles;
use crate::profiles::{self, Profile};
use crate::rawts::{self, WireSummary};
use crate::report_types::{CellMetrics, VerifyReport};

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
/// (video AUs, KLV records) and [`crate::oracles::program_accounting`]'s
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
    /// `Debug`-formatted `DiscontinuityKind` of the first `Discontinuity`
    /// event fed, if any.
    first_discontinuity: Option<String>,
    /// `Display`-formatted `NonConformantIssue` of the first
    /// `NonConformant` event fed, if any.
    first_nonconformant: Option<String>,
}

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
            first_discontinuity: None,
            first_nonconformant: None,
        }
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

    /// Fold one demuxed event into the tally.
    pub fn feed(&mut self, ev: &DemuxEvent) {
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
                self.record_pts(stream.pid, *pts);
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
                self.record_pts(stream.pid, *pts);
                self.klv_records += 1;
                self.per_program
                    .entry(stream.program_number)
                    .or_default()
                    .klv_records += 1;
                if self.track_klv_digests {
                    self.klv_digests.push(to_hex(&Sha256::digest(payload)));
                }
                self.klv_carriage_seen.insert(klv_carriage_of(kind));
            }
            DemuxEvent::Discontinuity { kind, .. } => {
                self.discontinuities += 1;
                self.first_discontinuity
                    .get_or_insert_with(|| format!("{kind:?}"));
            }
            DemuxEvent::NonConformant { issue, .. } => {
                self.nonconformant += 1;
                self.first_nonconformant
                    .get_or_insert_with(|| issue.to_string());
            }
            DemuxEvent::ReconnectDiscontinuity => {}
        }
    }

    fn record_pts(&mut self, pid: u16, pts: Pts90khz) {
        let now = pts.as_ticks() as u64;
        if let Some(&last) = self.last_pts_by_pid.get(&pid) {
            if !pts_is_monotonic_step(now, last) {
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

    /// Check the tally against `p`'s invariants for a `seconds`-long
    /// capture, requiring at least `slack` (e.g. `0.7` = 70%) of each
    /// nominal per-second count. `mode` governs whether a `Discontinuity`
    /// event fails the check (`Strict`) or is merely counted (`Lossy`);
    /// `NonConformant` always fails, in either mode.
    ///
    /// `wire` is the independent [`rawts`] reader's summary of the same
    /// bytes, alongside the tst-core-demuxed `Tally` — checked by
    /// [`oracles::check`]'s six wire-level oracles, parameterized by
    /// `p`'s [`profiles::Invariants`].
    pub fn finish(
        self,
        p: &Profile,
        seconds: f64,
        slack: f64,
        mode: VerifyMode,
        wire: &WireSummary,
    ) -> VerifyReport {
        let inv = profiles::invariants(p);
        let mut failures = Vec::new();

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

        if self.nonconformant > 0 {
            failures.push(format!(
                "nonconformant_event: {} event(s), first: {}",
                self.nonconformant,
                self.first_nonconformant.as_deref().unwrap_or("?")
            ));
        }
        if mode == VerifyMode::Strict && self.discontinuities > 0 {
            failures.push(format!(
                "discontinuity_event: {} event(s), first: {}",
                self.discontinuities,
                self.first_discontinuity.as_deref().unwrap_or("?")
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
        ));

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
    // Built per-profile (never `Demuxer::new()`/`DemuxerConfig::default()`)
    // — see `profiles::demuxer_config`'s doc comment for the av1-klv-a
    // finding this closes.
    let mut demux = Demuxer::with_config(profiles::demuxer_config(p));
    let mut tally = Tally::new();
    // Independent wire-level reader, fed the exact same bytes as the
    // demuxer — see `rawts`'s module doc for why it shares no code with
    // `Demuxer`.
    let mut wire_reader = rawts::Reader::new();

    tally.note_bytes(bytes);
    let demux_err = demux.feed(bytes).err();
    let wire_err = wire_reader.feed(bytes).err();

    // Canonical end-of-stream signal — see `demux_to_events.rs`'s doc
    // comment: without this the last access unit of every stream is left
    // sitting in the reassembler and silently dropped. Drained even after
    // a feed error above: whatever the demuxer managed to reassemble
    // before losing sync is still real signal for the tally.
    demux.flush();
    while let Some(ev) = demux.next_event() {
        tally.feed(&ev);
    }

    let wire = wire_reader.finish();
    let mut report = tally.finish(p, seconds, NOMINAL_COUNT_SLACK, mode, &wire);
    if let Some(e) = demux_err {
        report.pass = false;
        report.failures.push(format!("demux_error: {e}"));
    }
    if let Some(e) = wire_err {
        report.pass = false;
        report.failures.push(format!("rawts_sync_loss: {e}"));
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
    let bytes = std::fs::read(path)?;
    Ok(verify_bytes_with_mode(
        &bytes,
        p,
        seconds,
        VerifyMode::Strict,
    ))
}

#[cfg(test)]
mod tests {
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
        crate::r#gen::run(p, seconds, &path).expect("gen::run must succeed");
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
        DemuxEvent::Metadata {
            stream: StreamId {
                pid,
                kind: StreamKind::KlvAsync,
                program_number: PROGRAM,
            },
            pts: Pts90khz::new(pts_ticks),
            kind: MetadataKind::KlvAsync,
            payload: fixtures::klv_record(seq),
        }
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
}
