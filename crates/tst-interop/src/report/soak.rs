use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::csv::{RSS_HEADER, csv_rows};
use crate::proxy::ProxyStats;
use crate::report_types::{
    CellMetrics, ManagedSendStats, PublishMountReport, RECONNECT_MODES, VerifyReport,
};

/// Absolute ceiling on the warmup excluded from the RSS regression,
/// in seconds. The actual warmup is `min(MAX_WARMUP_S,
/// config.expected_duration_s * config.warmup_fraction)` — 30
/// minutes for a full-length run, or a fraction of the DECLARED run
/// length for a shorter one (a `--hours 1` smoke's own post-warmup
/// window would otherwise be near-empty against a flat 30-minute
/// floor) — whichever is smaller. Derived from the declared config
/// rather than the observed sample span so a truncated run doesn't
/// also shrink its own warmup exclusion.
pub(crate) const MAX_WARMUP_S: f64 = 30.0 * 60.0;

/// The RSS slope, in KiB per hour, above which `soak.sh` fails a
/// full-length run: its default `--rss-slope-threshold-kb-per-hour`
/// (the derivation is in the interop README's "Outputs and the
/// supervisor"). Other gates that judge memory the soak's way use it.
pub(crate) const RSS_SLOPE_THRESHOLD_KB_PER_HOUR: f64 = 200.0;

/// The warmup fraction `soak.sh` declares in `soak-config.json`.
pub(crate) const WARMUP_FRACTION: f64 = 0.1667;

/// The RSS warmup excluded from a run declared `expected_duration_s` long.
pub(crate) fn rss_warmup_s(expected_duration_s: f64, warmup_fraction: f64) -> f64 {
    MAX_WARMUP_S.min(expected_duration_s * warmup_fraction)
}

/// The soak's sampling-density rule over one process's post-warmup
/// `(elapsed_s, kb)` points, sorted by time: at least two distinct
/// timestamps, at least `min_samples` points, and no gap of `max_gap_s` or
/// more. Returns whether it holds, the largest gap and the distinct
/// timestamp count.
pub(crate) fn rss_coverage(
    points: &[(f64, f64)],
    min_samples: u64,
    max_gap_s: f64,
) -> (bool, f64, usize) {
    let mut largest_gap_s = 0.0f64;
    for w in points.windows(2) {
        largest_gap_s = largest_gap_s.max(w[1].0 - w[0].0);
    }
    let distinct_ts = points
        .iter()
        .map(|(t, _)| t.to_bits())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let pass = distinct_ts >= 2 && points.len() as u64 >= min_samples && largest_gap_s < max_gap_s;
    (pass, largest_gap_s, distinct_ts)
}

const KNOWN_LEGS: [&str; 5] = ["srt", "rist", "rtsp-publish", "rtsp-publish-es", "whep"];

/// Legs fed by an external publisher RECORDing into `recv`'s publish
/// mount instead of by a `tst-interop send` through an impairment
/// proxy. Their processes are `recv` and `publisher` (the shell loop
/// that relaunches the publisher tool; the tool itself exits at the
/// end of every generation by design), they have no proxy-stats or
/// send report, and they are judged by [`build_soak_results`]'s
/// publish-leg verdicts rather than the transport-leg ones.
///
/// `rtsp-publish` pushes an MP2T stream the mount forwards byte for
/// byte; `rtsp-publish-es` pushes elementary H.264 + KLV tracks the
/// mount re-muxes, so its bytes change in flight and it declares
/// [`LegDeclaration::remuxed`].
pub const PUBLISH_LEGS: [&str; 2] = ["rtsp-publish", "rtsp-publish-es"];

/// The WebRTC playback leg: `tst-interop serve-whep` serves one mount
/// for the whole run and a headless browser plays it, relaunched on
/// `scripts/interop/whep-page.html` every `viewer_reconnect_s` by the
/// viewer loop. Its processes are `serve` and `viewer` (the loop's
/// shell; the browser's own memory is not sampled, as a publish leg's
/// tool is not). It has no proxy, no sender and no recv report: it is
/// judged by [`build_soak_results`]'s `whep_*` verdicts from the
/// viewer's per-connection report ([`WhepViewerReport`]) and the
/// server's own counters ([`WhepServeReport`]).
pub const WHEP_LEGS: [&str; 1] = ["whep"];

/// Samples, one per second, within which a whep connection must show
/// its first decoded frame: the server sends the next keyframe (one per
/// second of the served GOP) once the session is connected.
const WHEP_FIRST_FRAME_SAMPLES: usize = 5;

/// The longest run of consecutive `FRAMES` samples that may fail to rise
/// after the first frame: two one-second samples. A third is a gap of
/// over 2 s, `whep_frames_continuous`'s failing bound.
const WHEP_MAX_STALL_SAMPLES: usize = 2;

/// How many samples a connection may fall short of its planned length:
/// the page samples once a second from 1 s, the loop's `timeout` lands
/// inside the last second, and a busy host can slip one more.
const WHEP_SAMPLE_SHORTFALL: u64 = 3;

/// `timeout`'s exit status when it ended the browser on schedule — the
/// only browser exit a connection may end with.
const WHEP_BROWSER_TIMED_OUT: i32 = 124;

/// Records at a connection's head whose two copies may be unpaired: the
/// data channel opens a moment after the session is connected, and a
/// record bound to a frame sent in that moment reaches the viewer in
/// the SEI only (the server counts it in `klv_dc_dropped_viewer`).
const WHEP_KLV_EDGE_HEAD: u64 = 3;

/// Records at a connection's tail whose two copies may be unpaired: the
/// loop ends the browser while the server keeps pushing, and the kill
/// can land between a record's two copies.
const WHEP_KLV_EDGE_TAIL: u64 = 2;

/// The fraction of the served rate a connection must reach over its
/// samples after the first-frame window: records per carriage against
/// `klv_records_pushed / seconds`, the last `FRAMES` sample against
/// `frames_pushed / seconds`. A connection that only ticks, or that
/// saw a handful of records on both carriages, fails on volume.
const WHEP_VOLUME_FLOOR: f64 = 0.9;

/// `soak.sh`'s `WHEP_RELAUNCH_GAP_S`: each browser is ended this many
/// seconds before its slot ends, so a connection's window is the slot
/// minus this. `whep_reconnects_observed` holds the viewer report's
/// own `connection_s` to it, so the report cannot state its window.
const WHEP_RELAUNCH_GAP_S: u64 = 3;

/// The per-leg corruption verdict family, in the order it is
/// emitted. Named once so the tap-disabled and no-attribution arms
/// cannot drift from the judged arm — a verdict missing from one of
/// them would simply not appear in `soak-results.json` for that kind
/// of run, which reads as "not applicable" rather than "not checked".
const CORRUPTION_VERDICTS: [&str; 6] = [
    "corruption_attributed",
    "corruption_detected",
    "corruption_recovered",
    "corruption_coverage",
    "corruption_declared",
    "corruption_excusal_budget",
];

/// Floor on the fraction of a leg's injections that must resolve to a
/// receiver position for `corruption_coverage_<leg>` to pass. See
/// that verdict's own comment for why it is not 100%.
const CORRUPTION_RESOLVED_FLOOR: f64 = 0.9;

/// Floor on the fraction of the SENDER's logged injections the
/// receiver must have ingested. Tighter than the resolution floor
/// because ingestion is a file read the receiver controls, not
/// something the network can take away — but still a floor, because
/// one missed tail read at teardown must not fail a 72-hour run.
const CORRUPTION_INGESTED_FLOOR: f64 = 0.99;

/// Q12: constant term of the lossy excusal budget
/// `K x expected_outage_windows + C + ceil(R x hours)` — the strictly
/// ONE-OFF excusals, the ones that happen once per run however long
/// it is: a stranded anchor at start-up, a teardown gap. Everything
/// that accrues with time is [`EXCUSAL_PER_HOUR`]'s job, and
/// everything that accrues per outage is `K x windows`.
const EXCUSAL_BUDGET_BASE: u64 = 8;

/// R for the excusal budget: excusals per HOUR of run, on top of the
/// one-off [`EXCUSAL_BUDGET_BASE`] and the per-outage `K x windows`
/// term.
///
/// Pinned from a retained 1-hour smoke run (`report soak`
/// over `/tmp/soak-resmoke`, 2026-09-14 — the newest run with all six
/// workers healthy): the `srt`/baseline leg excused 0, the
/// `rist`/pcr-sparse leg excused 9 (9 transport-loss events, 9
/// unexplained discontinuities) in 3541 s, with ZERO outage windows,
/// while every other corruption verdict on that leg passed (1284
/// events, 1275 attributed, 0 unexplained, 1493 injected / 1493
/// resolved). Pinned at 16/h — the observed 9/h with ~1.8x headroom.
///
/// ★ Why a RATE and not a bigger constant: those 9 are not one-off.
/// The first sits mid-run at packet 410490 and they are spread
/// through the impairment schedule rather than clustered at start-up
/// — they are the continuous-loss residue an ARQ transport does not
/// fully repair. `soak.sh` stretches the schedule across the whole
/// run (`phase_s = total / phases`), so a 72-hour run sees the same
/// distribution of impairment and should be expected to produce on
/// the order of hundreds. A constant term sized for 1 h would fail
/// that healthy run; one sized for 72 h would be meaningless at 1 h.
const EXCUSAL_PER_HOUR: u64 = 16;

/// The lossy excusal budget (Q12):
/// `K x expected_outage_windows + EXCUSAL_BUDGET_BASE + ceil(EXCUSAL_PER_HOUR x hours)`.
///
/// `duration_s` is the run's MEASURED length — the RSS-sample span
/// `run_duration_s`, which is also what `expected_outage_windows`
/// counts over, so both terms describe the same window. A run whose
/// samples span nothing (a single sample, or a sampler that never
/// ticked) falls back to the declared `expected_duration_s` rather
/// than collapsing the rate term to zero and failing a leg for a
/// telemetry gap.
pub(super) fn excusal_budget(k: u64, outage_windows: u64, duration_s: f64, declared_s: f64) -> u64 {
    let hours = (if duration_s > 0.0 {
        duration_s
    } else {
        declared_s
    })
    .max(0.0)
        / 3600.0;
    let rate = (EXCUSAL_PER_HOUR as f64 * hours).ceil();
    // `hours` comes from measured telemetry, so clamp rather than
    // trust it into a `u64` cast: a corrupt RSS file must not be able
    // to mint an unbounded budget (or saturate to 0 on a NaN).
    let rate = if rate.is_finite() {
        rate.clamp(0.0, u64::MAX as f64) as u64
    } else {
        0
    };
    k.saturating_mul(outage_windows)
        .saturating_add(EXCUSAL_BUDGET_BASE)
        .saturating_add(rate)
}

/// K for the excusal budget: how many elementary-stream PIDs the
/// leg's profile muxes (video + KLV per program, + audio when the
/// profile carries it). Unknown or undeclared profile → 2, the
/// smallest any profile has, so an undeclared leg gets the
/// tightest budget rather than a free pass.
fn media_pids(profile: Option<&str>) -> u64 {
    profile
        .and_then(crate::profiles::by_name)
        .map(|p| {
            crate::profiles::invariants(p)
                .programs
                .iter()
                .map(|ep| 2 + u64::from(ep.audio_pid.is_some()))
                .sum()
        })
        .unwrap_or(2)
}

/// The processes one leg runs, i.e. the `process` values its rows in
/// `rss.csv` may carry and the `<leg>-<process>` roles it reaps into
/// `exits.json`. A publish leg's `publisher` is the restart loop's
/// shell, not the publisher tool: its RSS rows and its exit status are
/// the loop's, and the tool's own memory is not sampled.
fn leg_processes(leg: &str) -> &'static [&'static str] {
    if PUBLISH_LEGS.contains(&leg) {
        &["recv", "publisher"]
    } else if WHEP_LEGS.contains(&leg) {
        &["serve", "viewer"]
    } else {
        &["send", "proxy", "recv"]
    }
}

/// Every worker `soak.sh` reaps into `exits.json` (the sampler is
/// killed by the script itself and is deliberately not a worker
/// here): `<leg>-<process>` for every leg and its [`leg_processes`].
/// The `worker_exits` verdict below only REQUIRES a role's entry when
/// its leg is present in the run (a single-leg srt-only run, e.g. a
/// local smoke test, never launches the `rist-*` three) — but a key
/// outside this whole set, for ANY run, is always an unconditional
/// failure, never silently ignored.
const WORKER_ROLES: [&str; 12] = [
    "srt-send",
    "srt-proxy",
    "srt-recv",
    "rist-send",
    "rist-proxy",
    "rist-recv",
    "rtsp-publish-recv",
    "rtsp-publish-publisher",
    "rtsp-publish-es-recv",
    "rtsp-publish-es-publisher",
    "whep-serve",
    "whep-viewer",
];
/// Fraction of the cadence-implied post-warmup sample count a
/// process must actually have.
pub(crate) const MIN_SAMPLE_COVERAGE: f64 = 0.9;
/// Largest tolerated gap between consecutive samples, in cadences.
pub(crate) const MAX_GAP_CADENCES: f64 = 3.0;

/// One `elapsed_s,leg,process,pid,rss_kb` row from `soak.sh`'s RSS
/// sampler. `rss_kb: None` means `/proc/<pid>/status` couldn't be
/// read at this tick — the sampler's own crash signal (see
/// [`SoakResults::process_exits`]); a healthy run never produces one
/// at all. NOT because the sampler outlives every tracked process —
/// the opposite is true and deliberately so: `soak.sh`'s sampler
/// stops `SAMPLER_END_SLACK_S` (35s) before its own nominal
/// deadline, specifically so it never samples a process that has
/// ALREADY exited on schedule (both proxies' own `--run-seconds`
/// budgets, sized from their own, earlier launch instants, can
/// otherwise elapse a few real seconds before the sampler's next
/// tick — confirmed empirically, ~6.5s for the srt leg, ~2.5s for
/// rist). A `None` that the same pid never follows with a sample means a
/// process died UNEXPECTEDLY, mid-run, not that the run reached its end;
/// one it does follow is a missed read (the sampler writes no row at all
/// for a live pid it could not read, so this arises only in older data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RssSample {
    pub elapsed_s: f64,
    pub leg: String,
    pub process: String,
    pub pid: u32,
    pub rss_kb: Option<u64>,
}

/// Parse `soak.sh`'s `rss.csv`: an `elapsed_s,leg,process,pid,rss_kb`
/// header line followed by one row per sample tick per process.
/// `rss_kb` may be empty (process gone at this tick). Loudly errors
/// (naming the offending line) on anything else — a malformed or
/// truncated soak artifact must never silently read as "no data,
/// every check vacuously passes."
pub fn parse_rss_csv(text: &str) -> Result<Vec<RssSample>, String> {
    let mut samples = Vec::new();
    for (line_no, fields) in csv_rows(text, RSS_HEADER, 5, "rss.csv")? {
        let elapsed_s: f64 = fields[0]
            .parse()
            .map_err(|_| format!("rss.csv line {line_no}: invalid elapsed_s {:?}", fields[0]))?;
        if !elapsed_s.is_finite() || elapsed_s < 0.0 {
            return Err(format!(
                "rss.csv line {line_no}: elapsed_s must be finite and non-negative, got {elapsed_s}"
            ));
        }
        let leg = fields[1].to_string();
        if !KNOWN_LEGS.contains(&leg.as_str()) {
            return Err(format!(
                "rss.csv line {line_no}: unknown leg {leg:?} (want one of {KNOWN_LEGS:?})"
            ));
        }
        let process = fields[2].to_string();
        let processes = leg_processes(&leg);
        if !processes.contains(&process.as_str()) {
            return Err(format!(
                "rss.csv line {line_no}: unknown process {process:?} for leg {leg:?} (want one of {processes:?})"
            ));
        }
        let pid: u32 = fields[3]
            .parse()
            .map_err(|_| format!("rss.csv line {line_no}: invalid pid {:?}", fields[3]))?;
        let rss_kb =
            if fields[4].is_empty() {
                None
            } else {
                Some(fields[4].parse::<u64>().map_err(|_| {
                    format!("rss.csv line {line_no}: invalid rss_kb {:?}", fields[4])
                })?)
            };
        samples.push(RssSample {
            elapsed_s,
            leg,
            process,
            pid,
            rss_kb,
        });
    }
    Ok(samples)
}

/// `soak.sh`'s declared run parameters, written to `soak-config.json`
/// at launch — BEFORE any evidence exists — so `report soak` judges the
/// run against what was configured, never against what the artifacts
/// happen to contain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakConfig {
    pub expected_duration_s: f64,
    pub rss_cadence_s: f64,
    /// `warmup_s = min(MAX_WARMUP_S, expected_duration_s * warmup_fraction)`.
    pub warmup_fraction: f64,
    /// The sampler deliberately stops this many seconds before the
    /// nominal deadline (`soak.sh`'s `SAMPLER_END_SLACK_S`), so the
    /// achievable RSS span is `expected_duration_s - sampler_end_slack_s`.
    pub sampler_end_slack_s: f64,
    /// Worker role -> exit status that is expected for this run
    /// (normally empty; a drill that kills a role on purpose lists it).
    #[serde(default)]
    pub expected_worker_exits: BTreeMap<String, i32>,
    /// Whether this run's senders were launched with `send --corrupt`
    /// (`soak.sh`'s corruption tap). `#[serde(default)]` so archived
    /// pre-corruption-tap configs still parse as `false` rather than
    /// failing to deserialize. Gates whether the `corruption_*_<leg>`
    /// verdicts judge a real attribution report or just record that
    /// the tap was off.
    #[serde(default)]
    pub corruption: bool,
    /// The tap spec `soak.sh` passed to `send --corrupt`, declared
    /// before launch and checked afterwards against the spec the
    /// receiver's own attribution report read out of the sender's log
    /// header. Run-wide because `soak.sh` gives both legs the same
    /// spec (only the seed differs per leg).
    ///
    /// `#[serde(default)]` so an archived config that declared
    /// `corruption: true` without a spec still parses; that yields a
    /// passing `corruption_declared_<leg>` saying the config predates
    /// the declaration, rather than failing evidence retroactively.
    #[serde(default)]
    pub corruption_spec: Option<CorruptionDeclaration>,
    /// Per-leg declaration of what `soak.sh` launched that leg with:
    /// which stream profile, and which impairment schedule (if any).
    /// Keyed by leg name: a transport leg (`"srt"`/`"rist"`) or a
    /// publish leg ([`PUBLISH_LEGS`]).
    ///
    /// `#[serde(default)]` so an archived pre-realism config still
    /// parses; a leg with no entry yields passing
    /// `profile_declared_<leg>`/`schedule_declared_<leg>` verdicts
    /// saying so, rather than failing evidence that predates the
    /// declaration.
    #[serde(default)]
    pub legs: BTreeMap<String, LegDeclaration>,
    /// The memory prediction `soak.sh` checked before launching any
    /// worker. Recorded into [`SoakResults::limitations`] for
    /// provenance; no verdict judges it. `#[serde(default)]` so a
    /// config written before the gate existed still parses.
    #[serde(default)]
    pub memory_gate: Option<MemoryGate>,
}

/// The launch-time memory gate as `soak.sh` declared it: the run's
/// predicted resident memory against its budget. Validated at parse
/// time: `headroom` is finite and `>= 1.0`, and
/// `predicted_kb x headroom <= budget_kb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryGate {
    /// Summed expected RSS of the roles the run launches, in KiB, before
    /// `headroom` is applied.
    pub predicted_kb: u64,
    /// The memory the run may use, in KiB (a fraction of `mem_total_kb`).
    pub budget_kb: u64,
    /// The factor the prediction is scaled by before it is held to the budget.
    pub headroom: f64,
    /// The host's `MemTotal` from `/proc/meminfo`, in KiB.
    pub mem_total_kb: u64,
}

/// What one leg of a soak run was DECLARED to be — written before the
/// run starts, checked against the run's own artifacts afterwards.
///
/// The profile half closes a specific hole: `soak.sh` passes the
/// profile name to `send --profile` and to `recv --expect`
/// independently, so a harness bug could run (and pass) one shape
/// while the published evidence claims another. The schedule half does
/// the same for the impairment the proxy actually ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegDeclaration {
    /// A [`crate::profiles`] name — rejected at parse time if unknown.
    pub profile: String,
    /// The `--schedule seed=,phases=,phase_s=` this leg's proxy was
    /// launched with, or `None` for a fixed-impairment run.
    #[serde(default)]
    pub schedule: Option<ScheduleDeclaration>,
    /// The `--klv-set` this leg's sender and receiver were launched
    /// with (`"compact"` / `"rich"`) — rejected at parse time if
    /// unknown. Closes the same hole `profile` does: the flag is
    /// passed to `send` and to `recv` independently, so a harness bug
    /// could run one set while the published evidence claims the
    /// other, and a rich run whose receiver judged in compact mode
    /// would quietly run NO rich-KLV oracle at all.
    #[serde(default)]
    pub klv_set: Option<String>,
    /// The `--klv-seed` both ends were given. Recorded for
    /// provenance, not checked: nothing in a recv report carries the
    /// seed back, and a wrong one is already fatal by another route —
    /// the presence schedule would not match, and every record would
    /// count as a census mismatch.
    #[serde(default)]
    pub klv_seed: Option<u64>,
    /// The `send --reconnect-mode` this leg's managed sender was
    /// launched with (`"blocking"` / `"background"`) — rejected at
    /// parse time if unknown — or `None` for a leg whose sender is
    /// not managed. The two modes exercise different things across
    /// an outage (a stalled producer and a replayed backlog, against
    /// a producer that keeps going and a bounded buffer that drops),
    /// and nothing else in a run's artifacts says which one it was.
    #[serde(default)]
    pub reconnect_mode: Option<String>,
    /// A publish leg's publisher command line (`PUBLISH_LEGS`).
    /// Recorded for provenance, not checked. `None` on every other
    /// leg — and rejected there at parse time.
    #[serde(default)]
    pub publisher: Option<String>,
    /// A publish leg's publisher generation length, in seconds: the
    /// publisher session ends and a new one ANNOUNCEs about every this
    /// many seconds. Required on a publish leg and recorded; rejected
    /// on every other leg. Not what `publisher_generations_<leg>`
    /// judges against — that is [`Self::publisher_generations`], the
    /// count the run actually launched, because a length floored to
    /// whole seconds does not divide back into the run's duration.
    #[serde(default)]
    pub publisher_drop_period_s: Option<u64>,
    /// How many publisher sessions the run launched on a publish leg
    /// (one per stream segment). Required on a publish leg, at least
    /// two; `publisher_generations_<leg>` requires every one of them
    /// to have ended. Rejected on every other leg.
    #[serde(default)]
    pub publisher_generations: Option<u64>,
    /// A publish leg's source digest: the lowercase hex sha256 of the
    /// whole stream the publishers pushed, generation after
    /// generation. `delivery_complete_<leg>` holds the receiver's
    /// `metrics.stream_sha256` to it, so a generation lost or cut
    /// short anywhere, the last one included, fails the leg.
    /// `soak.sh` writes it once the stream is generated, before any
    /// worker launches. `#[serde(default)]` so a config written
    /// before the field existed still parses; `report soak` then
    /// falls back to the leg's `source.sha256` file. Rejected on every
    /// other leg.
    #[serde(default)]
    pub source_sha256: Option<String>,
    /// A publish leg whose bytes legitimately change in flight (the mount re-muxes
    /// elementary tracks). `delivery_complete_<leg>` then judges KLV completeness,
    /// not the stream digest, and `source_sha256` must be absent.
    #[serde(default)]
    pub remuxed: Option<bool>,
    /// KLV records in one publisher generation's source; required when `remuxed`.
    #[serde(default)]
    pub klv_records_per_generation: Option<u64>,
    /// The leg's receiver ran `--klv-placement`; `klv_placement_<leg>` is required to pass.
    #[serde(default)]
    pub klv_placement: Option<bool>,
    /// The whep leg's mount kind (`"mp2t"` / `"elementary"`), the
    /// `serve-whep --mount-kind` it was launched with; checked against
    /// the kind the server's counters name. Required on the whep leg
    /// ([`WHEP_LEGS`]), rejected on every other leg.
    #[serde(default)]
    pub mount_kind: Option<String>,
    /// The whep leg's viewer command line (headless Chrome on the
    /// harness page). Recorded for provenance, not checked. Rejected on
    /// every other leg.
    #[serde(default)]
    pub viewer: Option<String>,
    /// How often the whep leg's viewer loop ends its browser and starts
    /// a new one, in seconds. Required on the whep leg (> 0) and checked
    /// against the slot the viewer report says it ran; rejected on
    /// every other leg.
    #[serde(default)]
    pub viewer_reconnect_s: Option<u64>,
    /// How many browser connections the whep leg's viewer loop makes
    /// over the run, one per reconnect slot. Required on the whep leg,
    /// at least two (one connection reconnects nothing);
    /// `whep_reconnects_observed` holds both the viewer's own count and
    /// the server's session total to it. Rejected on every other leg.
    #[serde(default)]
    pub viewer_connections: Option<u64>,
}

/// The declared half of the corruption tap's configuration, compared
/// field for field against the spec the receiver's attribution report
/// read out of the sender's own log header. The seed is not declared
/// here: `soak.sh` derives a distinct one per leg from the run seed,
/// and the log header's copy is what the receiver already judges
/// against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorruptionDeclaration {
    pub rate_per_10k: u32,
    pub min_gap: u64,
    pub classes: Vec<crate::corrupt::Class>,
}

/// The declared half of a leg's impairment schedule — compared field
/// for field against the proxy's own [`crate::proxy::ScheduleEcho`].
/// The generated phase table isn't declared: it is a pure function of
/// `seed` and `phases` (see [`crate::impair::generate_schedule`]), so
/// checking the three inputs checks the table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDeclaration {
    pub seed: u64,
    pub phases: u32,
    pub phase_s: u64,
}

pub fn parse_soak_config(text: &str) -> Result<SoakConfig, String> {
    let cfg: SoakConfig =
        serde_json::from_str(text).map_err(|e| format!("soak-config.json: {e}"))?;
    if !(cfg.expected_duration_s.is_finite() && cfg.expected_duration_s > 0.0) {
        return Err(format!(
            "soak-config.json: expected_duration_s must be finite and > 0, got {}",
            cfg.expected_duration_s
        ));
    }
    if !(cfg.rss_cadence_s.is_finite() && cfg.rss_cadence_s > 0.0) {
        return Err(format!(
            "soak-config.json: rss_cadence_s must be finite and > 0, got {}",
            cfg.rss_cadence_s
        ));
    }
    if !(cfg.warmup_fraction.is_finite() && (0.0..1.0).contains(&cfg.warmup_fraction)) {
        return Err(format!(
            "soak-config.json: warmup_fraction must be in [0, 1), got {}",
            cfg.warmup_fraction
        ));
    }
    if !(cfg.sampler_end_slack_s.is_finite() && cfg.sampler_end_slack_s >= 0.0) {
        return Err(format!(
            "soak-config.json: sampler_end_slack_s must be finite and >= 0, got {}",
            cfg.sampler_end_slack_s
        ));
    }
    if cfg.sampler_end_slack_s >= cfg.expected_duration_s {
        return Err(format!(
            "soak-config.json: sampler_end_slack_s ({}) must be less than expected_duration_s ({}) — \
             otherwise the achievable RSS span is non-positive before a single sample is examined",
            cfg.sampler_end_slack_s, cfg.expected_duration_s
        ));
    }
    // `duration_coverage`'s own passing bound (see `build_soak_results`)
    // is `run_duration_s >= expected_duration_s - sampler_end_slack_s -
    // 2*rss_cadence_s`. If that right-hand side is <= 0 every observed
    // span (which is always >= 0) passes vacuously — a short `--hours`
    // combined with the fixed 35s sampler slack can reach this (e.g.
    // 72s / cadence 30 / slack 35: 72 - 35 - 60 = -23). Caught here, at
    // config-declaration time, rather than only showing up as a
    // silent, uninformative pass.
    let min_span_s = cfg.expected_duration_s - cfg.sampler_end_slack_s - 2.0 * cfg.rss_cadence_s;
    if min_span_s <= 0.0 {
        return Err(format!(
            "soak-config.json: expected_duration_s ({}) minus sampler_end_slack_s ({}) minus \
             2 * rss_cadence_s ({}) = {min_span_s} <= 0 — every run would pass duration_coverage \
             vacuously (any observed span is >= 0)",
            cfg.expected_duration_s, cfg.sampler_end_slack_s, cfg.rss_cadence_s
        ));
    }
    // Same vacuous-pass hazard on the sample-coverage side:
    // `rss_sample_coverage_*` needs >= 2 distinct timestamps to
    // compute a gap at all (see `build_soak_results`'s `distinct_ts >=
    // 2` check) — below that, `min_samples` (90% of `expected_samples`,
    // itself possibly 0 or 1) can never meaningfully gate anything, so
    // reject a config whose OWN implied expected-sample count already
    // can't reach 2.
    let warmup_s = rss_warmup_s(cfg.expected_duration_s, cfg.warmup_fraction);
    let expected_samples = ((cfg.expected_duration_s - cfg.sampler_end_slack_s - warmup_s)
        / cfg.rss_cadence_s)
        .floor();
    if expected_samples < 2.0 {
        return Err(format!(
            "soak-config.json: implied post-warmup sample count is {expected_samples} (< 2) from \
             expected_duration_s ({}), sampler_end_slack_s ({}), warmup {warmup_s}s (min(1800, \
             expected_duration_s * warmup_fraction)), and rss_cadence_s ({}) — no run of this \
             length could ever pass rss_sample_coverage",
            cfg.expected_duration_s, cfg.sampler_end_slack_s, cfg.rss_cadence_s
        ));
    }
    for role in cfg.expected_worker_exits.keys() {
        if !WORKER_ROLES.contains(&role.as_str()) {
            return Err(format!(
                "soak-config.json: expected_worker_exits has unknown role {role:?} (want one of {WORKER_ROLES:?})"
            ));
        }
    }
    // Same reasoning as the unknown-role check above: a declaration
    // keyed by a leg this report never judges, or naming a profile
    // that doesn't exist, would silently verify nothing at all — and
    // `soak.sh` validates its config before launching a multi-day run
    // precisely so a typo surfaces now rather than at teardown.
    for (leg, decl) in &cfg.legs {
        if !KNOWN_LEGS.contains(&leg.as_str()) {
            return Err(format!(
                "soak-config.json: legs has unknown leg {leg:?} (want one of {KNOWN_LEGS:?})"
            ));
        }
        if crate::profiles::by_name(&decl.profile).is_none() {
            return Err(format!(
                "soak-config.json: legs.{leg}.profile {:?} is not a known profile",
                decl.profile
            ));
        }
        if let Some(set) = &decl.klv_set
            && crate::fixtures::KlvSet::parse(set).is_none()
        {
            return Err(format!(
                "soak-config.json: legs.{leg}.klv_set {set:?} is not a known KLV set \
                     (want \"compact\" or \"rich\")"
            ));
        }
        // Exact, lowercase names only: the verdict compares this
        // string with the one the sender wrote, so a spelling the
        // sender can never write is a declaration that can never be
        // met.
        if let Some(mode) = &decl.reconnect_mode
            && !RECONNECT_MODES.contains(&mode.as_str())
        {
            return Err(format!(
                "soak-config.json: legs.{leg}.reconnect_mode {mode:?} is not a known \
                     reconnect mode (want one of {RECONNECT_MODES:?}, or null for a leg whose \
                     sender is not managed)"
            ));
        }
        // A publish leg must declare at least two generations, so
        // `publisher_generations_<leg>` requires at least one
        // re-ANNOUNCE; anything else could only pass that verdict
        // vacuously. The publisher fields mean nothing on a
        // transport leg, so declaring them there is a typo.
        if PUBLISH_LEGS.contains(&leg.as_str()) {
            if decl.publisher_drop_period_s.is_none_or(|p| p == 0) {
                return Err(format!(
                    "soak-config.json: legs.{leg} is a publish leg and must declare \
                     publisher_drop_period_s > 0"
                ));
            }
            if decl.publisher_generations.is_none_or(|n| n < 2) {
                return Err(format!(
                    "soak-config.json: legs.{leg} is a publish leg and must declare \
                     publisher_generations >= 2 — one generation drops nothing"
                ));
            }
            if let Some(sha) = &decl.source_sha256
                && !is_sha256_hex(sha)
            {
                return Err(format!(
                    "soak-config.json: legs.{leg}.source_sha256 {sha:?} is not a \
                         lowercase hex sha256 digest"
                ));
            }
            // A re-muxed stream's bytes are not the source's, so a
            // digest could never match; KLV completeness is judged
            // against a per-generation count instead, which must be
            // declared and nonzero for that check to mean anything.
            // The count is only ever judged on a remuxed leg; anywhere
            // else it would be silently ignored.
            if decl.klv_records_per_generation.is_some() && decl.remuxed != Some(true) {
                return Err(format!(
                    "soak-config.json: legs.{leg} declares klv_records_per_generation \
                     without remuxed: true; the count is judged only on a remuxed leg"
                ));
            }
            if leg == "rtsp-publish-es" && decl.klv_placement != Some(true) {
                return Err(format!(
                    "soak-config.json: legs.{leg} must declare klv_placement: true; the \
                     placement oracle is what the elementary leg exists to judge"
                ));
            }
            if decl.remuxed == Some(true) {
                if decl.source_sha256.is_some() {
                    return Err(format!(
                        "soak-config.json: legs.{leg} is remuxed, so its bytes change in \
                         flight and source_sha256 must be absent"
                    ));
                }
                if decl.klv_records_per_generation.is_none_or(|n| n == 0) {
                    return Err(format!(
                        "soak-config.json: legs.{leg} is remuxed and must declare \
                         klv_records_per_generation > 0"
                    ));
                }
            }
        } else if decl.publisher.is_some()
            || decl.publisher_drop_period_s.is_some()
            || decl.publisher_generations.is_some()
            || decl.source_sha256.is_some()
            || decl.remuxed.is_some()
            || decl.klv_records_per_generation.is_some()
            || decl.klv_placement.is_some()
        {
            return Err(format!(
                "soak-config.json: legs.{leg} declares a publisher, but only a publish leg \
                 ({PUBLISH_LEGS:?}) has one"
            ));
        }
        // The whep leg's own three fields, for the same reason as the
        // publish fields above: a viewer with no reconnect slot or a
        // single connection could only pass `whep_reconnects_observed`
        // vacuously, and the fields mean nothing on any other leg.
        if WHEP_LEGS.contains(&leg.as_str()) {
            match decl.mount_kind.as_deref() {
                Some("mp2t") | Some("elementary") => {}
                other => {
                    return Err(format!(
                        "soak-config.json: legs.{leg} is the WebRTC playback leg and must \
                         declare mount_kind \"mp2t\" or \"elementary\", got {other:?}"
                    ));
                }
            }
            if decl.viewer_reconnect_s.is_none_or(|p| p == 0) {
                return Err(format!(
                    "soak-config.json: legs.{leg} is the WebRTC playback leg and must declare \
                     viewer_reconnect_s > 0"
                ));
            }
            if decl.viewer_connections.is_none_or(|n| n < 2) {
                return Err(format!(
                    "soak-config.json: legs.{leg} is the WebRTC playback leg and must declare \
                     viewer_connections >= 2 — one connection reconnects nothing"
                ));
            }
        } else if decl.mount_kind.is_some()
            || decl.viewer.is_some()
            || decl.viewer_reconnect_s.is_some()
            || decl.viewer_connections.is_some()
        {
            return Err(format!(
                "soak-config.json: legs.{leg} declares a viewer, but only the WebRTC playback \
                 leg ({WHEP_LEGS:?}) has one"
            ));
        }
    }
    // Reject a declared tap spec the tap itself would refuse, at
    // declaration time rather than after a multi-day run has produced
    // evidence nothing can judge. Same rule, from the same code:
    // `CorruptConfig::validate` is what `send --corrupt` enforces.
    if let Some(spec) = &cfg.corruption_spec {
        crate::corrupt::CorruptConfig {
            rate_per_10k: spec.rate_per_10k,
            min_gap: spec.min_gap,
            classes: spec.classes.clone(),
            seed: 0,
            // The soak always sends at AU scale 1.
            geometry_scale: 1,
        }
        .validate()
        .map_err(|e| format!("soak-config.json: corruption_spec: {e}"))?;
    }
    if let Some(gate) = &cfg.memory_gate {
        if !(gate.headroom.is_finite() && gate.headroom >= 1.0) {
            return Err(format!(
                "soak-config.json: memory_gate: headroom must be finite and >= 1.0, got {}",
                gate.headroom
            ));
        }
        if gate.predicted_kb as f64 * gate.headroom > gate.budget_kb as f64 {
            return Err(format!(
                "soak-config.json: memory_gate: predicted_kb {} x headroom {} exceeds \
                 budget_kb {}",
                gate.predicted_kb, gate.headroom, gate.budget_kb
            ));
        }
    }
    Ok(cfg)
}

/// 64 lowercase hex digits: the shape `sha256sum` prints and
/// `CellMetrics::stream_sha256` carries, so a declared digest in any
/// other spelling could never equal the received one.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `profile_declared_<leg>`: the recv report was judged against the
/// profile `soak-config.json` declared for the leg. Shared by
/// transport and publish legs.
fn profile_declared_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    recv: &VerifyReport,
) -> SoakVerdict {
    let (profile_pass, profile_detail) = match declared {
        None => (
            true,
            format!("{leg_name}: no declaration (pre-realism config)"),
        ),
        Some(d) => match &recv.profile {
            Some(observed) if *observed == d.profile => (
                true,
                format!("{leg_name}: recv judged the declared profile {observed:?}"),
            ),
            Some(observed) => (
                false,
                format!(
                    "{leg_name}: config declared profile {:?} but the recv report was \
                     judged against {observed:?}",
                    d.profile
                ),
            ),
            None => (
                false,
                format!(
                    "{leg_name}: config declared profile {:?} but the recv report carries \
                     no profile (an offline `verify` report, or one written before `recv` \
                     stamped it)",
                    d.profile
                ),
            ),
        },
    };
    SoakVerdict {
        name: format!("profile_declared_{leg_name}"),
        pass: profile_pass,
        provisional: false,
        detail: profile_detail,
    }
}

/// `klv_declared_<leg>`: the receiver ran the KLV oracles of the set
/// `soak-config.json` declared for the leg. Shared by transport and
/// publish legs.
fn klv_declared_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    recv: &VerifyReport,
) -> SoakVerdict {
    let observed_rich = recv.metrics.klv_rich.is_some();
    let (klv_pass, klv_detail) = match declared.and_then(|d| d.klv_set.as_deref()) {
        None => (
            true,
            format!("{leg_name}: no KLV set declared (pre-realism config)"),
        ),
        Some("rich") if observed_rich => {
            let rich = recv.metrics.klv_rich.as_ref();
            (
                true,
                format!(
                    "{leg_name}: declared rich and recv judged {} record(s) against the \
                     rich oracles (seed {:?})",
                    rich.map_or(0, |r| r.records),
                    declared.and_then(|d| d.klv_seed)
                ),
            )
        }
        Some("rich") => (
            false,
            format!(
                "{leg_name}: config declared the rich KLV set but the recv report carries \
                 no klv_rich block — the receiver judged this leg in compact mode, so no \
                 rich-KLV oracle ran"
            ),
        ),
        Some("compact") if observed_rich => (
            false,
            format!(
                "{leg_name}: config declared the compact KLV set but the recv report \
                 carries a klv_rich block"
            ),
        ),
        Some(other) => (
            !observed_rich,
            format!("{leg_name}: declared KLV set {other:?}"),
        ),
    };
    SoakVerdict {
        name: format!("klv_declared_{leg_name}"),
        pass: klv_pass,
        provisional: false,
        detail: klv_detail,
    }
}

/// `soak.sh`'s `exits.json`: `{ "<role>": <exit status>, ... }` for every
/// worker it reaped (every leg's processes; never the sampler, which the
/// script itself kills on schedule).
pub fn parse_worker_exits(text: &str) -> Result<BTreeMap<String, i32>, String> {
    serde_json::from_str(text).map_err(|e| format!("exits.json: {e}"))
}

/// Ordinary-least-squares slope of `points` (x, y) pairs — used here
/// as KB of RSS per second of elapsed wall-clock time. Returns `0.0`
/// for fewer than 2 points or a degenerate (all-same-x) input rather
/// than dividing by zero.
pub(crate) fn linear_regression_slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let sum_x: f64 = points.iter().map(|(x, _)| x).sum();
    let sum_y: f64 = points.iter().map(|(_, y)| y).sum();
    let sum_xy: f64 = points.iter().map(|(x, y)| x * y).sum();
    let sum_xx: f64 = points.iter().map(|(x, _)| x * x).sum();
    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        return 0.0;
    }
    (n * sum_xy - sum_x * sum_y) / denom
}

/// Number of outage windows (`impair::Engine::in_outage`'s zero-based
/// `[k*period, k*period+dur)` numbering, measured from the PROXY's
/// own launch instant) whose START falls within the period send/recv
/// are actually active.
///
/// `duration_s` is `run_duration_s` — the active-traffic period's own
/// length, NOT measured from the proxy's `t=0` but from when
/// send/recv themselves start. `soak.sh` deliberately launches the
/// proxy `outage_dur_s + 30` seconds ahead of send/recv (its own
/// `SRT_PROXY_WARMUP_S` constant — mirrored here as `head_start_s`,
/// two independent literals for one concept; keep them in sync if
/// either changes) specifically so scheduled window `k=0` — which
/// always starts at the proxy's `t=0`, before send/recv exist — can
/// never reach real traffic. The pre-fix version of this function
/// measured from the proxy's own `t=0` and so always counted window
/// 0 for any nonzero `duration_s`, even though it's structurally
/// impossible for send/recv to ever observe it (caught in PR review:
/// a 1h smoke run, whose active period never reaches window 1 either,
/// was reporting "1 window expected" as if window 0 were a real,
/// reachable event). Fixed by counting windows whose start `k*period_s`
/// falls within `[head_start_s, head_start_s + duration_s)` instead —
/// window 0's start (`t=0`) is always before `head_start_s`, so it's
/// now correctly excluded.
fn expected_outage_windows(
    duration_s: f64,
    outage_period_s: Option<u64>,
    outage_dur_s: u64,
) -> u64 {
    let Some(period_s) = outage_period_s else {
        return 0;
    };
    if period_s == 0 || duration_s <= 0.0 {
        return 0;
    }
    let period_s = period_s as f64;
    let head_start_s = outage_dur_s as f64 + 30.0;
    let active_end_s = head_start_s + duration_s;
    // Smallest k with k*period_s >= head_start_s (first window whose
    // start can reach the active period), and the smallest k with
    // k*period_s >= active_end_s (first window whose start is past
    // it) — the count of windows in between is the answer.
    let first_reachable = (head_start_s / period_s).ceil() as u64;
    let first_past_end = (active_end_s / period_s).ceil() as u64;
    first_past_end.saturating_sub(first_reachable)
}

/// What one phase of the impairment engine realises, from its
/// renewal process (Q11) — see `impair::Engine::decide`: outside a
/// burst every packet first rolls duplication (`dup_pct`), then
/// loss (`draw_pct`); a hit drops a run of `R ~ U[lo, hi]` packets
/// (`burst_run` when `burst`, otherwise `(1, 1)`), the rest of the run
/// without a draw. With `q` the per-packet fire probability, `G` the
/// geometric run of forwards before a fire (`E[G] = (1-q)/q`,
/// `Var[G] = (1-q)/q²`), `m`/`v` the run mean/variance:
/// cycle `X = G + R`, reward `R`, long-run drop fraction
/// `p = m / E[X] = m q / (1 - q + m q)` — `5.5q / (1 + 4.5q)` for the
/// (3, 8) burst runs, exactly `q` for `(1, 1)` — and per-packet
/// variance rate `Var(R - pX) / E[X] = ((1-p)² v + p² Var[G]) / E[X]`
/// (renewal-reward CLT), which is `q(1-q)` — the binomial term — for
/// a non-burst phase and larger for a burst one (each fire drops ~m
/// correlated packets).
///
/// Deliberately does NOT add an outage-coverage term: outage-window
/// drops are counted apart and excluded from the comparison — see
/// `proxy::PhaseCounters::outage_dropped`.
#[derive(Clone, Copy, Debug)]
struct PhaseModel {
    /// Expected drop fraction.
    p: f64,
    /// Variance of the drop COUNT per packet.
    var_rate: f64,
}

fn renewal_model(draw_pct: f64, dup_pct: f64, burst: bool, burst_run: (u32, u32)) -> PhaseModel {
    let q = (draw_pct / 100.0).clamp(0.0, 1.0) * (1.0 - (dup_pct / 100.0).clamp(0.0, 1.0));
    if q <= 0.0 {
        return PhaseModel {
            p: 0.0,
            var_rate: 0.0,
        };
    }
    // `Engine::decide` reads `burst_run` ONLY under `if p.burst`, so a
    // phase that is not bursty drops exactly one packet per fire
    // whatever its `burst_run` says — which a hand-edited or
    // deserialized artifact can perfectly well disagree with. Key on
    // the same flag the engine keys on, or the expectation models a
    // burst the engine never ran.
    let (lo, hi) = if burst { burst_run } else { (1, 1) };
    // Same clamp as `Engine::decide`: lo >= 1, hi >= lo.
    let lo_u = lo.max(1);
    let lo = f64::from(lo_u);
    let hi = f64::from(hi.max(lo_u));
    let m = (lo + hi) / 2.0;
    let v = ((hi - lo + 1.0).powi(2) - 1.0) / 12.0;
    let e_x = (1.0 - q) / q + m;
    let var_g = (1.0 - q) / (q * q);
    let p = m / e_x;
    let var_rate = ((1.0 - p).powi(2) * v + p * p * var_g) / e_x;
    PhaseModel { p, var_rate }
}

/// Standard-error multiplier for [`drop_rate_tolerance`]. ~6 standard
/// errors is generous enough that sampling noise at any packet volume
/// this soak pushes won't false-positive, while a genuine
/// multiple-of-the-configured-rate regression still fails — the flat
/// `max(3pp, expected*30%)` band this replaced did NOT scale down
/// with volume, so a real 2x regression at high packet counts could
/// sit comfortably inside a fixed few-percentage-point band forever.
const DROP_RATE_TOLERANCE_SIGMA: f64 = 6.0;

/// Absolute floor on the tolerance band (0.1 percentage points): keeps
/// the band finite for `n == 0` (a genuine 0-packet division would
/// otherwise propagate `inf`/`NaN` into `LegResult::
/// drop_fraction_tolerance`'s JSON, which `serde_json` can't
/// serialize) and absorbs model residue at packet counts where the
/// statistical term is smaller than that.
const DROP_RATE_TOLERANCE_FLOOR: f64 = 0.001;

/// `DROP_RATE_TOLERANCE_SIGMA` standard errors of the observed drop
/// FRACTION (`sigma` = sqrt(Σ nᵢ·var_rateᵢ) / N from the per-phase
/// renewal models), floored at [`DROP_RATE_TOLERANCE_FLOOR`]. Scales
/// down as the packet count grows — unlike the flat percentage band
/// this replaced, a 72h run's much larger packet count tightens the
/// check rather than leaving it exactly as loose as a five-second
/// interop-matrix cell would need.
fn drop_rate_tolerance(sigma: f64) -> f64 {
    (DROP_RATE_TOLERANCE_SIGMA * sigma).max(DROP_RATE_TOLERANCE_FLOOR)
}

/// One (leg, process) RSS linear-regression result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssSlope {
    pub leg: String,
    pub process: String,
    pub slope_kb_per_hour: f64,
    pub samples_used: usize,
}

/// Per-(leg, process) post-warmup sampling coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssCoverage {
    pub leg: String,
    pub process: String,
    pub expected_samples: u64,
    pub observed_samples: u64,
    pub largest_gap_s: f64,
    pub pass: bool,
}

/// One (leg, process) that disappeared from `/proc` mid-run — see
/// [`RssSample::rss_kb`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessExit {
    pub leg: String,
    pub process: String,
    pub elapsed_s: f64,
    pub pid: u32,
}

/// One named check in [`SoakResults::verdicts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakVerdict {
    pub name: String,
    pub pass: bool,
    /// A provisional verdict is recorded for evidence but never
    /// makes [`SoakResults::overall_pass`] false — see the module
    /// doc's "Known telemetry limitations" section.
    pub provisional: bool,
    pub detail: String,
}

/// Computed numbers for one leg (`"srt"` or `"rist"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegResult {
    pub leg: String,
    pub outage_period_s: Option<u64>,
    pub outage_dur_s: u64,
    pub expected_outage_windows: u64,
    pub proxy_forwarded: u64,
    pub proxy_dropped: u64,
    /// The effective (phase-integrated) expected drop rate, as a
    /// percentage — `expected_drop_fraction * 100.0`: the engine's
    /// realised (renewal) rate, packet-weighted across phases. For a
    /// fixed-mode run (no schedule) that is exactly the configured
    /// `loss_pct` scaled by `1 − dup_pct`.
    pub loss_pct: f64,
    pub observed_drop_fraction: f64,
    pub expected_drop_fraction: f64,
    pub drop_fraction_tolerance: f64,
    /// How many phases the drop-rate verdict integrated over — `1`
    /// for fixed mode or a pre-schedule stats file, `table.len()`
    /// for a scheduled run.
    pub phases: usize,
    pub recv_pass: bool,
    pub recv_failures: Vec<String>,
    pub send_video_aus: u64,
    pub recv_video_aus: u64,
    /// `AttributionReport::injected` from the recv report's
    /// `corruption_attribution`, or `None` when the config didn't
    /// declare `corruption` or the recv report carries no attribution.
    #[serde(default)]
    pub corruption_injected: Option<u64>,
    /// The send report's `managed_send` block, carried over unchanged
    /// so the leg's reconnect and gap-buffer counters sit beside its
    /// sent and received access-unit totals. `None` for a leg whose
    /// sender was not managed. Recorded, never gated — and not a
    /// complete account of what an outage cost: see the
    /// `gap_messages_dropped` entry in [`SoakResults::limitations`].
    #[serde(default)]
    pub managed_send: Option<ManagedSendStats>,
}

/// The full `soak-results.json` document: [`build_soak_results`]'s
/// output and [`run`]'s return value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakResults {
    pub run_duration_s: f64,
    pub expected_duration_s: f64,
    pub warmup_s: f64,
    pub rss_slope_threshold_kb_per_hour: Option<f64>,
    pub rss_slopes: Vec<RssSlope>,
    pub coverage: Vec<RssCoverage>,
    pub process_exits: Vec<ProcessExit>,
    pub worker_exits: BTreeMap<String, i32>,
    pub legs: Vec<LegResult>,
    /// The publish legs (`PUBLISH_LEGS`), apart from `legs` because
    /// they have no proxy or sender to report on. `#[serde(default)]`
    /// so a results file written before publish legs existed still
    /// loads.
    #[serde(default)]
    pub publish_legs: Vec<PublishLegResult>,
    /// The WebRTC playback leg ([`WHEP_LEGS`]), when the run had it.
    /// `#[serde(default)]` so a results file written before the leg
    /// existed still loads.
    #[serde(default)]
    pub whep_legs: Vec<WhepLegResult>,
    pub verdicts: Vec<SoakVerdict>,
    /// `true` iff every non-[`SoakVerdict::provisional`] verdict
    /// passed.
    pub overall_pass: bool,
    pub limitations: Vec<String>,
}

/// One publish leg's result: what its receiver judged, and the
/// publish mount's own counters beside the declared drop period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishLegResult {
    pub leg: String,
    pub publisher_drop_period_s: Option<u64>,
    /// The floor `publisher_generations_<leg>` held the mount's
    /// `generation` to: the declared generation count, or `None`
    /// without one.
    pub min_generations: Option<u64>,
    pub publish_mount: Option<PublishMountReport>,
    pub recv_pass: bool,
    pub recv_failures: Vec<String>,
    pub recv_video_aus: u64,
}

/// A publish leg's raw evidence: the receiver's report, and the
/// digest of the stream the publishers pushed. The
/// publisher is an external tool that writes no report of its own;
/// what it delivered is what the receiver judged, and how many
/// publishers came and went is the mount's own count, carried in the
/// report's `publish_mount` block.
#[derive(Debug, Clone)]
pub struct PublishLegArtifacts {
    pub recv_report: VerifyReport,
    /// The leg directory's `source.sha256` file, trimmed, or `None`
    /// when there is none. Read only when the config declares no
    /// [`LegDeclaration::source_sha256`] (a config written before
    /// that field existed).
    pub source_sha256_file: Option<String>,
}

/// The whep leg's result: what the viewer saw over every connection
/// and the server's own counters, beside the declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhepLegResult {
    pub leg: String,
    pub mount_kind: Option<String>,
    pub viewer_reconnect_s: Option<u64>,
    /// The declared connection count `whep_reconnects_observed` judged
    /// against, or `None` without one.
    pub viewer_connections_declared: Option<u64>,
    pub connections_observed: u64,
    pub server_sessions_total: u64,
    /// The last `FRAMES` sample of every connection, summed: frames the
    /// browser decoded over the run.
    pub frames_decoded: u64,
    pub klv_sei_records: u64,
    pub klv_dc_records: u64,
    /// Records whose two copies named different (PTS, RTP timestamp)
    /// pairs, summed over connections. Recorded, not gated: the matrix's
    /// `whep/*` cells gate the frame binding.
    pub klv_copies_disagreeing: u64,
    /// Records seen on one carriage only, all of them within a
    /// connection's head or tail allowance (see `whep_klv_sei_equals_dc`).
    pub klv_unpaired_at_edges: u64,
    pub mount: WhepMountCounters,
    pub server: WhepServerCounters,
}

/// The viewer loop's report (`whep/viewer-report.json`): one entry per
/// browser connection, parsed by `soak.sh` from that connection's
/// console (`scripts/interop/whep-page.html`'s `FRAMES`, `AUDIO`, `KLV`
/// and `ERROR` lines).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhepViewerReport {
    /// The reconnect slot the loop ran on (`WHEP_RECONNECT_S`), in seconds.
    pub reconnect_s: u64,
    /// How long each browser was given before the loop ended it.
    pub connection_s: u64,
    pub connections: Vec<WhepConnection>,
}

/// One browser connection of the whep leg's viewer loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhepConnection {
    pub index: u64,
    #[serde(default)]
    pub started_utc: String,
    /// The browser's exit status: `WHEP_BROWSER_TIMED_OUT` (124) when the
    /// loop's `timeout` ended it on schedule.
    pub browser_rc: i32,
    /// The page's once-a-second `FRAMES` samples (frames decoded so
    /// far), in order.
    pub frames: Vec<u64>,
    /// The page's `AUDIO` samples, when audio was negotiated.
    #[serde(default)]
    pub audio: Vec<u64>,
    /// Records seen via the SEI and via the data channel.
    pub klv_sei: u64,
    pub klv_dc: u64,
    /// The set digest of each carriage's records (sha256 over the sorted
    /// per-record digests, `verify::klv_set_hash`'s shape).
    pub klv_sei_set_sha256: String,
    pub klv_dc_set_sha256: String,
    /// Records seen on one carriage only, with their position from that
    /// carriage's head and tail.
    #[serde(default)]
    pub klv_unpaired: Vec<WhepUnpairedRecord>,
    /// Records whose two copies named different (PTS, RTP timestamp) pairs.
    #[serde(default)]
    pub klv_copies_disagreeing: u64,
    /// The page's `ERROR` lines and the browser's own `ERROR:CONSOLE` lines.
    pub page_errors: u64,
    pub console_errors: u64,
    #[serde(default)]
    pub first_error: Option<String>,
}

/// A record one carriage delivered and the other did not, located on
/// the carriage that has it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhepUnpairedRecord {
    /// `"sei"` or `"dc"`: the carriage that has it.
    pub via: String,
    /// Records before it on that carriage.
    pub from_head: u64,
    /// Records after it on that carriage.
    pub from_tail: u64,
}

/// `serve-whep --json`'s counters, the leg's server-side evidence.
/// Every field defaults so a counter added later never fails an
/// older report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhepServeReport {
    pub profile: Option<String>,
    pub mount_kind: Option<String>,
    /// The window served (`serve-whep --seconds`): the served rates the
    /// volume floors use are the mount's push counters over it.
    pub seconds: f64,
    pub viewer_connected_before_push: bool,
    pub mount: WhepMountCounters,
    pub server: WhepServerCounters,
}

/// The mount counters `serve-whep` writes (`tst_webrtc::MountStats`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhepMountCounters {
    pub frames_pushed: u64,
    pub keyframes_pushed: u64,
    pub klv_records_pushed: u64,
    pub klv_records_dropped_unbound: u64,
    pub klv_sei_inserted: u64,
    pub klv_sei_dropped_oversize: u64,
    pub klv_dc_messages_sent: u64,
    pub klv_dc_dropped_oversize: u64,
    pub klv_dc_dropped_viewer: u64,
    pub audio_frames_pushed: u64,
    pub audio_frames_dropped: u64,
    pub plis_received: u64,
    pub nacks_received: u64,
    pub viewer_packets_dropped_queue_full: u64,
    pub rtp_clock_rebases: u64,
    pub frames_pts_reordered: u64,
    pub demux_nonconformant: u64,
}

/// The server counters `serve-whep` writes (`tst_webrtc::ServerStats`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhepServerCounters {
    pub active_sessions: u64,
    pub sessions_total: u64,
    pub sessions_rejected_cap: u64,
    pub sessions_rejected_codec: u64,
    pub sessions_rejected_auth: u64,
    pub sessions_reaped_ice_timeout: u64,
    pub sessions_reaped_disconnected: u64,
    pub rtp_packets_sent: u64,
    pub rtp_bytes_sent: u64,
    pub datagrams_unrouted: u64,
}

/// The whep leg's raw evidence: the viewer loop's report and the
/// server's counters.
#[derive(Debug, Clone)]
pub struct WhepLegArtifacts {
    pub viewer: WhepViewerReport,
    pub serve: WhepServeReport,
}

/// One leg's raw evidence artifacts — `soak.sh` writes these as
/// `proxy --stats-json`'s output, `recv --json`'s output, and
/// `send --json`'s output respectively.
#[derive(Debug, Clone)]
pub struct LegArtifacts {
    pub proxy_stats: ProxyStats,
    pub recv_report: VerifyReport,
    pub send_metrics: CellMetrics,
    /// `None` for the `rist` leg (sustained impairment only, no
    /// scheduled outage — see the module doc).
    pub outage_period_s: Option<u64>,
}

/// Pure input to [`build_soak_results`].
pub struct SoakInputs {
    pub rss_samples: Vec<RssSample>,
    /// `(leg name, artifacts)` pairs — `"srt"` is always present;
    /// `"rist"` is included whenever that leg's three report files
    /// were supplied.
    pub legs: Vec<(String, LegArtifacts)>,
    /// `(leg name, artifacts)` for each publish leg (`PUBLISH_LEGS`)
    /// the run had — empty when it had none.
    pub publish_legs: Vec<(String, PublishLegArtifacts)>,
    /// `(leg name, artifacts)` for the WebRTC playback leg ([`WHEP_LEGS`])
    /// when the run had it — empty otherwise.
    pub whep_legs: Vec<(String, WhepLegArtifacts)>,
    pub rss_slope_threshold_kb_per_hour: Option<f64>,
    /// The run's declared parameters (`soak-config.json`) — judged
    /// against, never derived from, the artifacts on disk.
    pub config: SoakConfig,
    /// Worker role -> exit status, from `exits.json`.
    pub worker_exits: BTreeMap<String, i32>,
}

/// Compute every verdict from already-parsed inputs — no I/O. [`run`]
/// is the file-driven wrapper around this.
///
/// # Errors
///
/// Errors (rather than returning a clean, vacuously-passing
/// [`SoakResults`]) if `rss_samples` is completely empty — a
/// header-only `rss.csv` most likely means the sampler crashed
/// before writing a single tick, or a `--rss` path typo pointed at
/// the wrong file; either way, this must never silently read as a
/// data-free "the soak was fine" report (mirrors [`super::merge`]'s own
/// "no cells" hard error for the same reason). A `rss.csv` that HAS
/// some rows but is missing an entire expected `(leg, process)`
/// combination (or has rows for it that are all pre-warmup) is a
/// softer problem — handled instead by an explicit, non-
/// [`SoakVerdict::provisional`], FAILING `rss_data_present_<leg>_
/// <process>` verdict per combination, so `overall_pass` still goes
/// false but the rest of the report (the combinations that DO have
/// data) stays readable.
pub fn build_soak_results(inputs: SoakInputs) -> Result<SoakResults, String> {
    let SoakInputs {
        rss_samples,
        legs,
        publish_legs,
        whep_legs,
        rss_slope_threshold_kb_per_hour,
        config,
        worker_exits,
    } = inputs;

    if rss_samples.is_empty() {
        return Err(
            "rss.csv has zero data rows — refusing to write a clean pass for a data-free \
             soak run (the RSS sampler most likely crashed before its first tick, or --rss \
             points at the wrong file)"
                .to_string(),
        );
    }

    // `rss_samples` is non-empty here (the `is_empty()` hard-error
    // above already returned), so min_t/max_t are always assigned a
    // real elapsed_s from the loop below — no separate empty-case
    // branch needed.
    let run_duration_s = {
        let mut min_t = f64::INFINITY;
        let mut max_t = f64::NEG_INFINITY;
        for s in &rss_samples {
            min_t = min_t.min(s.elapsed_s);
            max_t = max_t.max(s.elapsed_s);
        }
        (max_t - min_t).max(0.0)
    };
    let warmup_s = rss_warmup_s(config.expected_duration_s, config.warmup_fraction);

    // The run's own declared achievable window: `soak.sh`'s sampler
    // deliberately stops `sampler_end_slack_s` before the nominal
    // deadline (see `RssSample`'s doc), so the longest RSS span a
    // healthy run can ever produce is `expected_duration_s -
    // sampler_end_slack_s`, not `expected_duration_s` itself.
    let achievable_span_s = config.expected_duration_s - config.sampler_end_slack_s;
    let mut verdicts = Vec::new();
    {
        // Two cadences of slack: the sampler's first tick lands at
        // t≈0 and its last up to one tick short of the achievable
        // end.
        let min_span_s = achievable_span_s - 2.0 * config.rss_cadence_s;
        verdicts.push(SoakVerdict {
            name: "duration_coverage".to_string(),
            pass: run_duration_s >= min_span_s,
            provisional: false,
            detail: format!(
                "observed RSS span {run_duration_s:.0}s against a configured {:.0}s run \
                 (achievable {achievable_span_s:.0}s after the sampler's {:.0}s end slack; \
                 minimum accepted {min_span_s:.0}s)",
                config.expected_duration_s, config.sampler_end_slack_s
            ),
        });
    }

    // An empty `rss_kb` is an exit only when the same pid never reports
    // again. One followed by a later sample of that pid is a missed read
    // (the pid was alive), recorded as a limitation and never judged.
    let resumed = |s: &RssSample| {
        rss_samples.iter().any(|t| {
            t.rss_kb.is_some()
                && t.pid == s.pid
                && t.leg == s.leg
                && t.process == s.process
                && t.elapsed_s > s.elapsed_s
        })
    };
    let sample_misses: Vec<&RssSample> = rss_samples
        .iter()
        .filter(|s| s.rss_kb.is_none() && resumed(s))
        .collect();
    let process_exits: Vec<ProcessExit> = rss_samples
        .iter()
        .filter(|s| s.rss_kb.is_none() && !resumed(s))
        .map(|s| ProcessExit {
            leg: s.leg.clone(),
            process: s.process.clone(),
            elapsed_s: s.elapsed_s,
            pid: s.pid,
        })
        .collect();

    // Post-warmup (leg, process) -> (elapsed_s, rss_kb) points.
    // BTreeMap keeps iteration (and so `rss_slopes`' output order)
    // sorted by (leg, process) without a separate sort step. Each
    // group's own points are sorted by elapsed time below —
    // `rss_samples` isn't guaranteed to arrive in timestamp order,
    // and the gap computation over `windows(2)` needs it to be.
    let mut groups: BTreeMap<(String, String), Vec<(f64, f64)>> = BTreeMap::new();
    for s in &rss_samples {
        if s.elapsed_s < warmup_s {
            continue;
        }
        if let Some(kb) = s.rss_kb {
            groups
                .entry((s.leg.clone(), s.process.clone()))
                .or_default()
                .push((s.elapsed_s, kb as f64));
        }
    }
    for points in groups.values_mut() {
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    // Every leg this run had, transport and publish alike.
    let leg_names: Vec<&str> = legs
        .iter()
        .map(|(n, _)| n.as_str())
        .chain(publish_legs.iter().map(|(n, _)| n.as_str()))
        .chain(whep_legs.iter().map(|(n, _)| n.as_str()))
        .collect();
    // Every `(leg, process)` combination this run is SUPPOSED to
    // have RSS evidence for — one entry per `leg_processes` value
    // for each leg actually present. Compared against
    // `groups`'s keys (which ones actually have >=1 post-warmup,
    // rss_kb-present sample) BEFORE `groups` is consumed below, so a
    // whole missing process (or one whose only samples are all
    // pre-warmup, or all crashed/`rss_kb: None`) gets its own
    // FAILING verdict instead of just silently absent from
    // `rss_slopes` — see [`build_soak_results`]'s `# Errors` section
    // for why a total data absence needs a harder signal than "this
    // vec happens to be shorter than expected."
    let mut missing_data: Vec<(String, String)> = Vec::new();
    for &leg_name in &leg_names {
        for &process in leg_processes(leg_name) {
            if !groups.contains_key(&(leg_name.to_string(), process.to_string())) {
                missing_data.push((leg_name.to_string(), process.to_string()));
            }
        }
    }

    // Per-(leg, process) sampling-density check, computed BEFORE
    // slopes: a group with too few post-warmup samples (or too
    // large a gap between consecutive ones) can't support a
    // trustworthy regression, so it's excluded from `rss_slopes`
    // below rather than silently reporting a slope built from a
    // couple of stray points.
    let expected_samples = ((achievable_span_s - warmup_s) / config.rss_cadence_s)
        .floor()
        .max(0.0) as u64;
    let min_samples = (expected_samples as f64 * MIN_SAMPLE_COVERAGE).ceil() as u64;
    let max_gap_s = MAX_GAP_CADENCES * config.rss_cadence_s;
    let mut coverage: Vec<RssCoverage> = Vec::new();
    for ((leg, process), points) in &groups {
        let (pass, largest_gap_s, distinct_ts) = rss_coverage(points, min_samples, max_gap_s);
        coverage.push(RssCoverage {
            leg: leg.clone(),
            process: process.clone(),
            expected_samples,
            observed_samples: points.len() as u64,
            largest_gap_s,
            pass,
        });
        verdicts.push(SoakVerdict {
            name: format!("rss_sample_coverage_{leg}_{process}"),
            pass,
            provisional: false,
            detail: format!(
                "{} post-warmup sample(s) ({} distinct timestamps) against {expected_samples} \
                 expected at a {:.0}s cadence (minimum {min_samples}); largest gap {largest_gap_s:.0}s \
                 (must be strictly under {max_gap_s:.0}s)",
                points.len(),
                distinct_ts,
                config.rss_cadence_s
            ),
        });
    }

    // Only groups whose coverage passed get a regression computed
    // over them — a slope from a handful of sparse or gappy points
    // isn't evidence of anything.
    let covered: std::collections::BTreeSet<(String, String)> = coverage
        .iter()
        .filter(|c| c.pass)
        .map(|c| (c.leg.clone(), c.process.clone()))
        .collect();
    let rss_slopes: Vec<RssSlope> = groups
        .into_iter()
        .filter(|(key, _)| covered.contains(key))
        .map(|((leg, process), points)| {
            debug_assert!(
                points.len() >= 2,
                "a coverage-passed group must have at least 2 samples"
            );
            let samples_used = points.len();
            let slope_kb_per_hour = linear_regression_slope(&points) * 3600.0;
            RssSlope {
                leg,
                process,
                slope_kb_per_hour,
                samples_used,
            }
        })
        .collect();

    for (leg, process) in &missing_data {
        verdicts.push(SoakVerdict {
            name: format!("rss_data_present_{leg}_{process}"),
            pass: false,
            provisional: false,
            detail: format!(
                "no post-warmup RSS samples for {leg}/{process} — either this process never \
                 appears in rss.csv at all, or every sample for it fell before the \
                 {warmup_s:.0}s warmup cutoff (crashed/never-sampled processes show up here, \
                 not just in zero_process_exits)"
            ),
        });
    }
    for slope in &rss_slopes {
        let provisional = rss_slope_threshold_kb_per_hour.is_none();
        let pass = match rss_slope_threshold_kb_per_hour {
            Some(threshold) => slope.slope_kb_per_hour <= threshold,
            None => true,
        };
        verdicts.push(SoakVerdict {
            name: format!("rss_slope_{}_{}", slope.leg, slope.process),
            pass,
            provisional,
            detail: format!(
                "{:.1} KiB/h over {} post-warmup sample(s)",
                slope.slope_kb_per_hour, slope.samples_used
            ),
        });
    }

    verdicts.push(SoakVerdict {
        name: "zero_process_exits".to_string(),
        pass: process_exits.is_empty(),
        provisional: false,
        detail: if process_exits.is_empty() {
            "no process disappeared from /proc before the runner's own shutdown".to_string()
        } else {
            format!(
                "{} unexpected exit(s): {:?}",
                process_exits.len(),
                process_exits
            )
        },
    });

    // `worker_exits` judges the DECLARED `exits.json` this function
    // was handed — it never runs at all if `run()`'s earlier
    // `read_leg_artifacts` call already hard-errored because a killed
    // worker never wrote its own per-leg report file (send-report/
    // recv-report/proxy-stats). Both paths end in a nonzero process
    // exit, but only this verdict names which role and status failed;
    // the hard-error path is a bare "No such file" (see
    // `SoakResults::limitations`'s last entry).
    //
    // A single-leg (srt-only) run — e.g. a local smoke test, or any
    // invocation that omits the three `--rist-*` flags per
    // `run`'s own doc comment — never launches the `rist-*` trio at
    // all, so only the roles whose leg is actually present are
    // required. The same scoping `missing_data` above already
    // applies to RSS/leg checks. A publish leg's roles are its
    // `recv` and the publisher LOOP — the publisher tool itself exits
    // at the end of every generation by design and is never a
    // worker.
    let required_roles = leg_names.iter().flat_map(|&leg_name| {
        leg_processes(leg_name)
            .iter()
            .map(move |process| format!("{leg_name}-{process}"))
    });

    let mut exit_problems: Vec<String> = Vec::new();
    for role in required_roles {
        let role = role.as_str();
        match worker_exits.get(role) {
            None => exit_problems.push(format!("{role}: no exit status recorded")),
            Some(&status) => {
                let expected = config.expected_worker_exits.get(role).copied().unwrap_or(0);
                if status != expected {
                    exit_problems.push(format!("{role}: exit {status} (expected {expected})"));
                }
            }
        }
    }
    for (role, &status) in &worker_exits {
        if !WORKER_ROLES.contains(&role.as_str()) {
            exit_problems.push(format!(
                "unknown role {role} in exits.json (status {status})"
            ));
        }
    }
    verdicts.push(SoakVerdict {
        name: "worker_exits".to_string(),
        pass: exit_problems.is_empty(),
        provisional: false,
        detail: if exit_problems.is_empty() {
            "every worker exited with its expected status".to_string()
        } else {
            exit_problems.join("; ")
        },
    });

    let mut leg_results = Vec::new();
    for (leg_name, artifacts) in &legs {
        let outage_dur_s = artifacts.proxy_stats.config.outage_dur_s;
        let outage_windows =
            expected_outage_windows(run_duration_s, artifacts.outage_period_s, outage_dur_s);

        // Phase-integrated expectation (spec §6.2), each phase's
        // rate taken from the engine's own renewal process rather
        // than its configured `loss_pct` (Q11). Fixed mode is the
        // one-phase case: the schedule echo is absent, the model is
        // built from the config's `loss_pct` as a NON-burst phase
        // (fixed mode never bursts, and there the renewal rate IS
        // that fraction scaled by the dup gate), and `phases` is a
        // single counter carrying the run's whole total.
        let dup_pct = artifacts.proxy_stats.config.dup_pct;
        let models: Vec<PhaseModel> = match &artifacts.proxy_stats.config.schedule {
            Some(s) => s
                .table
                .iter()
                .map(|ph| renewal_model(ph.draw_pct, dup_pct, ph.burst, ph.burst_run))
                .collect(),
            None => vec![renewal_model(
                artifacts.proxy_stats.config.loss_pct,
                dup_pct,
                false,
                (1, 1),
            )],
        };
        // `(forwarded, dropped, outage_dropped)` per phase. The
        // third is a SUB-COUNT of the second, so the totals still
        // reconcile against the top-level pair; the drop-rate
        // verdict below judges `dropped - outage_dropped` over
        // `forwarded + dropped - outage_dropped`, because the
        // expectation models continuous loss only and an outage
        // window drops unconditionally.
        let counters: Vec<(u64, u64, u64)> = if artifacts.proxy_stats.phases.is_empty() {
            // Pre-feature stats file (`#[serde(default)]`): no
            // per-phase split was recorded — fall back to the
            // totals, judged as one phase, exactly as before.
            vec![(
                artifacts.proxy_stats.forwarded,
                artifacts.proxy_stats.dropped,
                0,
            )]
        } else {
            artifacts
                .proxy_stats
                .phases
                .iter()
                .map(|c| (c.forwarded, c.dropped, c.outage_dropped))
                .collect()
        };
        let phase_count = counters.len();
        let total_forwarded: u64 = counters.iter().map(|(f, _, _)| f).sum();
        let total_dropped: u64 = counters.iter().map(|(_, d, _)| d).sum();
        let total_outage: u64 = counters.iter().map(|(_, _, o)| o).sum();
        // Outage-window drops leave BOTH the numerator and the
        // denominator: a packet the proxy blackholed for the window
        // was never offered to the continuous-loss process at all.
        // `o.min(d)` only because this runs BEFORE the
        // `outage_consistent` check below rejects the artifact — on
        // every path that reaches a verdict `o <= d` already holds,
        // and the clamp exists so a malformed file is reported as
        // malformed rather than underflowing here.
        let total: u64 = counters.iter().map(|(f, d, o)| f + d - o.min(d)).sum();

        // The per-phase split and the top-level totals are incremented
        // together at every site in `proxy::run`, so when a phases
        // array is present the two MUST reconcile. They are
        // independent fields in the JSON, though, so a hand-edited or
        // partially-written artifact can disagree — and since the
        // computation above reads only the per-phase sums, a
        // disagreement would otherwise go unnoticed and yield a rate
        // computed from numbers the file itself contradicts. Same
        // fail-loud stance as the length check below.
        let totals_reconcile = artifacts.proxy_stats.phases.is_empty()
            || (total_forwarded == artifacts.proxy_stats.forwarded
                && total_dropped == artifacts.proxy_stats.dropped);
        // `outage_dropped` is a sub-count of `dropped` at the one
        // site that writes it, so a phase claiming more of them than
        // drops is a hand-edited or truncated artifact — fail loud
        // rather than compute a negative packet total from it.
        let outage_consistent = counters.iter().all(|(_, d, o)| o <= d);

        let (drop_pass, expected, tolerance, observed, drop_detail) = if !totals_reconcile {
            (
                false,
                0.0,
                0.0,
                0.0,
                format!(
                    "{leg_name}: malformed artifact — per-phase counters sum to {}/{} \
                     (forwarded/dropped) but the top-level totals say {}/{}",
                    total_forwarded,
                    total_dropped,
                    artifacts.proxy_stats.forwarded,
                    artifacts.proxy_stats.dropped
                ),
            )
        } else if !outage_consistent {
            (
                false,
                0.0,
                0.0,
                0.0,
                format!(
                    "{leg_name}: malformed artifact — a phase records more outage-window \
                     drops than drops"
                ),
            )
        } else if models.len() != counters.len() {
            // A malformed artifact (e.g. hand-edited or truncated
            // stats JSON whose phase-counter count doesn't match its
            // own schedule echo) must fail loud, never panic and
            // never silently fall back to a partial computation.
            (
                false,
                0.0,
                0.0,
                0.0,
                format!(
                    "{leg_name}: malformed artifact — {} phase rate(s) in the schedule echo \
                     but {} phase counter(s) recorded",
                    models.len(),
                    phase_count
                ),
            )
        } else if total == 0 {
            (
                false,
                0.0,
                0.0,
                0.0,
                format!("{leg_name}: proxy observed zero packets — no traffic crossed it"),
            )
        } else {
            let expected_drops: f64 = counters
                .iter()
                .zip(&models)
                .map(|((f, d, o), m)| (f + d - o) as f64 * m.p)
                .sum();
            let variance: f64 = counters
                .iter()
                .zip(&models)
                .map(|((f, d, o), m)| (f + d - o) as f64 * m.var_rate)
                .sum();
            let expected = expected_drops / total as f64;
            let tolerance = drop_rate_tolerance(variance.sqrt() / total as f64);
            let observed = (total_dropped - total_outage) as f64 / total as f64;
            let pass = (observed - expected).abs() <= tolerance;
            (
                pass,
                expected,
                tolerance,
                observed,
                format!(
                    "{leg_name}: observed {:.2}%, expected {:.2}% ± {:.2}pp over {} phase(s) \
                     ({} forwarded, {} dropped, {} outage-window drop(s) excluded)",
                    observed * 100.0,
                    expected * 100.0,
                    tolerance * 100.0,
                    phase_count,
                    total_forwarded,
                    total_dropped,
                    total_outage
                ),
            )
        };

        verdicts.push(SoakVerdict {
            name: format!("drop_rate_consistent_with_impairment_{leg_name}"),
            pass: drop_pass,
            provisional: false,
            detail: drop_detail,
        });

        let observed_reconnects = artifacts.recv_report.reconnects;
        verdicts.push(SoakVerdict {
            name: format!("reconnect_count_matches_outage_count_{leg_name}"),
            pass: observed_reconnects.is_none_or(|n| n == outage_windows),
            provisional: true,
            detail: match observed_reconnects {
                Some(observed) => format!(
                    "{leg_name}: recv --managed observed {observed} successful receive-side \
                     transport rebuild(s) (`ManagedRecvTransport::reconnects_count`) against \
                     {outage_windows} expected outage window(s) over {:.1}h. That counter \
                     counts every successful factory rebuild, not outage windows directly — \
                     a single outage window CAN drive more than one rebuild if the \
                     freshly-reconnected transport breaks again before the outage fully \
                     clears (observed empirically during this fix wave's own short-outage \
                     dry-run testing at an 8s outage duration; not yet confirmed either way \
                     at the 90s outage duration this run schedule actually uses). Recorded \
                     for comparison, still provisional until enough real runs establish the \
                     actual rebuild-to-window ratio at production outage duration.",
                    run_duration_s / 3600.0
                ),
                None => format!(
                    "{leg_name}: expected {outage_windows} outage window(s) over {:.1}h; \
                     recv wasn't run with --managed on this leg (no reconnect counter \
                     available) — recorded, not verified",
                    run_duration_s / 3600.0
                ),
            },
        });

        verdicts.push(SoakVerdict {
            name: format!("recv_invariants_{leg_name}"),
            pass: artifacts.recv_report.pass,
            provisional: false,
            detail: if artifacts.recv_report.pass {
                "final tallies within expected loss bounds".to_string()
            } else {
                artifacts.recv_report.failures.join("; ")
            },
        });

        // `profile_declared`/`schedule_declared` check the run against
        // what `soak-config.json` said it would be, the same
        // declared-before-evidence stance `duration_coverage` and
        // `worker_exits` take. Both are cheap and both close a silent
        // mis-publication: a run that exercised a different wire shape
        // (or a different impairment) than its own evidence page
        // claims would otherwise pass every other verdict.
        let declared = config.legs.get(leg_name.as_str());
        verdicts.push(profile_declared_verdict(
            leg_name,
            declared,
            &artifacts.recv_report,
        ));

        // The echo carries the generated phase table too, but only its
        // three inputs are declared — the table is a pure function of
        // them (`impair::generate_schedule`), so matching the inputs
        // matches the table.
        let declared_schedule = declared.and_then(|d| d.schedule.as_ref());
        let echoed_schedule =
            artifacts
                .proxy_stats
                .config
                .schedule
                .as_ref()
                .map(|s| ScheduleDeclaration {
                    seed: s.seed,
                    phases: s.phases,
                    phase_s: s.phase_s,
                });
        let (schedule_pass, schedule_detail) = match (declared_schedule, &echoed_schedule) {
            (None, None) => (
                true,
                format!(
                    "{leg_name}: no schedule declared and the proxy ran in fixed-impairment \
                     mode"
                ),
            ),
            (Some(d), Some(e)) if d == e => (
                true,
                format!(
                    "{leg_name}: proxy ran the declared schedule (seed {}, {} phase(s) of {}s)",
                    e.seed, e.phases, e.phase_s
                ),
            ),
            (Some(d), Some(e)) => (
                false,
                format!(
                    "{leg_name}: config declared schedule seed {} / {} phase(s) / {}s but the \
                     proxy echoed seed {} / {} phase(s) / {}s",
                    d.seed, d.phases, d.phase_s, e.seed, e.phases, e.phase_s
                ),
            ),
            (Some(d), None) => (
                false,
                format!(
                    "{leg_name}: config declared schedule seed {} / {} phase(s) / {}s but the \
                     proxy ran in fixed-impairment mode (no schedule echo)",
                    d.seed, d.phases, d.phase_s
                ),
            ),
            (None, Some(e)) => (
                false,
                format!(
                    "{leg_name}: proxy ran schedule seed {} / {} phase(s) / {}s that the \
                     config never declared",
                    e.seed, e.phases, e.phase_s
                ),
            ),
        };
        verdicts.push(SoakVerdict {
            name: format!("schedule_declared_{leg_name}"),
            pass: schedule_pass,
            provisional: false,
            detail: schedule_detail,
        });

        // Third declaration check, same stance as the two above. A
        // rich run whose receiver was launched in compact mode runs
        // NO rich-KLV oracle at all: `klv_rich` comes back `None`,
        // every rich verdict is skipped, and the run passes while
        // checking none of the content contract the evidence page
        // says it checks. The observable is exactly that block's
        // presence.
        verdicts.push(klv_declared_verdict(
            leg_name,
            declared,
            &artifacts.recv_report,
        ));

        // Fourth declaration check. The two reconnect modes spend an
        // outage differently — `blocking` stalls the producer and
        // replays the backlog, `background` keeps producing into a
        // bounded buffer that drops — so a run in the wrong mode
        // exercised something other than what its evidence says, and
        // every other verdict would still pass. Only the declared
        // side decides whether there is anything to check: a leg
        // that declared no mode has no managed sender, or predates
        // the declaration.
        let managed_send = artifacts.send_metrics.managed_send.as_ref();
        let observed_mode = managed_send.and_then(|m| m.reconnect_mode.as_deref());
        let declared_mode = declared.and_then(|d| d.reconnect_mode.as_deref());
        let (mode_pass, mode_detail) = match (declared_mode, observed_mode) {
            (None, None) => (
                true,
                format!(
                    "{leg_name}: not applicable — no reconnect mode declared (a leg whose \
                     sender is not managed, or a config that predates the declaration)"
                ),
            ),
            (None, Some(o)) => (
                true,
                format!(
                    "{leg_name}: not applicable — no reconnect mode declared (a config that \
                     predates the declaration); the send report says {o:?}"
                ),
            ),
            (Some(d), Some(o)) if d == o => (
                true,
                format!("{leg_name}: the managed sender ran the declared reconnect mode {o:?}"),
            ),
            (Some(d), Some(o)) => (
                false,
                format!(
                    "{leg_name}: config declared reconnect mode {d:?} but the send report \
                     says the managed sender ran {o:?}"
                ),
            ),
            (Some(d), None) if managed_send.is_some() => (
                false,
                format!(
                    "{leg_name}: config declared reconnect mode {d:?} but the send report's \
                     managed_send block carries no reconnect mode (written before `send` \
                     recorded it, or by a mode this harness has no name for)"
                ),
            ),
            (Some(d), None) => (
                false,
                format!(
                    "{leg_name}: config declared reconnect mode {d:?} but the send report \
                     carries no managed_send block (send ran without --managed?)"
                ),
            ),
        };
        verdicts.push(SoakVerdict {
            name: format!("reconnect_mode_declared_{leg_name}"),
            pass: mode_pass,
            provisional: false,
            detail: mode_detail,
        });

        // `corruption_attributed`/`corruption_detected`/`corruption_recovered`
        // judge the recv side's `AttributionReport` (present iff the
        // capture was made with `recv --corruption-log`). Declared-off
        // (`!config.corruption`) yields passing "disabled" verdicts so a
        // corruption-free run's `soak-results.json` isn't cluttered with
        // failures for a check that was never meant to run; declared-on
        // with no attribution present is a real config/harness mismatch
        // (the tap was declared but recv never got a `--corruption-log`)
        // and fails loud rather than silently reading as "no corruption
        // observed".
        let corr = artifacts
            .recv_report
            .metrics
            .corruption_attribution
            .as_ref();
        let mk = |name: &str, pass: bool, detail: String| SoakVerdict {
            name: format!("{name}_{leg_name}"),
            pass,
            provisional: false,
            detail,
        };
        if !config.corruption {
            for n in CORRUPTION_VERDICTS {
                verdicts.push(mk(
                    n,
                    true,
                    format!("{leg_name}: corruption tap disabled in soak-config.json"),
                ));
            }
        } else {
            match corr {
                None => {
                    for n in CORRUPTION_VERDICTS {
                        verdicts.push(mk(
                            n,
                            false,
                            format!(
                                "{leg_name}: corruption declared but recv report carries no \
                                 attribution (recv ran without --corruption-log?)"
                            ),
                        ));
                    }
                }
                Some(a) => {
                    // Every count below is the report's UNCAPPED
                    // counter, never the length of the sample list
                    // beside it. Those lists stop at `MAX_SAMPLES`,
                    // so a check reading `len()` would tell the
                    // reader of a 72-hour leg that it had 64
                    // unexplained events when it had thousands — and,
                    // worse, would PASS once the cap was full of
                    // events the lossy tier had already excused.
                    verdicts.push(mk(
                        "corruption_attributed",
                        a.unexplained_total() == 0,
                        format!(
                            "{leg_name}: {} event(s), {} attributed, {} unexplained ({} \
                             resync / {} discontinuity / {} non-conformance; first: {:?}), {} \
                             excused as transport loss; {} injected / {} resolved / {} \
                             unresolved",
                            a.events,
                            a.attributed_events,
                            a.unexplained_total(),
                            a.unexplained_resyncs,
                            a.unexplained_discontinuities,
                            a.unexplained_nonconformant,
                            a.unexplained_events.first(),
                            a.unexplained_transport_loss,
                            a.injected,
                            a.resolved,
                            a.unresolved
                        ),
                    ));
                    verdicts.push(mk(
                        "corruption_detected",
                        a.undetected_total() == 0,
                        format!(
                            "{leg_name}: {} detectable injection(s), {} undetected (first: \
                             {:?}), {} excused as lost in transit; {} noticed by the demuxer, \
                             {} only by the harness's raw reader ({:?})",
                            a.detectable,
                            a.undetected_total(),
                            a.undetected.first(),
                            a.undetected_lost,
                            a.detected_by_demux,
                            a.detected_by_reader_only,
                            a.detected_by_reader_only_per_class
                        ),
                    ));
                    verdicts.push(mk(
                        "corruption_recovered",
                        a.unrecovered_total() == 0,
                        format!(
                            "{leg_name}: {} unrecovered within {} packets (first: {:?}), {} \
                             excused as lost in transit",
                            a.unrecovered_total(),
                            a.recovery_bound,
                            a.unrecovered.first(),
                            a.unrecovered_lost
                        ),
                    ));

                    // The three verdicts above all pass on an EMPTY
                    // finding list, so a leg that injected nothing —
                    // a tap that silently did no work, a receiver
                    // that never ingested the log, a sender killed
                    // before its first injection — passes every one
                    // of them while proving nothing. This is the
                    // verdict that makes them mean something:
                    // injections actually happened, the receiver read
                    // the whole log, and most of them were placed on
                    // the receiver's own timeline.
                    //
                    // The 90% resolution floor rather than 100%:
                    // injections logged during an outage window
                    // resolve against a PCR anchor the receiver never
                    // saw and are deliberately STRANDED rather than
                    // guessed at (`corrupt::Attribution::on_pcr`), so
                    // a run with outages legitimately leaves a few
                    // unresolved. A leg resolving less than nine in
                    // ten is not absorbing outages, it is failing to
                    // follow the stream.
                    // Both halves are FLOORS, not equalities. The
                    // ingested one used to demand `a.injected ==
                    // sent`, which over 72 hours would fail a whole
                    // run on one missed tail read — the receiver
                    // drains the log once more after its loop ends,
                    // but a single unlucky interleaving at teardown
                    // is not evidence of anything. A leg that read 99
                    // of every 100 injections judged the run; one
                    // that read 90 did not.
                    let sender = artifacts.send_metrics.corruption.as_ref();
                    let sent = sender.map_or(0, |c| c.injections);
                    let resolved_frac = if a.injected == 0 {
                        0.0
                    } else {
                        a.resolved as f64 / a.injected as f64
                    };
                    let ingested_frac = if sent == 0 {
                        0.0
                    } else {
                        a.injected as f64 / sent as f64
                    };
                    // Ceiling, so the floor cannot be met by rounding
                    // on a small run: 99% of 101 needs 100, not 99.
                    let min_ingested = (sent as f64 * CORRUPTION_INGESTED_FLOOR).ceil() as u64;
                    let log_failed = sender.is_some_and(|c| c.log_write_failed);
                    let pass = sent > 0
                        && !log_failed
                        && a.injected >= min_ingested
                        && resolved_frac >= CORRUPTION_RESOLVED_FLOOR;
                    verdicts.push(mk(
                        "corruption_coverage",
                        pass,
                        format!(
                            "{leg_name}: sender logged {sent} injection(s){}, receiver ingested \
                             {} of them ({:.1}%, floor {:.0}% = {min_ingested}), {} resolved to \
                             a receiver position ({:.1}%, floor {:.0}%), {} unresolved, {} \
                             anchor samples rejected",
                            match sender {
                                Some(c) if c.log_write_failed =>
                                    " (sender reported a corruption-log write failure — the \
                                      log is incomplete)"
                                        .to_string(),
                                Some(_) => String::new(),
                                None => " (send report carries no corruption block — sender ran \
                                      without --corrupt?)"
                                    .to_string(),
                            },
                            a.injected,
                            ingested_frac * 100.0,
                            CORRUPTION_INGESTED_FLOOR * 100.0,
                            a.resolved,
                            // Floored, not rounded: a run that resolved
                            // 999 of 1000 is 99.9%, never "100.0%" —
                            // rounding would hide the last unresolved
                            // injection behind a false full mark.
                            (resolved_frac * 1000.0).floor() / 10.0,
                            CORRUPTION_RESOLVED_FLOOR * 100.0,
                            a.unresolved,
                            a.anchors_rejected
                        ),
                    ));

                    // And the declaration half: the spec the receiver
                    // read out of the sender's log header must be the
                    // spec the config said this run would inject. A
                    // run that quietly injected at a tenth of the
                    // declared rate, or from a narrower class set,
                    // would otherwise pass every verdict above while
                    // its published evidence described a different
                    // experiment.
                    let observed = CorruptionDeclaration {
                        rate_per_10k: a.rate_per_10k,
                        min_gap: a.min_gap,
                        classes: a.classes.clone(),
                    };
                    let (spec_pass, spec_detail) = match &config.corruption_spec {
                        None => (
                            true,
                            format!(
                                "{leg_name}: no corruption spec declared (pre-realism config); \
                                 the sender's log header says rate {}/10k, min_gap {}, {} \
                                 class(es)",
                                observed.rate_per_10k,
                                observed.min_gap,
                                observed.classes.len()
                            ),
                        ),
                        Some(d) if *d == observed => (
                            true,
                            format!(
                                "{leg_name}: sender injected the declared spec (rate {}/10k, \
                                 min_gap {}, {} class(es))",
                                d.rate_per_10k,
                                d.min_gap,
                                d.classes.len()
                            ),
                        ),
                        Some(d) => (
                            false,
                            format!(
                                "{leg_name}: config declared rate {}/10k, min_gap {}, classes \
                                 {:?} but the sender's log header says rate {}/10k, min_gap \
                                 {}, classes {:?}",
                                d.rate_per_10k,
                                d.min_gap,
                                d.classes,
                                observed.rate_per_10k,
                                observed.min_gap,
                                observed.classes
                            ),
                        ),
                    };
                    verdicts.push(mk("corruption_declared", spec_pass, spec_detail));

                    // Q12: under lossy judgement the three finding
                    // verdicts EXCUSE — undetected/unrecovered
                    // injections with a foreign jump in their window,
                    // and every unexplained discontinuity — so a leg
                    // whose evidence was all excused passes them
                    // vacuously. Both soak legs are ARQ (`?latency=` /
                    // `?buffer=`): the transport recovers continuous
                    // loss, so outage windows and the per-hour residue
                    // an ARQ transport does not fully repair are what
                    // legitimately leave gaps. Budget the excusals by
                    // the schedule AND by how long the run lasted —
                    // see `excusal_budget`.
                    let profile = artifacts.recv_report.profile.as_deref().or(config
                        .legs
                        .get(leg_name.as_str())
                        .map(|d| d.profile.as_str()));
                    let k = media_pids(profile);
                    let budget = excusal_budget(
                        k,
                        outage_windows,
                        run_duration_s,
                        config.expected_duration_s,
                    );
                    // Same expression `excusal_budget` uses, `.max(0.0)`
                    // included, so the detail can never quote hours the
                    // budget was not computed from.
                    let budget_hours = if run_duration_s > 0.0 {
                        run_duration_s
                    } else {
                        config.expected_duration_s
                    }
                    .max(0.0)
                        / 3600.0;
                    let excused =
                        a.unexplained_transport_loss + a.undetected_lost + a.unrecovered_lost;
                    let unexplained_disc = artifacts
                        .recv_report
                        .metrics
                        .discontinuities
                        .saturating_sub(a.attributed_discontinuities);
                    verdicts.push(mk(
                        "corruption_excusal_budget",
                        excused <= budget && unexplained_disc <= budget,
                        format!(
                            "{leg_name}: {excused} excused ({} transport-loss events, of \
                             which {} PCR jumps beside a gap, {} undetected-lost, of which \
                             {} in a reconnect gap, {} unrecovered-lost) and \
                             {unexplained_disc} unexplained discontinuities against budget \
                             {budget} (K={k} media PIDs x {outage_windows} outage window(s) \
                             + {EXCUSAL_BUDGET_BASE} + {EXCUSAL_PER_HOUR}/h x \
                             {budget_hours:.2}h)",
                            a.unexplained_transport_loss,
                            a.pcr_anomalies_excused,
                            a.undetected_lost,
                            a.lost_in_reconnect_gap,
                            a.unrecovered_lost
                        ),
                    ));
                }
            }
        }

        leg_results.push(LegResult {
            leg: leg_name.clone(),
            outage_period_s: artifacts.outage_period_s,
            outage_dur_s,
            expected_outage_windows: outage_windows,
            proxy_forwarded: artifacts.proxy_stats.forwarded,
            proxy_dropped: artifacts.proxy_stats.dropped,
            loss_pct: expected * 100.0,
            observed_drop_fraction: observed,
            expected_drop_fraction: expected,
            drop_fraction_tolerance: tolerance,
            phases: phase_count,
            recv_pass: artifacts.recv_report.pass,
            recv_failures: artifacts.recv_report.failures.clone(),
            send_video_aus: artifacts.send_metrics.video_aus,
            recv_video_aus: artifacts.recv_report.metrics.video_aus,
            corruption_injected: corr.map(|a| a.injected),
            managed_send: managed_send.cloned(),
        });
    }

    // Publish legs. No proxy and no tst-interop sender, so the
    // drop-rate, reconnect-count and corruption verdicts have nothing
    // to judge; the declaration checks that DO apply run as on a
    // transport leg, the ones that cannot say why, and the leg's own
    // exercise — the publisher dropping and re-ANNOUNCing on a
    // schedule — is `publisher_generations_<leg>`.
    let mut publish_leg_results = Vec::new();
    for (leg_name, artifacts) in &publish_legs {
        let recv = &artifacts.recv_report;
        let declared = config.legs.get(leg_name.as_str());
        verdicts.push(SoakVerdict {
            name: format!("recv_invariants_{leg_name}"),
            pass: recv.pass,
            provisional: false,
            detail: if recv.pass {
                "final tallies within expected bounds across every publisher generation".to_string()
            } else {
                recv.failures.join("; ")
            },
        });
        verdicts.push(profile_declared_verdict(leg_name, declared, recv));
        verdicts.push(klv_declared_verdict(leg_name, declared, recv));

        let schedule = declared.and_then(|d| d.schedule.as_ref());
        verdicts.push(SoakVerdict {
            name: format!("schedule_declared_{leg_name}"),
            pass: schedule.is_none(),
            provisional: false,
            detail: match schedule {
                None => format!(
                    "{leg_name}: not applicable — this leg has no impairment proxy (the \
                     publisher pushes to the mount over loopback TCP) and declares no schedule"
                ),
                Some(d) => format!(
                    "{leg_name}: config declared schedule seed {} / {} phase(s) / {}s, but \
                     this leg has no proxy to run it",
                    d.seed, d.phases, d.phase_s
                ),
            },
        });

        let mode = declared.and_then(|d| d.reconnect_mode.as_deref());
        verdicts.push(SoakVerdict {
            name: format!("reconnect_mode_declared_{leg_name}"),
            pass: mode.is_none(),
            provisional: false,
            detail: match mode {
                None => format!(
                    "{leg_name}: not applicable — the publisher is an external tool, not a \
                     managed tst-interop sender; its drops are what \
                     publisher_generations_{leg_name} judges"
                ),
                Some(m) => format!(
                    "{leg_name}: config declared reconnect mode {m:?}, but this leg has no \
                     managed sender to run it"
                ),
            },
        });

        for n in CORRUPTION_VERDICTS {
            verdicts.push(SoakVerdict {
                name: format!("{n}_{leg_name}"),
                pass: true,
                provisional: false,
                detail: format!(
                    "{leg_name}: not applicable — the publisher replays a generated stream \
                     with no corruption tap, so there is no injection log to judge against"
                ),
            });
        }

        // Judged against the generations the run DECLARED it launched,
        // not a count re-derived from the run length. The mount counts
        // a publisher once its session has ENDED, and every one must
        // have: `soak.sh` stops the stream 30 + N s before the run, so
        // the last publisher finishes before `recv` snapshots the mount
        // at its deadline. One still connected there would be cut off,
        // exit nonzero and fail `worker_exits` as well.
        let period = declared.and_then(|d| d.publisher_drop_period_s);
        let launched = declared.and_then(|d| d.publisher_generations);
        let min_generations = launched;
        let observed = recv.publish_mount.as_ref().map(|m| m.generation);
        let (gen_pass, gen_detail) = match (launched, observed) {
            (Some(n), Some(g)) => (
                g >= n,
                format!(
                    "{leg_name}: {g} publisher(s) ended on the mount, against a floor of \
                     {n} = every declared generation. The stream stops 30 + {n} s before the \
                     run, so the last publisher ends before recv's deadline; one cut off \
                     there would also exit nonzero and fail worker_exits. Each generation \
                     ends gracefully — EOS, then the session closes — so an abrupt publisher \
                     loss is not exercised here"
                ),
            ),
            (Some(_), None) => (
                false,
                format!(
                    "{leg_name}: the recv report carries no publish_mount block, so nothing \
                     says how many publishers came and went (recv ran on a non-publish URL, or \
                     predates the block)"
                ),
            ),
            _ => (
                false,
                format!(
                    "{leg_name}: no publisher_generations declared for this publish leg, \
                     so there is no floor to judge the mount's generation count against"
                ),
            ),
        };
        verdicts.push(SoakVerdict {
            name: format!("publisher_generations_{leg_name}"),
            pass: gen_pass,
            provisional: false,
            detail: gen_detail,
        });

        // Every byte of every generation arrived, in order. The
        // generations concatenate to the source byte for byte, so the
        // receiver's whole-capture digest equals the source's exactly
        // when nothing was lost, cut short or reordered. The count
        // floors in `recv_invariants` cannot see a lost LAST
        // generation (no discontinuity follows it, and the rest still
        // clears the floor); this can.
        //
        // A remuxed leg's bytes change in flight, so its digest says
        // nothing; what must survive the re-mux is every KLV record of
        // every generation, with the mount itself reporting no KLV
        // unit dropped and some access unit emitted.
        let remuxed = declared.and_then(|d| d.remuxed) == Some(true);
        let got = &recv.metrics.stream_sha256;
        let source = match declared.and_then(|d| d.source_sha256.as_deref()) {
            Some(d) => Some((d, "declared in soak-config.json")),
            None => artifacts
                .source_sha256_file
                .as_deref()
                .map(|d| (d, "declared via the source.sha256 artefact")),
        };
        verdicts.push(match source {
            _ if remuxed => remuxed_delivery_verdict(leg_name, declared, recv),
            Some((want, how)) => SoakVerdict {
                name: format!("delivery_complete_{leg_name}"),
                pass: got == want,
                provisional: false,
                detail: format!(
                    "{leg_name}: recv stream_sha256 {got} against the source's {want} \
                     ({how}){}",
                    if got == want {
                        ""
                    } else {
                        " — some generation's bytes were lost, cut short or altered"
                    }
                ),
            },
            None => SoakVerdict {
                name: format!("delivery_complete_{leg_name}"),
                pass: false,
                provisional: false,
                detail: format!(
                    "{leg_name}: no source digest declared (no legs.{leg_name}.source_sha256 \
                     in soak-config.json and no source.sha256 artefact), so nothing says \
                     whether every generation's bytes arrived"
                ),
            },
        });

        verdicts.push(klv_placement_verdict(leg_name, declared, recv));

        publish_leg_results.push(PublishLegResult {
            leg: leg_name.clone(),
            publisher_drop_period_s: period,
            min_generations,
            publish_mount: recv.publish_mount.clone(),
            recv_pass: recv.pass,
            recv_failures: recv.failures.clone(),
            recv_video_aus: recv.metrics.video_aus,
        });
    }

    let mut whep_leg_results = Vec::new();
    for (leg_name, artifacts) in &whep_legs {
        let declared = config.legs.get(leg_name.as_str());
        let viewer = &artifacts.viewer;
        let serve = &artifacts.serve;

        verdicts.push(whep_profile_declared_verdict(leg_name, declared, serve));

        // The same not-applicable family a publish leg gets, so the
        // verdict set reads the same for every leg: a declared schedule
        // or reconnect mode is a declaration nothing here could run.
        let schedule = declared.and_then(|d| d.schedule.as_ref());
        verdicts.push(SoakVerdict {
            name: format!("schedule_declared_{leg_name}"),
            pass: schedule.is_none(),
            provisional: false,
            detail: match schedule {
                None => format!(
                    "{leg_name}: not applicable — this leg has no impairment proxy (the \
                     browser plays the mount over loopback) and declares no schedule"
                ),
                Some(d) => format!(
                    "{leg_name}: config declared schedule seed {} / {} phase(s) / {}s, but \
                     this leg has no proxy to run it",
                    d.seed, d.phases, d.phase_s
                ),
            },
        });
        let mode = declared.and_then(|d| d.reconnect_mode.as_deref());
        verdicts.push(SoakVerdict {
            name: format!("reconnect_mode_declared_{leg_name}"),
            pass: mode.is_none(),
            provisional: false,
            detail: match mode {
                None => format!(
                    "{leg_name}: not applicable — the server is fed in-process and the \
                     browser reconnects on the viewer loop's schedule, which \
                     whep_reconnects_observed judges"
                ),
                Some(m) => format!(
                    "{leg_name}: config declared reconnect mode {m:?}, but this leg has no \
                     managed sender to run it"
                ),
            },
        });
        for n in CORRUPTION_VERDICTS {
            verdicts.push(SoakVerdict {
                name: format!("{n}_{leg_name}"),
                pass: true,
                provisional: false,
                detail: format!(
                    "{leg_name}: not applicable — the server is fed the generator's schedule \
                     in-process with no corruption tap, so there is no injection log to \
                     judge against"
                ),
            });
        }

        verdicts.push(whep_reconnects_verdict(leg_name, declared, viewer, serve));
        let (frames_verdict, frames_decoded) = whep_frames_verdict(leg_name, viewer, serve);
        verdicts.push(frames_verdict);
        let (klv_verdict, unpaired_at_edges) = whep_klv_verdict(leg_name, viewer, serve);
        verdicts.push(klv_verdict);

        whep_leg_results.push(WhepLegResult {
            leg: leg_name.clone(),
            mount_kind: declared.and_then(|d| d.mount_kind.clone()),
            viewer_reconnect_s: declared.and_then(|d| d.viewer_reconnect_s),
            viewer_connections_declared: declared.and_then(|d| d.viewer_connections),
            connections_observed: viewer.connections.len() as u64,
            server_sessions_total: serve.server.sessions_total,
            frames_decoded,
            klv_sei_records: viewer.connections.iter().map(|c| c.klv_sei).sum(),
            klv_dc_records: viewer.connections.iter().map(|c| c.klv_dc).sum(),
            klv_copies_disagreeing: viewer
                .connections
                .iter()
                .map(|c| c.klv_copies_disagreeing)
                .sum(),
            klv_unpaired_at_edges: unpaired_at_edges,
            mount: serve.mount.clone(),
            server: serve.server.clone(),
        });
    }

    // A declared leg must be judged. Without this, a `report soak`
    // run by hand that omits a leg's artefacts drops every one of its
    // verdicts and its roles from `required_roles`, and the run can
    // pass with the leg never looked at.
    for leg in config.legs.keys() {
        if !leg_names.contains(&leg.as_str()) {
            verdicts.push(SoakVerdict {
                name: format!("leg_evidence_{leg}"),
                pass: false,
                provisional: false,
                detail: format!(
                    "{leg}: declared in soak-config.json but no report supplied, so none of \
                     its verdicts ran"
                ),
            });
        }
    }

    let overall_pass = verdicts.iter().all(|v| v.provisional || v.pass);

    Ok(SoakResults {
        run_duration_s,
        expected_duration_s: config.expected_duration_s,
        warmup_s,
        rss_slope_threshold_kb_per_hour,
        rss_slopes,
        coverage,
        process_exits,
        worker_exits,
        legs: leg_results,
        publish_legs: publish_leg_results,
        whep_legs: whep_leg_results,
        verdicts,
        overall_pass,
        limitations: sample_miss_limitations(&sample_misses)
            .into_iter()
            .chain(limitations_with_gate(
                config.memory_gate.as_ref(),
                vec![
                "A managed sender's own reconnect counters (legs[].managed_send's \
             reconnect_attempts / reconnect_successes) are recorded, not gated. \
             recv --managed's ManagedRecvTransport::reconnects_count IS a real observed \
             count, but a single outage window has not been confirmed to always drive \
             exactly one rebuild (a short 8s-outage dry-run observed a second rebuild \
             shortly after the first, before the real 90s-outage-duration ratio could be \
             established over enough runs), so reconnect_count_matches_outage_count_* \
             verdicts stay provisional and never gate the run even when a real count is \
             present."
                    .to_string(),
                "recv's VerifyReport carries no per-event delivery-gap timestamps; \
             drop_rate_consistent_with_impairment_* verdicts approximate 'gaps confined to \
             outage windows' via an aggregate drop-rate check against the proxy's cumulative \
             counters instead of a per-event ±30s timestamp localization."
                    .to_string(),
                "Outage-window drops (including reconnect handshake attempts that land while \
             the window is still active) are counted apart by the proxy \
             (PhaseCounters::outage_dropped) and excluded from both sides of the \
             drop_rate_consistent_with_impairment_* comparison, so an outage-bearing leg is \
             held to the same continuous-loss expectation as one without. A stats file \
             written before that counter existed reports 0 of them and is judged the old \
             way, with the ~0.02-0.2 percentage-point outage excess it never modelled."
                    .to_string(),
                "A worker killed before it writes its own per-leg report artifact (send-report/\
             recv-report/proxy-stats) makes report soak hard-error (exit 2) instead of \
             reaching this function and producing a worker_exits verdict at all — both \
             outcomes are nonzero, but only the latter names which role and status failed."
                    .to_string(),
                "legs[].managed_send's gap_messages_dropped / gap_bytes_dropped are recorded, \
             not gated, and are not the whole of what an outage cost: they count what the \
             managed sender's gap buffer evicted once it knew the link was down. What the \
             transport had already accepted and never delivered in the seconds before it \
             noticed the break is in neither counter, and has been measured as most of an \
             outage's loss. The difference between send_video_aus and recv_video_aus is \
             the fuller figure. gap_len_at_exit is the backlog stranded at exit, recorded \
             beside the drop counters and not gated."
                    .to_string(),
            ],
            ))
            .collect(),
    })
}

/// The `sample_misses` limitation: empty RSS reads of a pid that reported
/// again afterwards. Absent when there were none.
fn sample_miss_limitations(misses: &[&RssSample]) -> Option<String> {
    if misses.is_empty() {
        return None;
    }
    let list: Vec<String> = misses
        .iter()
        .map(|s| {
            format!(
                "{}/{} pid {} at {:.0} s",
                s.leg, s.process, s.pid, s.elapsed_s
            )
        })
        .collect();
    Some(format!(
        "sample_misses: {} empty RSS read(s) of a process that reported again afterwards, \
         so not exits and not judged: {}",
        misses.len(),
        list.join(", ")
    ))
}

/// [`build_soak_results`]'s fixed limitations, plus the declared
/// memory gate's line when the config carries one.
fn limitations_with_gate(gate: Option<&MemoryGate>, mut limitations: Vec<String>) -> Vec<String> {
    if let Some(g) = gate {
        limitations.push(format!(
            "memory gate: predicted {} MB x headroom {} <= budget {} MB (MemTotal {} MB); \
             checked by soak.sh at launch, recorded here for provenance, not a verdict",
            g.predicted_kb / 1024,
            g.headroom,
            g.budget_kb / 1024,
            g.mem_total_kb / 1024
        ));
    }
    limitations
}

/// `delivery_complete_<leg>` for a [`LegDeclaration::remuxed`] publish
/// leg: the receiver counted every KLV record of every declared
/// generation, and the mount dropped none and emitted access units.
fn remuxed_delivery_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    recv: &VerifyReport,
) -> SoakVerdict {
    let name = format!("delivery_complete_{leg_name}");
    let per = declared.and_then(|d| d.klv_records_per_generation);
    let generations = declared.and_then(|d| d.publisher_generations);
    let (Some(per), Some(generations)) = (per, generations) else {
        return SoakVerdict {
            name,
            pass: false,
            provisional: false,
            detail: format!(
                "{leg_name}: remuxed, but klv_records_per_generation or \
                 publisher_generations is not declared, so there is no KLV count to judge \
                 delivery against"
            ),
        };
    };
    let Some(mount) = recv.publish_mount.as_ref() else {
        return SoakVerdict {
            name,
            pass: false,
            provisional: false,
            detail: format!(
                "{leg_name}: the recv report carries no publish_mount block, so nothing \
                 says whether the mount dropped KLV units"
            ),
        };
    };
    let want = per.saturating_mul(generations);
    let got = recv.metrics.klv_records;
    let pass = got == want && mount.klv_units_dropped == 0 && mount.aus_emitted > 0;
    SoakVerdict {
        name,
        pass,
        provisional: false,
        detail: format!(
            "{leg_name}: recv counted {got} KLV record(s) against {want} = {per} per \
             generation x {generations} declared generation(s); the mount reported \
             klv_units_dropped {} and aus_emitted {} (remuxed: the stream digest is not \
             judged){}",
            mount.klv_units_dropped,
            mount.aus_emitted,
            if pass {
                ""
            } else {
                " — KLV records were lost, duplicated or dropped by the mount, or no \
                 access unit was emitted"
            }
        ),
    }
}

/// `klv_placement_<leg>` for a publish leg: when the leg declares
/// [`LegDeclaration::klv_placement`], its receiver's placement block
/// shows every record within tolerance, no unexpected reset, and one
/// generation per declared publisher generation. A leg that does not
/// declare the oracle passes as not applicable.
fn klv_placement_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    recv: &VerifyReport,
) -> SoakVerdict {
    let name = format!("klv_placement_{leg_name}");
    if declared.and_then(|d| d.klv_placement) != Some(true) {
        return SoakVerdict {
            name,
            pass: true,
            provisional: false,
            detail: format!(
                "{leg_name}: not applicable — this leg does not declare the placement oracle"
            ),
        };
    }
    let Some(m) = recv.metrics.klv_placement.as_ref() else {
        return SoakVerdict {
            name,
            pass: false,
            provisional: false,
            detail: format!(
                "{leg_name}: the recv report carries no klv_placement block (recv ran \
                 without --klv-placement)"
            ),
        };
    };
    let generations = declared.and_then(|d| d.publisher_generations);
    let pass =
        m.out_of_tolerance == 0 && m.unexpected_resets == 0 && Some(m.generations) == generations;
    SoakVerdict {
        name,
        pass,
        provisional: false,
        detail: format!(
            "{leg_name}: {} record(s) placed, {} out of tolerance (max error {} ticks, \
             tolerance {} ticks), {} unexpected reset(s), {} generation(s) seen against {} \
             declared; the generation budget excused {} backward PTS step(s) and {} timing \
             anomaly event(s){}",
            m.records,
            m.out_of_tolerance,
            m.max_abs_error_ticks,
            m.tolerance_ticks,
            m.unexpected_resets,
            m.generations,
            generations.map_or_else(|| "none".to_string(), |n| n.to_string()),
            m.generation_resets_excused,
            m.timing_anomalies_excused,
            m.first_problem
                .as_deref()
                .map_or_else(String::new, |p| format!("; first problem: {p}"))
        ),
    }
}

/// `profile_declared_whep`: the server's counters name the profile and
/// the mount kind `soak-config.json` declared for the leg. The server
/// writes both from its own options, the way a recv report carries the
/// profile it judged against.
fn whep_profile_declared_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    serve: &WhepServeReport,
) -> SoakVerdict {
    let name = format!("profile_declared_{leg_name}");
    let Some(d) = declared else {
        return SoakVerdict {
            name,
            pass: true,
            provisional: false,
            detail: format!(
                "{leg_name}: no legs.{leg_name} declaration in soak-config.json (config \
                 predates the declaration) — nothing to check against"
            ),
        };
    };
    let (Some(profile), Some(kind)) = (serve.profile.as_deref(), serve.mount_kind.as_deref())
    else {
        return SoakVerdict {
            name,
            pass: false,
            provisional: false,
            detail: format!(
                "{leg_name}: config declared profile {:?} / mount kind {:?}, but the server's \
                 counters name no profile or mount kind (written before serve-whep recorded \
                 them), so nothing says what was served",
                d.profile, d.mount_kind
            ),
        };
    };
    let pass = profile == d.profile && Some(kind) == d.mount_kind.as_deref();
    SoakVerdict {
        name,
        pass,
        provisional: false,
        detail: format!(
            "{leg_name}: config declared profile {:?} / mount kind {:?}; the server served \
             {profile:?} / {kind:?}",
            d.profile, d.mount_kind
        ),
    }
}

/// `whep_reconnects_observed`: the viewer loop made exactly the declared
/// number of connections, on the declared slot, and the server saw
/// exactly that many sessions — two independent counts of the same
/// reconnects, one per side.
fn whep_reconnects_verdict(
    leg_name: &str,
    declared: Option<&LegDeclaration>,
    viewer: &WhepViewerReport,
    serve: &WhepServeReport,
) -> SoakVerdict {
    let name = format!("whep_reconnects_observed_{leg_name}");
    let observed = viewer.connections.len() as u64;
    let sessions = serve.server.sessions_total;
    let (Some(expected), Some(slot)) = declared
        .map(|d| (d.viewer_connections, d.viewer_reconnect_s))
        .unwrap_or((None, None))
    else {
        return SoakVerdict {
            name,
            pass: false,
            provisional: false,
            detail: format!(
                "{leg_name}: no viewer_connections / viewer_reconnect_s declared for this \
                 leg, so there is no reconnect count to judge the {observed} connection(s) \
                 against"
            ),
        };
    };
    let window_ok = viewer.connection_s.saturating_add(WHEP_RELAUNCH_GAP_S) == slot;
    let pass =
        observed == expected && sessions == expected && viewer.reconnect_s == slot && window_ok;
    SoakVerdict {
        name,
        pass,
        provisional: false,
        detail: format!(
            "{leg_name}: {observed} browser connection(s) = {} reconnect(s) against {} \
             expected ({expected} declared connections); the server counted {sessions} \
             session(s); the viewer loop ran a {}s slot against the declared {slot}s, each \
             browser given {} s ({} = the slot less the {WHEP_RELAUNCH_GAP_S} s relaunch gap)",
            observed.saturating_sub(1),
            expected.saturating_sub(1),
            viewer.reconnect_s,
            viewer.connection_s,
            if window_ok { "as expected" } else { "NOT" }
        ),
    }
}

/// The served rate per second of `count` over the serve report's
/// window, or `None` when the report names no window.
fn whep_served_rate(serve: &WhepServeReport, count: u64) -> Option<f64> {
    (serve.seconds.is_finite() && serve.seconds > 0.0).then(|| count as f64 / serve.seconds)
}

/// The volume floor for one connection: [`WHEP_VOLUME_FLOOR`] of the
/// served rate over the samples after the first-frame window.
fn whep_volume_floor(rate: f64, samples: usize) -> f64 {
    WHEP_VOLUME_FLOOR * rate * samples.saturating_sub(WHEP_FIRST_FRAME_SAMPLES) as f64
}

/// `whep_frames_continuous`: every connection was ended by the loop's
/// own timeout, reported no error, decoded its first frame within
/// [`WHEP_FIRST_FRAME_SAMPLES`] samples, from then on never went more
/// than [`WHEP_MAX_STALL_SAMPLES`] samples without the decoded frame
/// count rising — no gap over 2 s except at a reconnect — and decoded at
/// least [`WHEP_VOLUME_FLOOR`] of the served frame rate over its
/// remaining samples; and the server dropped no video packet for a
/// full queue and demuxed nothing non-conformant. Returns the verdict
/// and the frames decoded over the run.
fn whep_frames_verdict(
    leg_name: &str,
    viewer: &WhepViewerReport,
    serve: &WhepServeReport,
) -> (SoakVerdict, u64) {
    let mut problems: Vec<String> = Vec::new();
    let mut longest_stall = 0usize;
    let mut frames_decoded = 0u64;
    let mut lowest_margin: Option<(u64, f64, u64)> = None;
    if viewer.connections.is_empty() {
        problems.push("the viewer report lists no connection".to_string());
    }
    let frame_rate = whep_served_rate(serve, serve.mount.frames_pushed);
    if frame_rate.is_none() {
        problems.push(format!(
            "the serve report names no window (seconds {}), so there is no served frame \
             rate to hold the connections to",
            serve.seconds
        ));
    }
    for (counter, value) in [
        (
            "viewer_packets_dropped_queue_full",
            serve.mount.viewer_packets_dropped_queue_full,
        ),
        ("demux_nonconformant", serve.mount.demux_nonconformant),
    ] {
        if value > 0 {
            problems.push(format!(
                "the server's mount counter {counter} is {value}, not 0"
            ));
        }
    }
    for c in &viewer.connections {
        let k = c.index;
        let last = c.frames.last().copied().unwrap_or(0);
        frames_decoded += last;
        if let Some(rate) = frame_rate {
            let floor = whep_volume_floor(rate, c.frames.len());
            if (last as f64) < floor {
                problems.push(format!(
                    "connection {k}: {last} frames decoded over {} sample(s), below the \
                     floor of {floor:.0} ({WHEP_VOLUME_FLOOR} x {rate:.1} frames/s served x \
                     (samples - {WHEP_FIRST_FRAME_SAMPLES}))",
                    c.frames.len()
                ));
            }
            if lowest_margin.is_none_or(|(_, f, l)| (last as f64) - floor < l as f64 - f) {
                lowest_margin = Some((k, floor, last));
            }
        }
        if c.browser_rc != WHEP_BROWSER_TIMED_OUT {
            problems.push(format!(
                "connection {k}: browser exit {}, not ended by the loop's timeout \
                 ({WHEP_BROWSER_TIMED_OUT})",
                c.browser_rc
            ));
        }
        if c.page_errors > 0 || c.console_errors > 0 {
            problems.push(format!(
                "connection {k}: {} page error(s), {} browser console error(s){}",
                c.page_errors,
                c.console_errors,
                c.first_error
                    .as_deref()
                    .map(|e| format!(" — first: {e}"))
                    .unwrap_or_default()
            ));
        }
        if (c.frames.len() as u64) + WHEP_SAMPLE_SHORTFALL < viewer.connection_s {
            problems.push(format!(
                "connection {k}: only {} FRAMES sample(s) over a planned {} s",
                c.frames.len(),
                viewer.connection_s
            ));
        }
        let Some(first) = c
            .frames
            .iter()
            .take(WHEP_FIRST_FRAME_SAMPLES)
            .position(|&f| f > 0)
        else {
            problems.push(format!(
                "connection {k}: no decoded frame within the first {WHEP_FIRST_FRAME_SAMPLES} \
                 sample(s) (values {:?})",
                c.frames
                    .iter()
                    .take(WHEP_FIRST_FRAME_SAMPLES)
                    .collect::<Vec<_>>()
            ));
            continue;
        };
        let mut stall = 0usize;
        for i in first + 1..c.frames.len() {
            if c.frames[i] > c.frames[i - 1] {
                stall = 0;
                continue;
            }
            stall += 1;
            longest_stall = longest_stall.max(stall);
            if stall == WHEP_MAX_STALL_SAMPLES + 1 {
                problems.push(format!(
                    "connection {k}: FRAMES stalled at {} for over {WHEP_MAX_STALL_SAMPLES} s \
                     from sample {}",
                    c.frames[i],
                    i + 2 - stall
                ));
            }
        }
    }
    let verdict = SoakVerdict {
        name: format!("whep_frames_continuous_{leg_name}"),
        pass: problems.is_empty(),
        provisional: false,
        detail: if problems.is_empty() {
            let (k, floor, last) = lowest_margin.unwrap_or((0, 0.0, 0));
            format!(
                "{leg_name}: {} connection(s) of {} s: each decoded its first frame within \
                 {WHEP_FIRST_FRAME_SAMPLES} samples, never stalled over \
                 {WHEP_MAX_STALL_SAMPLES} s (longest stall {longest_stall} s) and decoded at \
                 least {WHEP_VOLUME_FLOOR} of the served {:.1} frames/s (tightest: connection \
                 {k}, {last} frames against a floor of {floor:.0}); {frames_decoded} frames \
                 decoded in all; the server dropped no packet for a full queue and demuxed \
                 nothing non-conformant",
                viewer.connections.len(),
                viewer.connection_s,
                frame_rate.unwrap_or(0.0)
            )
        } else {
            format!(
                "{leg_name}: {} problem(s): {}",
                problems.len(),
                problems.join("; ")
            )
        },
    };
    (verdict, frames_decoded)
}

/// `whep_klv_sei_equals_dc`: in every connection the browser saw the
/// same set of KLV records in the SEI as on the data channel, each
/// carriage at least [`WHEP_VOLUME_FLOOR`] of the served record rate
/// over the samples after the first-frame window, and every record's
/// two copies named the same (PTS, RTP timestamp); and the server
/// dropped no record unbound or oversize. A record on one carriage only
/// is allowed within [`WHEP_KLV_EDGE_HEAD`] of its carriage's head or
/// [`WHEP_KLV_EDGE_TAIL`] of its tail, the two moments a connection's
/// two carriages are not yet, or no longer, both delivering. Returns the
/// verdict and the number of such edge records.
fn whep_klv_verdict(
    leg_name: &str,
    viewer: &WhepViewerReport,
    serve: &WhepServeReport,
) -> (SoakVerdict, u64) {
    let mut problems: Vec<String> = Vec::new();
    let mut at_edges = 0u64;
    let mut sei_total = 0u64;
    let mut dc_total = 0u64;
    let mut lowest_margin: Option<(u64, f64, u64)> = None;
    if viewer.connections.is_empty() {
        problems.push("the viewer report lists no connection".to_string());
    }
    let klv_rate = whep_served_rate(serve, serve.mount.klv_records_pushed);
    if klv_rate.is_none() {
        problems.push(format!(
            "the serve report names no window (seconds {}), so there is no served record \
             rate to hold the connections to",
            serve.seconds
        ));
    }
    for (counter, value) in [
        (
            "klv_records_dropped_unbound",
            serve.mount.klv_records_dropped_unbound,
        ),
        (
            "klv_sei_dropped_oversize",
            serve.mount.klv_sei_dropped_oversize,
        ),
        (
            "klv_dc_dropped_oversize",
            serve.mount.klv_dc_dropped_oversize,
        ),
    ] {
        if value > 0 {
            problems.push(format!(
                "the server's mount counter {counter} is {value}, not 0"
            ));
        }
    }
    for c in &viewer.connections {
        let k = c.index;
        sei_total += c.klv_sei;
        dc_total += c.klv_dc;
        if c.klv_copies_disagreeing > 0 {
            problems.push(format!(
                "connection {k}: {} record(s) whose SEI and data-channel copies name \
                 different (PTS, RTP timestamp) pairs — the two copies come from one \
                 server-side binding and never legitimately disagree",
                c.klv_copies_disagreeing
            ));
        }
        if c.klv_sei == 0 || c.klv_dc == 0 {
            problems.push(format!(
                "connection {k}: {} record(s) via SEI, {} via the data channel — a carriage \
                 delivered nothing",
                c.klv_sei, c.klv_dc
            ));
            continue;
        }
        if let Some(rate) = klv_rate {
            let floor = whep_volume_floor(rate, c.frames.len());
            let fewest = c.klv_sei.min(c.klv_dc);
            if (fewest as f64) < floor {
                problems.push(format!(
                    "connection {k}: {} record(s) via SEI, {} via the data channel over {} \
                     sample(s), below the floor of {floor:.0} ({WHEP_VOLUME_FLOOR} x \
                     {rate:.1} records/s served x (samples - {WHEP_FIRST_FRAME_SAMPLES}))",
                    c.klv_sei,
                    c.klv_dc,
                    c.frames.len()
                ));
            }
            if lowest_margin.is_none_or(|(_, f, l)| (fewest as f64) - floor < l as f64 - f) {
                lowest_margin = Some((k, floor, fewest));
            }
        }
        if c.klv_sei_set_sha256 == c.klv_dc_set_sha256 {
            continue;
        }
        if c.klv_unpaired.is_empty() {
            problems.push(format!(
                "connection {k}: the SEI set ({} record(s)) and the data-channel set ({}) \
                 differ, and the report lists no unpaired record to locate the difference",
                c.klv_sei, c.klv_dc
            ));
            continue;
        }
        for u in &c.klv_unpaired {
            if u.from_head < WHEP_KLV_EDGE_HEAD || u.from_tail < WHEP_KLV_EDGE_TAIL {
                at_edges += 1;
            } else {
                problems.push(format!(
                    "connection {k}: a record via {} ({} record(s) before it, {} after) has \
                     no copy on the other carriage",
                    u.via, u.from_head, u.from_tail
                ));
            }
        }
    }
    let verdict = SoakVerdict {
        name: format!("whep_klv_sei_equals_dc_{leg_name}"),
        pass: problems.is_empty(),
        provisional: false,
        detail: if problems.is_empty() {
            let (k, floor, fewest) = lowest_margin.unwrap_or((0, 0.0, 0));
            format!(
                "{leg_name}: {} connection(s): the SEI record set equals the data-channel set \
                 in each ({sei_total} record(s) via SEI, {dc_total} via the data channel; \
                 {at_edges} unpaired only at a connection's edges — the first \
                 {WHEP_KLV_EDGE_HEAD} records before the data channel opened or the last \
                 {WHEP_KLV_EDGE_TAIL} as the browser was ended), each carriage at least \
                 {WHEP_VOLUME_FLOOR} of the served {:.1} records/s (tightest: connection {k}, \
                 {fewest} records against a floor of {floor:.0}), and no copy pair disagreed \
                 on (PTS, RTP timestamp); the server dropped no record unbound or oversize",
                viewer.connections.len(),
                klv_rate.unwrap_or(0.0)
            )
        } else {
            format!(
                "{leg_name}: {} problem(s): {}",
                problems.len(),
                problems.join("; ")
            )
        },
    };
    (verdict, at_edges)
}

fn read_to_string(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn read_leg_artifacts(
    proxy_stats_path: &Path,
    recv_report_path: &Path,
    send_report_path: &Path,
    outage_period_s: Option<u64>,
) -> Result<LegArtifacts, String> {
    let proxy_stats: ProxyStats = serde_json::from_str(&read_to_string(proxy_stats_path)?)
        .map_err(|e| format!("parse {}: {e}", proxy_stats_path.display()))?;
    let recv_report: VerifyReport = serde_json::from_str(&read_to_string(recv_report_path)?)
        .map_err(|e| format!("parse {}: {e}", recv_report_path.display()))?;
    let send_metrics: CellMetrics = serde_json::from_str(&read_to_string(send_report_path)?)
        .map_err(|e| format!("parse {}: {e}", send_report_path.display()))?;
    Ok(LegArtifacts {
        proxy_stats,
        recv_report,
        send_metrics,
        outage_period_s,
    })
}

/// `report soak`'s file-driven entry point: read every artifact
/// path, build [`SoakInputs`], and write [`build_soak_results`]'s
/// output to `out_path`. The `rist` leg is entirely optional — pass
/// `None` to omit it (the `srt` leg alone is enough for a local
/// smoke run; the full 72h run supplies both). So are the publish
/// legs: `whep` is the whep leg's `(viewer report, serve counters)`
/// pair when the run had that leg; `publish_legs` maps each one run (a [`PUBLISH_LEGS`] name,
/// at most once) to its recv report, the only artifact it has.
///
/// # Errors
///
/// Errors on an unreadable or unparsable artifact, and on a publish
/// leg name outside [`PUBLISH_LEGS`] or given twice.
#[allow(clippy::too_many_arguments)]
pub fn run(
    rss_path: &Path,
    config_path: &Path,
    exits_path: &Path,
    srt_proxy_stats_path: &Path,
    srt_recv_report_path: &Path,
    srt_send_report_path: &Path,
    srt_outage_period_s: u64,
    rist: Option<(&Path, &Path, &Path)>,
    publish_legs: &[(String, PathBuf)],
    whep: Option<(&Path, &Path)>,
    rss_slope_threshold_kb_per_hour: Option<f64>,
    out_path: &Path,
) -> Result<SoakResults, String> {
    let rss_samples = parse_rss_csv(&read_to_string(rss_path)?)?;
    let config = parse_soak_config(&read_to_string(config_path)?)?;
    let worker_exits = parse_worker_exits(&read_to_string(exits_path)?)?;

    let mut legs = vec![(
        "srt".to_string(),
        read_leg_artifacts(
            srt_proxy_stats_path,
            srt_recv_report_path,
            srt_send_report_path,
            Some(srt_outage_period_s),
        )?,
    )];

    if let Some((proxy_stats_path, recv_report_path, send_report_path)) = rist {
        legs.push((
            "rist".to_string(),
            read_leg_artifacts(proxy_stats_path, recv_report_path, send_report_path, None)?,
        ));
    }

    let publish_leg_reports = publish_legs;
    let mut publish_legs: Vec<(String, PublishLegArtifacts)> = Vec::new();
    for (leg, path) in publish_leg_reports {
        if !PUBLISH_LEGS.contains(&leg.as_str()) {
            return Err(format!(
                "publish leg {leg:?} is not a publish leg (want one of {PUBLISH_LEGS:?})"
            ));
        }
        if publish_legs.iter().any(|(seen, _)| seen == leg) {
            return Err(format!("publish leg {leg:?} given more than once"));
        }
        let recv_report: VerifyReport = serde_json::from_str(&read_to_string(path)?)
            .map_err(|e| format!("parse {}: {e}", path.display()))?;
        // `soak.sh` writes the source digest beside the recv report.
        let source_sha256_file = path
            .parent()
            .map(|dir| dir.join("source.sha256"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        publish_legs.push((
            leg.clone(),
            PublishLegArtifacts {
                recv_report,
                source_sha256_file,
            },
        ));
    }

    let mut whep_legs: Vec<(String, WhepLegArtifacts)> = Vec::new();
    if let Some((viewer_path, serve_path)) = whep {
        let viewer: WhepViewerReport = serde_json::from_str(&read_to_string(viewer_path)?)
            .map_err(|e| format!("parse {}: {e}", viewer_path.display()))?;
        let serve: WhepServeReport = serde_json::from_str(&read_to_string(serve_path)?)
            .map_err(|e| format!("parse {}: {e}", serve_path.display()))?;
        whep_legs.push((WHEP_LEGS[0].to_string(), WhepLegArtifacts { viewer, serve }));
    }

    let results = build_soak_results(SoakInputs {
        rss_samples,
        legs,
        publish_legs,
        whep_legs,
        rss_slope_threshold_kb_per_hour,
        config,
        worker_exits,
    })?;

    let json = serde_json::to_string_pretty(&results).expect("SoakResults always serializes");
    std::fs::write(out_path, json).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    Ok(results)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::report_types::KlvPlacementMetrics;

    pub(crate) fn proxy_stats(
        forwarded: u64,
        dropped: u64,
        loss_pct: f64,
        outage_period_s: Option<u64>,
        outage_dur_s: u64,
    ) -> ProxyStats {
        ProxyStats {
            forwarded,
            dropped,
            duped: 0,
            reordered: 0,
            seed: 1,
            config: crate::proxy::ConfigEcho {
                loss_pct,
                dup_pct: 0.0,
                reorder_pct: 0.0,
                reorder_hold: 0,
                jitter_ms_max: 0,
                base_delay_ms: 0,
                outage_period_s,
                outage_dur_s,
                schedule: None,
            },
            // Fixed mode: one phase carrying the whole run, the shape
            // `proxy::run` writes for a non-scheduled relay.
            phases: vec![crate::proxy::PhaseCounters {
                index: 0,
                forwarded,
                dropped,
                duped: 0,
                outage_dropped: 0,
            }],
        }
    }

    /// One phase of a synthetic schedule for [`scheduled_stats`] — only
    /// `index` and `loss_pct` vary across the tests below; the rest
    /// (burst shape, jitter/reorder/delay overrides) are irrelevant to
    /// the drop-rate verdict, which reads only `loss_pct`.
    fn phase(index: u32, loss: f64) -> crate::impair::Phase {
        crate::impair::Phase {
            index,
            loss_pct: loss,
            burst: false,
            burst_run: (1, 1),
            draw_pct: loss,
            jitter_ms_max: 0,
            reorder_pct: 0.0,
            reorder_hold: 0,
            base_delay_ms: 0,
        }
    }

    /// A scheduled-mode `ProxyStats`: `counts[i] = (forwarded,
    /// dropped)` for phase `i`, `losses[i]` is that phase's
    /// `loss_pct` in the echoed schedule table. Built on top of
    /// [`proxy_stats`] (its own `loss_pct`/`schedule`/`phases` are
    /// discarded and replaced) so the two fixtures can't drift apart
    /// on the fields they share.
    fn scheduled_stats(counts: &[(u64, u64)], losses: &[f64]) -> ProxyStats {
        let total_forwarded = counts.iter().map(|c| c.0).sum();
        let total_dropped = counts.iter().map(|c| c.1).sum();
        let mut s = proxy_stats(total_forwarded, total_dropped, 0.0, None, 0);
        s.config.schedule = Some(crate::proxy::ScheduleEcho {
            seed: 1,
            phases: losses.len() as u32,
            phase_s: 60,
            table: losses
                .iter()
                .enumerate()
                .map(|(i, &l)| phase(i as u32, l))
                .collect(),
        });
        s.phases = counts
            .iter()
            .enumerate()
            .map(|(i, &(f, d))| crate::proxy::PhaseCounters {
                index: i as u32,
                forwarded: f,
                dropped: d,
                duped: 0,
                outage_dropped: 0,
            })
            .collect();
        s
    }

    pub(crate) fn cell_metrics(video_aus: u64) -> CellMetrics {
        CellMetrics {
            video_aus,
            keyframes: video_aus / 30,
            klv_records: 0,
            klv_set_sha256: Some(String::new()),
            audio_frames: 0,
            programs_seen: 1,
            pts_monotonic: true,
            misp_sei_seen: false,
            bytes: 0,
            stream_sha256: String::new(),
            discontinuities: 0,
            nonconformant: 0,
            corruption: None,
            corruption_attribution: None,
            klv_rich: None,
            klv_placement: None,
            since_reconnect: None,
            managed_send: None,
            judged_profile: None,
            skipped_oracles: None,
        }
    }

    pub(crate) fn passing_recv_report(video_aus: u64) -> VerifyReport {
        VerifyReport {
            pass: true,
            failures: Vec::new(),
            metrics: cell_metrics(video_aus),
            reconnects: None,
            profile: None,
            skipped_oracles: Vec::new(),
            publish_mount: None,
        }
    }

    fn rss_row(leg: &str, process: &str, elapsed_s: f64, rss_kb: Option<u64>) -> RssSample {
        RssSample {
            elapsed_s,
            leg: leg.to_string(),
            process: process.to_string(),
            pid: 1000,
            rss_kb,
        }
    }

    fn one_leg(artifacts: LegArtifacts) -> Vec<(String, LegArtifacts)> {
        vec![("srt".to_string(), artifacts)]
    }

    /// A config whose expected duration matches a synthetic series that
    /// spans `span_s` at `cadence_s` — the sampler-slack term is 0 so
    /// `span == expected` is the fully-covered case.
    fn cfg(span_s: f64, cadence_s: f64) -> SoakConfig {
        SoakConfig {
            expected_duration_s: span_s,
            rss_cadence_s: cadence_s,
            warmup_fraction: 1.0 / 6.0,
            sampler_end_slack_s: 0.0,
            expected_worker_exits: BTreeMap::new(),
            corruption: false,
            corruption_spec: None,
            legs: BTreeMap::new(),
            memory_gate: None,
        }
    }

    fn six_clean_exits() -> BTreeMap<String, i32> {
        [
            "srt-send",
            "srt-proxy",
            "srt-recv",
            "rist-send",
            "rist-proxy",
            "rist-recv",
        ]
        .into_iter()
        .map(|r| (r.to_string(), 0))
        .collect()
    }

    /// 3 processes x `n` samples every `cadence_s`, all flat.
    fn flat_series(n: usize, cadence_s: f64) -> Vec<RssSample> {
        let mut s = Vec::new();
        for i in 0..n {
            let t = i as f64 * cadence_s;
            s.push(rss_row("srt", "send", t, Some(50_000)));
            s.push(rss_row("srt", "proxy", t, Some(20_000)));
            s.push(rss_row("srt", "recv", t, Some(50_000)));
        }
        s
    }

    fn verdict<'a>(r: &'a SoakResults, name: &str) -> &'a SoakVerdict {
        r.verdicts
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("verdict {name} must be present: {:?}", r.verdicts))
    }

    fn inputs(
        samples: Vec<RssSample>,
        config: SoakConfig,
        exits: BTreeMap<String, i32>,
    ) -> SoakInputs {
        SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config,
            worker_exits: exits,
        }
    }

    /// A fully-covered, fully-clean single-leg (`"srt"`) run — every
    /// verdict `inputs()` can produce on its own already passes. Tests
    /// that only care about one additional verdict (e.g. the
    /// corruption ones below) start from this and mutate just the
    /// field(s) they're probing, rather than re-deriving a passing
    /// baseline by hand.
    fn healthy_inputs() -> SoakInputs {
        inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), six_clean_exits())
    }

    #[test]
    fn fully_covered_run_passes_every_completeness_verdict() {
        // 121 samples at 30s = 3600s span, expected 3600.
        let r = build_soak_results(inputs(
            flat_series(121, 30.0),
            cfg(3600.0, 30.0),
            six_clean_exits(),
        ))
        .unwrap();
        assert!(verdict(&r, "duration_coverage").pass);
        assert!(verdict(&r, "rss_sample_coverage_srt_send").pass);
        assert!(verdict(&r, "worker_exits").pass);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    #[test]
    fn run_truncated_at_forty_percent_fails_duration_coverage() {
        // Series spans 1440s of an expected 3600s.
        let r = build_soak_results(inputs(
            flat_series(49, 30.0),
            cfg(3600.0, 30.0),
            six_clean_exits(),
        ))
        .unwrap();
        let v = verdict(&r, "duration_coverage");
        assert!(!v.pass, "{}", v.detail);
        assert!(!v.provisional);
        assert!(!r.overall_pass);
        assert_eq!(r.expected_duration_s, 3600.0);
    }

    #[test]
    fn one_sample_per_process_fails_sample_coverage_and_skips_slope() {
        let mut samples = flat_series(1, 30.0);
        // Put the one sample past warmup so it is the only post-warmup point.
        for s in &mut samples {
            s.elapsed_s = 3000.0;
        }
        let r = build_soak_results(inputs(samples, cfg(3600.0, 30.0), six_clean_exits())).unwrap();
        let v = verdict(&r, "rss_sample_coverage_srt_send");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            r.rss_slopes.is_empty(),
            "slope must not be computed from a single sample: {:?}",
            r.rss_slopes
        );
        assert!(r.verdicts.iter().all(|v| !v.name.starts_with("rss_slope_")));
        assert!(!r.overall_pass);
    }

    #[test]
    fn ten_minute_gap_in_an_otherwise_complete_series_fails_sample_coverage() {
        let mut samples = flat_series(121, 30.0);
        // Drop every sample in (1800, 2400) — a 600s hole after warmup.
        samples.retain(|s| !(s.elapsed_s > 1800.0 && s.elapsed_s < 2400.0));
        let r = build_soak_results(inputs(samples, cfg(3600.0, 30.0), six_clean_exits())).unwrap();
        let v = verdict(&r, "rss_sample_coverage_srt_send");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("largest gap"), "{}", v.detail);
        let c = r.coverage.iter().find(|c| c.process == "send").unwrap();
        assert!(c.largest_gap_s >= 600.0);
    }

    #[test]
    fn nonzero_worker_exit_fails_worker_exits_verdict() {
        let mut exits = six_clean_exits();
        exits.insert("srt-send".to_string(), 137);
        let r =
            build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits)).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(!v.pass);
        assert!(
            v.detail.contains("srt-send") && v.detail.contains("137"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    #[test]
    fn expected_nonzero_exit_listed_in_config_passes() {
        let mut exits = six_clean_exits();
        exits.insert("srt-send".to_string(), 137);
        let mut config = cfg(3600.0, 30.0);
        config
            .expected_worker_exits
            .insert("srt-send".to_string(), 137);
        let r = build_soak_results(inputs(flat_series(121, 30.0), config, exits)).unwrap();
        assert!(verdict(&r, "worker_exits").pass);
    }

    #[test]
    fn missing_worker_role_in_exits_fails() {
        // `inputs()` builds a single-leg (srt-only) run via `one_leg`,
        // so the removed role must be one of the srt-* three — a
        // missing rist-* role is legitimately not required here (see
        // `single_leg_run_requires_only_that_legs_roles` below).
        let mut exits = six_clean_exits();
        exits.remove("srt-recv");
        let r =
            build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits)).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(!v.pass && v.detail.contains("srt-recv"), "{}", v.detail);
    }

    /// A single-leg (srt-only) run — e.g. a local smoke test — never
    /// launches the `rist-*` trio at all, so `worker_exits` must not
    /// require their presence in `exits.json`; only the srt-* three
    /// (the leg actually present in `legs`) are required.
    #[test]
    fn single_leg_run_requires_only_that_legs_roles() {
        let exits: BTreeMap<String, i32> = [("srt-send", 0), ("srt-proxy", 0), ("srt-recv", 0)]
            .into_iter()
            .map(|(r, s)| (r.to_string(), s))
            .collect();
        let r =
            build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits)).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(v.pass, "{}", v.detail);
    }

    /// A key in `exits.json` that isn't one of the six `WORKER_ROLES`
    /// must fail `worker_exits` rather than being silently ignored —
    /// otherwise an unknown role's nonzero status (a renamed worker, a
    /// typo, a future seventh role) never surfaces anywhere.
    #[test]
    fn unknown_role_in_exits_json_fails_worker_exits() {
        let mut exits = six_clean_exits();
        exits.insert("bogus-role".to_string(), 137);
        let r =
            build_soak_results(inputs(flat_series(121, 30.0), cfg(3600.0, 30.0), exits)).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(
            !v.pass && v.detail.contains("bogus-role") && v.detail.contains("137"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    // (a) flat RSS -> pass (and, with no threshold set, provisional).
    // Samples cover all 3 processes (not just "send") so the
    // per-combination data-presence check (see `missing_data`) has
    // nothing to flag here — this test is about the regression
    // math, not that check.
    #[test]
    fn flat_rss_slope_passes() {
        let mut samples: Vec<RssSample> = Vec::new();
        for i in 0..20 {
            let t = i as f64 * 60.0;
            samples.push(rss_row("srt", "send", t, Some(50_000)));
            samples.push(rss_row("srt", "proxy", t, Some(20_000)));
            samples.push(rss_row("srt", "recv", t, Some(50_000)));
        }
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(1140.0, 60.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let slope = results
            .rss_slopes
            .iter()
            .find(|s| s.leg == "srt" && s.process == "send")
            .expect("send slope must be present");
        assert!(
            slope.slope_kb_per_hour.abs() < 1.0,
            "flat RSS must regress to ~0 slope, got {}",
            slope.slope_kb_per_hour
        );
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "rss_slope_srt_send")
            .expect("verdict must be present");
        assert!(v.pass);
        assert!(v.provisional, "no threshold given -> provisional");
        assert!(results.overall_pass);
    }

    // (b) 1 MiB/h ramp -> slope recorded; passes with no threshold,
    // fails once a threshold below it is set.
    #[test]
    fn ramping_rss_slope_recorded_and_gated_by_threshold() {
        let mut samples: Vec<RssSample> = Vec::new();
        for i in 0..600 {
            let elapsed_s = i as f64 * 60.0;
            let recv_kb = 50_000 + (elapsed_s / 3600.0 * 1024.0) as u64;
            // send/proxy stay flat — this test is about recv's slope
            // and threshold-gating, but every process still needs
            // >=1 post-warmup sample or the new data-presence check
            // (see `missing_data`) would flag them as missing.
            samples.push(rss_row("srt", "recv", elapsed_s, Some(recv_kb)));
            samples.push(rss_row("srt", "send", elapsed_s, Some(50_000)));
            samples.push(rss_row("srt", "proxy", elapsed_s, Some(20_000)));
        }
        let base_inputs = || SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples.clone(),
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(35940.0, 60.0),
            worker_exits: six_clean_exits(),
        };

        let no_threshold =
            build_soak_results(base_inputs()).expect("non-empty rss_samples must not error");
        let slope = no_threshold
            .rss_slopes
            .iter()
            .find(|s| s.process == "recv")
            .expect("recv slope must be present");
        assert!(
            (slope.slope_kb_per_hour - 1024.0).abs() < 20.0,
            "expected ~1024 KiB/h, got {}",
            slope.slope_kb_per_hour
        );
        let v = no_threshold
            .verdicts
            .iter()
            .find(|v| v.name == "rss_slope_srt_recv")
            .unwrap();
        assert!(v.pass, "no threshold set -> never fails");
        assert!(v.provisional);

        let mut with_threshold = base_inputs();
        with_threshold.rss_slope_threshold_kb_per_hour = Some(512.0);
        let results =
            build_soak_results(with_threshold).expect("non-empty rss_samples must not error");
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "rss_slope_srt_recv")
            .unwrap();
        assert!(
            !v.pass,
            "a ~1024 KiB/h slope must fail a 512 KiB/h threshold"
        );
        assert!(!v.provisional);
        assert!(!results.overall_pass);
    }

    // (c) an unexplained gap (drop rate the configured schedule
    // can't account for) fails the drop-rate check — see the module
    // doc's "Known telemetry limitations" section for why this
    // stands in for a per-event gap-timing check.
    #[test]
    fn unexplained_excess_drop_fails_the_drop_rate_check() {
        let samples = vec![rss_row("srt", "send", 0.0, Some(1000))];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                // No outage configured, loss_pct=0 -> expected drop
                // fraction ~0, but the proxy actually dropped half of
                // everything.
                proxy_stats: proxy_stats(500, 500, 0.0, None, 0),
                recv_report: passing_recv_report(500),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(0.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(
            !v.pass,
            "an unexplained 50% drop rate against ~0% expected must fail"
        );
        assert!(!v.provisional);
        assert!(!results.overall_pass);
    }

    // Positive control for (c): a run whose observed drop rate
    // matches what the proxy's CONTINUOUS configured loss_pct
    // predicts must pass — deliberately independent of the outage
    // schedule (an outage window's true impact on a managed sender
    // is reduced throughput, not proxy-visible drops; see the
    // module doc's "Known telemetry limitations" section for the
    // empirical finding behind that design choice).
    #[test]
    fn drop_rate_matching_configured_outage_passes() {
        let total = 100_000u64;
        let dropped = (total as f64 * 0.02).round() as u64;
        // All 3 processes present at a real 300s cadence (not just
        // "send", and not just the two endpoints) — this test
        // asserts `overall_pass`, so it must clear both the
        // per-combination data-presence check AND the new
        // duration/sample-coverage checks too.
        let mut samples = Vec::new();
        for i in 0..=12 {
            let t = i as f64 * 300.0;
            samples.push(rss_row("srt", "send", t, Some(1000)));
            samples.push(rss_row("srt", "proxy", t, Some(1000)));
            samples.push(rss_row("srt", "recv", t, Some(1000)));
        }
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(total - dropped, dropped, 2.0, Some(3600), 360),
                recv_report: passing_recv_report(total),
                send_metrics: cell_metrics(total),
                outage_period_s: Some(3600),
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(3600.0, 300.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(
            v.pass,
            "drop rate matching the configured outage+loss schedule must pass: {}",
            v.detail
        );
        assert!(results.overall_pass);
    }

    /// `expected_outage_windows` must not count window 0: `soak.sh`
    /// deliberately launches the proxy `outage_dur_s + 30` seconds
    /// ahead of send/recv
    /// specifically so window 0 (which always starts at the proxy's
    /// own `t=0`) never overlaps traffic that hasn't started yet.
    /// The 1h smoke's own real shape (outage_period_s=21600,
    /// outage_dur_s=90, ~3542s of active traffic) never reaches
    /// window 1 either (that starts at t=21600 in the proxy's frame,
    /// long after the ~3572s active period this shape covers) — so
    /// the correct expectation is 0, not 1 (the pre-fix answer,
    /// which always counted window 0 for any nonzero duration even
    /// though it's structurally unreachable).
    #[test]
    fn expected_outage_windows_excludes_the_unreachable_warmup_window() {
        let samples = vec![
            rss_row("srt", "send", 0.0, Some(1000)),
            rss_row("srt", "send", 3542.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(98_000, 2_000, 2.0, Some(21600), 90),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: Some(21600),
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(3542.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        assert_eq!(
            results.legs[0].expected_outage_windows, 0,
            "a 1h-smoke-shaped run must never reach window 0 (proxy-only, before \
             send/recv start) or window 1 (t=21600, long after this run ends)"
        );
    }

    /// The opposite direction, so the fix isn't just trivially always
    /// zero: a run long enough to actually reach several scheduled
    /// windows must count them correctly, still excluding window 0.
    /// 72h at a 6h period reaches windows 1..=12 (window 12 starts at
    /// t=259200, comfortably inside the ~259320s active period the
    /// 120s head-start leaves room for; window 13 at t=280800 does
    /// not).
    #[test]
    fn expected_outage_windows_counts_real_72h_schedule_correctly() {
        let samples = vec![
            rss_row("srt", "send", 0.0, Some(1000)),
            rss_row("srt", "send", 259200.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(98_000, 2_000, 2.0, Some(21600), 90),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: Some(21600),
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(259200.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        assert_eq!(
            results.legs[0].expected_outage_windows, 12,
            "72h at a 6h period must count windows 1..=12 (never window 0, the \
             proxy-only warmup window)"
        );
    }

    /// (Important fix regression) The tolerance must SCALE DOWN with
    /// packet volume: a genuine ~2x drop-rate regression (4%
    /// observed against a 2% configured `loss_pct`) must FAIL at a
    /// large `n` (the scale a 72h run actually reaches — hundreds
    /// of thousands to tens of millions of packets), where 2
    /// percentage points of absolute deviation is many standard
    /// errors of noise, not something a flat volume-blind
    /// percentage band could ever catch.
    #[test]
    fn large_n_two_x_drop_rate_regression_fails() {
        let total = 10_000_000u64;
        let dropped = (total as f64 * 0.04).round() as u64; // observed 4%, configured 2%
        let samples = vec![
            rss_row("srt", "send", 0.0, Some(1000)),
            rss_row("srt", "proxy", 0.0, Some(1000)),
            rss_row("srt", "recv", 0.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(total - dropped, dropped, 2.0, None, 0),
                recv_report: passing_recv_report(total),
                send_metrics: cell_metrics(total),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(0.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(
            !v.pass,
            "a 4%-observed/2%-configured drop rate at n=10,000,000 must fail: {}",
            v.detail
        );
        assert!(!v.provisional);
        assert!(!results.overall_pass);
    }

    // Phase-integrated drop-rate verdict (spec §6.2): a scheduled
    // run's expected drop rate is the traffic-weighted average of
    // each phase's own `loss_pct` over that phase's own
    // (forwarded+dropped) share, not one constant rate applied to
    // the whole run's total.

    #[test]
    fn phase_integrated_drop_verdict_passes_a_consistent_four_phase_run() {
        // losses 1/4/2/0.5 % over 100k packets each, observed exactly
        // on-rate.
        let st = scheduled_stats(
            &[
                (99_000, 1_000),
                (96_000, 4_000),
                (98_000, 2_000),
                (99_500, 500),
            ],
            &[1.0, 4.0, 2.0, 0.5],
        );
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = st;
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("4 phase"), "{}", v.detail);
    }

    #[test]
    fn phase_integrated_drop_verdict_fails_when_one_phase_doubles_its_drops() {
        let st = scheduled_stats(
            &[
                (99_000, 1_000),
                (92_000, 8_000), // phase 1's drops doubled: 4% -> 8%
                (98_000, 2_000),
                (99_500, 500),
            ],
            &[1.0, 4.0, 2.0, 0.5],
        );
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = st;
        let r = build_soak_results(inputs).unwrap();
        assert!(
            !r.verdicts
                .iter()
                .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
                .unwrap()
                .pass
        );
    }

    #[test]
    fn one_phase_schedule_is_judged_like_fixed_mode() {
        // loss_pct 2.0 in the echo, matching the one-phase schedule's
        // own loss_pct below, so the two fixtures describe the same
        // impairment via the two different mechanisms.
        let fixed = proxy_stats(98_000, 2_000, 2.0, None, 0);
        let sched = scheduled_stats(&[(98_000, 2_000)], &[2.0]);
        let mut a = healthy_inputs();
        a.legs[0].1.proxy_stats = fixed;
        let mut b = healthy_inputs();
        b.legs[0].1.proxy_stats = sched;
        let ra = build_soak_results(a).unwrap();
        let rb = build_soak_results(b).unwrap();
        let la = &ra.legs[0];
        let lb = &rb.legs[0];
        assert_eq!(
            (la.expected_drop_fraction, la.drop_fraction_tolerance),
            (lb.expected_drop_fraction, lb.drop_fraction_tolerance)
        );
        assert_eq!((la.phases, lb.phases), (1, 1));
    }

    /// A stats file written before per-phase counting existed
    /// (`phases` empty, `config.schedule` absent — exactly what
    /// `#[serde(default)]` produces for that vintage of JSON) must be
    /// judged exactly as before: totals against the one configured
    /// `loss_pct`, one phase.
    #[test]
    fn pre_feature_stats_file_falls_back_to_totals_against_configured_loss_pct() {
        let mut st = proxy_stats(98_000, 2_000, 2.0, None, 0);
        assert!(st.config.schedule.is_none());
        // Simulate the actual pre-feature shape: `#[serde(default)]`
        // leaves `phases` empty on a stats file written before
        // per-phase counting existed (the `proxy_stats` fixture above
        // always populates one entry, the shape `proxy::run` writes
        // today).
        st.phases.clear();
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = st;
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("1 phase"), "{}", v.detail);
        assert_eq!(r.legs[0].phases, 1);
        assert!((r.legs[0].expected_drop_fraction - 0.02).abs() < 1e-9);
    }

    /// A malformed artifact — a schedule echo whose phase-rate count
    /// doesn't match its own phase-counter count (e.g. hand-edited or
    /// truncated stats JSON) — must fail loud with a message naming
    /// the mismatch, never panic and never silently fall back to a
    /// partial computation.
    #[test]
    fn mismatched_phase_rate_and_counter_lengths_fails_loud_not_panics() {
        let mut st = scheduled_stats(&[(99_000, 1_000), (96_000, 4_000)], &[1.0, 4.0]);
        st.phases.pop(); // 2 rates in the schedule echo, 1 counter left
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = st;
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("malformed"), "{}", v.detail);
    }

    /// The same fail-loud stance for the OTHER way a stats artifact
    /// can contradict itself: per-phase counters that do not sum to
    /// the top-level totals. `proxy::run` increments both together at
    /// every site, so they always reconcile on a real run — but they
    /// are independent JSON fields, and the rate above is computed
    /// from the per-phase sums ALONE, so without this check a
    /// hand-edited or partially-written file would yield a confident
    /// rate the file itself contradicts.
    #[test]
    fn phase_counters_that_dont_sum_to_the_totals_fail_loud() {
        let mut st = scheduled_stats(&[(99_000, 1_000), (96_000, 4_000)], &[1.0, 4.0]);
        // Phases still sum to 195_000/5_000; claim something else.
        st.forwarded = 1;
        st.dropped = 2;
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = st;
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("malformed"), "{}", v.detail);
        assert!(
            v.detail.contains("195000") && v.detail.contains("5000"),
            "the detail must name the per-phase sums: {}",
            v.detail
        );
    }

    /// Anti-vacuity for the check above: a PRE-FEATURE stats file
    /// (empty `phases`, top-level totals only) must still be judged
    /// normally, not swept up as malformed. `#[serde(default)]` makes
    /// that shape deserializable on purpose.
    #[test]
    fn a_stats_file_with_no_phase_split_is_not_malformed() {
        let mut inputs = healthy_inputs();
        // The pre-feature shape: totals only, no per-phase split.
        // `#[serde(default)]` is what makes such a file deserialize,
        // and the reconciliation check must not fire on it.
        inputs.legs[0].1.proxy_stats.phases.clear();
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(!v.detail.contains("malformed"), "{}", v.detail);
    }

    /// (Important fix regression) The SAME 2x relative deviation
    /// (4% observed against 2% configured) at a SMALL `n` must
    /// still pass — at low packet counts that deviation is
    /// unremarkable sampling noise, not evidence of a real
    /// regression, and the tolerance (whether via the binomial term
    /// or the small absolute floor) must reflect that rather than
    /// flagging every short/thin cell as broken.
    #[test]
    fn small_n_two_x_drop_rate_deviation_still_passes() {
        let total = 50u64;
        let dropped = 2u64; // 4% observed vs 2% configured, same ratio as the large-n test above
        // A short but dense RSS series (this test asserts
        // `overall_pass`, so it must also clear duration/sample
        // coverage — not just the drop-rate check it's actually
        // about).
        let mut samples = Vec::new();
        for i in 0..=3 {
            let t = i as f64 * 60.0;
            samples.push(rss_row("srt", "send", t, Some(1000)));
            samples.push(rss_row("srt", "proxy", t, Some(1000)));
            samples.push(rss_row("srt", "recv", t, Some(1000)));
        }
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(total - dropped, dropped, 2.0, None, 0),
                recv_report: passing_recv_report(total),
                send_metrics: cell_metrics(total),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(180.0, 60.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(
            v.pass,
            "the same 4%-vs-2% deviation at n=50 must pass (sampling noise, not a \
             regression): {}",
            v.detail
        );
        assert!(results.overall_pass);
    }

    fn burst_phase(index: u32, loss: f64) -> crate::impair::Phase {
        crate::impair::Phase {
            burst: true,
            burst_run: (3, 8),
            draw_pct: loss / 5.5,
            ..phase(index, loss)
        }
    }

    /// One long 4 % burst phase at 10 M packets: the engine's realised
    /// rate is 5.5q/(1+4.5q) = 3.8732 % (impair.rs), 0.127 pp under
    /// the configured 4 % — past the 0.1 pp floor once n makes the
    /// statistical term small. Healthy engine, false FAIL today.
    #[test]
    fn drop_rate_one_phase_burst_run_matches_the_renewal_expectation() {
        let n = 10_000_000u64;
        let dropped = 387_324u64; // round(n x 0.04 / 1.0327273)
        let mut stats = scheduled_stats(&[(n - dropped, dropped)], &[4.0]);
        stats.config.schedule.as_mut().unwrap().table[0] = burst_phase(0, 4.0);
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = stats;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("expected 3.87%"), "{}", v.detail);
        let leg = &r.legs[0];
        assert!(
            (leg.expected_drop_fraction - 0.038732).abs() < 1e-5,
            "{leg:?}"
        );
        // Burst-aware tolerance: never below the binomial one.
        let binomial = 6.0 * (0.038732 * (1.0 - 0.038732) / n as f64).sqrt();
        assert!(
            leg.drop_fraction_tolerance >= binomial.max(0.001),
            "{leg:?}"
        );
    }

    /// A short clean phase followed by a burst phase the run sat in
    /// for ten times longer (the schedule clamps to its last phase):
    /// the mixture is packet-weighted, and the burst share is what
    /// the engine actually realises.
    #[test]
    fn drop_rate_last_phase_dominated_burst_run_matches_the_renewal_expectation() {
        let mut stats = scheduled_stats(&[(995_000, 5_000), (9_612_676, 387_324)], &[0.5, 4.0]);
        stats.config.schedule.as_mut().unwrap().table[1] = burst_phase(1, 4.0);
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = stats;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(
            (r.legs[0].expected_drop_fraction - 392_324.0 / 11_000_000.0).abs() < 1e-6,
            "{:?}",
            r.legs[0]
        );
    }

    /// `Engine::decide` consults `burst_run` only under `if p.burst`,
    /// so a phase carrying a burst run WITHOUT the flag drops exactly
    /// one packet per fire. Keying the model on `burst_run` alone
    /// would have expected 10.1 % where the engine realises 2 %.
    #[test]
    fn drop_rate_ignores_a_burst_run_on_a_phase_that_is_not_bursty() {
        let n = 10_000_000u64;
        let dropped = 200_000u64; // 2.0%: one drop per fire
        let mut stats = scheduled_stats(&[(n - dropped, dropped)], &[2.0]);
        {
            let ph = &mut stats.config.schedule.as_mut().unwrap().table[0];
            ph.burst = false;
            ph.burst_run = (3, 8); // stale/hand-edited: the engine ignores it
            ph.draw_pct = 2.0;
        }
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = stats;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(
            (r.legs[0].expected_drop_fraction - 0.02).abs() < 1e-12,
            "{:?}",
            r.legs[0]
        );
    }

    /// The renewal model reduces to the binomial one for a non-burst
    /// phase, so fixed-mode verdicts are unchanged.
    #[test]
    fn drop_rate_fixed_mode_keeps_the_binomial_tolerance() {
        let n = 1_000_000u64;
        let mut inputs = healthy_inputs();
        inputs.legs[0].1.proxy_stats = proxy_stats(n - 20_000, 20_000, 2.0, None, 0);
        let r = build_soak_results(inputs).unwrap();
        let leg = &r.legs[0];
        assert!((leg.expected_drop_fraction - 0.02).abs() < 1e-12, "{leg:?}");
        let binomial = (6.0 * (0.02_f64 * 0.98 / n as f64).sqrt()).max(0.001);
        assert!(
            (leg.drop_fraction_tolerance - binomial).abs() < 1e-9,
            "{leg:?}"
        );
    }

    /// Outage-window drops enter the observed rate but not the
    /// expectation (the module doc's own "watch item"). Counted apart,
    /// they leave both `d` and `n`.
    #[test]
    fn outage_window_drops_are_excluded_from_the_drop_rate_verdict() {
        let n = 10_000_000u64;
        let build = |outage_dropped: u64| {
            let mut stats = proxy_stats(n - 230_000, 230_000, 2.0, Some(21_600), 90);
            stats.phases[0].outage_dropped = outage_dropped;
            let mut inputs = healthy_inputs();
            inputs.legs[0].1.proxy_stats = stats;
            inputs.legs[0].1.outage_period_s = Some(21_600);
            build_soak_results(inputs).unwrap()
        };
        // 2.0 % continuous + 0.3 % of outage drops.
        let r = build(30_000);
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("30000 outage-window drop(s) excluded"),
            "{}",
            v.detail
        );
        assert!(
            (r.legs[0].observed_drop_fraction - 200_000.0 / 9_970_000.0).abs() < 1e-9,
            "{:?}",
            r.legs[0]
        );
        // Reverse control: the same excess with nothing attributed to
        // an outage is still an unexplained 0.3 pp and still fails.
        let r = build(0);
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(!v.pass, "{}", v.detail);
        // Malformed: more outage drops than drops.
        let r = build(230_001);
        let v = verdict(&r, "drop_rate_consistent_with_impairment_srt");
        assert!(
            !v.pass && v.detail.contains("malformed artifact"),
            "{}",
            v.detail
        );
    }

    /// The `n == 0` degenerate case must still produce a FINITE
    /// tolerance (never NaN/infinity, which `serde_json` can't
    /// serialize) even though `drop_pass` is already forced false
    /// by the `total != 0` guard regardless of the tolerance value.
    #[test]
    fn zero_packets_tolerance_is_finite_not_nan_or_infinite() {
        let samples = vec![
            rss_row("srt", "send", 0.0, Some(1000)),
            rss_row("srt", "proxy", 0.0, Some(1000)),
            rss_row("srt", "recv", 0.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(0, 0, 2.0, None, 0),
                recv_report: passing_recv_report(0),
                send_metrics: cell_metrics(0),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(0.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let leg = &results.legs[0];
        assert!(leg.drop_fraction_tolerance.is_finite());
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "drop_rate_consistent_with_impairment_srt")
            .unwrap();
        assert!(!v.pass, "zero packets crossing the proxy must still fail");
    }

    // (d) malformed CSV -> loud, line-naming error.
    #[test]
    fn malformed_csv_is_a_loud_error() {
        let err = parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,srt,send,not-a-pid,1000\n")
            .expect_err("non-numeric pid must be rejected");
        assert!(err.contains("line 2"), "{err}");

        let err = parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,mars,send,1000,1000\n")
            .expect_err("unknown leg must be rejected");
        assert!(err.contains("unknown leg"), "{err}");

        let err = parse_rss_csv("elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1000\n")
            .expect_err("wrong field count must be rejected");
        assert!(err.contains("expected 5 fields"), "{err}");

        let err =
            parse_rss_csv("not,the,right,header\n").expect_err("wrong header must be rejected");
        assert!(err.contains("header"), "{err}");
    }

    #[test]
    fn zero_process_exits_when_every_row_has_rss() {
        let samples = vec![
            rss_row("srt", "send", 0.0, Some(1000)),
            rss_row("srt", "send", 30.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: Vec::new(),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(30.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "zero_process_exits")
            .unwrap();
        assert!(v.pass);
        assert!(results.process_exits.is_empty());
    }

    #[test]
    fn a_missing_rss_kb_row_is_recorded_as_a_process_exit_and_fails() {
        let samples = vec![
            rss_row("srt", "recv", 0.0, Some(1000)),
            rss_row("srt", "recv", 30.0, None),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: Vec::new(),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(30.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");
        assert_eq!(results.process_exits.len(), 1);
        assert_eq!(results.process_exits[0].elapsed_s, 30.0);
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "zero_process_exits")
            .unwrap();
        assert!(!v.pass);
        assert!(!results.overall_pass);
    }

    /// One empty read between live samples of the same pid is a missed
    /// read: no exit, the verdict passes, and the miss is a limitation.
    #[test]
    fn a_lone_empty_rss_sample_followed_by_live_rows_is_a_limitation_not_an_exit() {
        let samples = vec![
            rss_row("rist", "send", 420.0, Some(1000)),
            rss_row("rist", "send", 450.0, None),
            rss_row("rist", "send", 480.0, Some(1000)),
        ];
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: Vec::new(),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(30.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");
        assert!(
            results.process_exits.is_empty(),
            "{:?}",
            results.process_exits
        );
        let v = results
            .verdicts
            .iter()
            .find(|v| v.name == "zero_process_exits")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(
            results
                .limitations
                .iter()
                .any(|l| l.starts_with("sample_misses: 1 ")
                    && l.contains("rist/send pid 1000 at 450 s")),
            "{:?}",
            results.limitations
        );
    }

    /// A pid that goes empty and never reports again is an exit, even
    /// when another pid of the same process name keeps reporting.
    #[test]
    fn a_pid_that_stops_reporting_is_an_exit_while_a_restarted_pid_reports() {
        let mut gone = rss_row("srt", "send", 60.0, None);
        gone.pid = 7;
        let mut before = rss_row("srt", "send", 30.0, Some(1000));
        before.pid = 7;
        let mut restarted = rss_row("srt", "send", 90.0, Some(1000));
        restarted.pid = 8;
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: vec![before, gone, restarted],
            legs: Vec::new(),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(30.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");
        assert_eq!(results.process_exits.len(), 1);
        assert_eq!(results.process_exits[0].pid, 7);
        assert!(!results.overall_pass);
    }

    // File-driven round trip, mirroring `merge_and_render_end_to_end_via_tempdir`'s
    // convention above.
    #[test]
    fn run_reads_files_and_writes_soak_results_json() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-soak-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let rss_path = dir.join("rss.csv");
        std::fs::write(
            &rss_path,
            "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
        )
        .expect("write rss.csv");

        let config_path = dir.join("soak-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string(&cfg(180.0, 30.0)).unwrap(),
        )
        .expect("write soak-config.json");
        let exits_path = dir.join("exits.json");
        std::fs::write(
            &exits_path,
            serde_json::to_string(&six_clean_exits()).unwrap(),
        )
        .expect("write exits.json");

        let proxy_path = dir.join("proxy-stats.json");
        std::fs::write(
            &proxy_path,
            serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
        )
        .expect("write proxy stats");
        let recv_path = dir.join("recv-report.json");
        std::fs::write(
            &recv_path,
            serde_json::to_string(&passing_recv_report(1000)).unwrap(),
        )
        .expect("write recv report");
        let send_path = dir.join("send-report.json");
        std::fs::write(
            &send_path,
            serde_json::to_string(&cell_metrics(1000)).unwrap(),
        )
        .expect("write send report");

        let out_path = dir.join("soak-results.json");
        let results = run(
            &rss_path,
            &config_path,
            &exits_path,
            &proxy_path,
            &recv_path,
            &send_path,
            0,
            None,
            &[],
            None,
            None,
            &out_path,
        )
        .expect("run must succeed");

        assert!(out_path.exists());
        assert_eq!(results.legs.len(), 1);
        assert_eq!(results.legs[0].leg, "srt");
        let written = std::fs::read_to_string(&out_path).expect("read written soak-results.json");
        assert!(written.contains("\"overall_pass\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (Critical fix regression) A header-only `rss.csv` (zero data
    /// rows — the exact shape a crashed-before-first-tick sampler
    /// would produce) must be a hard error through the FULL
    /// file-driven path, not just a quietly-empty `rss_samples` in
    /// the pure function — `run` must neither write `--out` nor
    /// return a clean, vacuously-passing report.
    #[test]
    fn header_only_rss_csv_is_a_hard_error_through_run() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-soak-header-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let rss_path = dir.join("rss.csv");
        std::fs::write(&rss_path, "elapsed_s,leg,process,pid,rss_kb\n")
            .expect("write header-only rss.csv");

        let config_path = dir.join("soak-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string(&cfg(180.0, 30.0)).unwrap(),
        )
        .expect("write soak-config.json");
        let exits_path = dir.join("exits.json");
        std::fs::write(
            &exits_path,
            serde_json::to_string(&six_clean_exits()).unwrap(),
        )
        .expect("write exits.json");

        let proxy_path = dir.join("proxy-stats.json");
        std::fs::write(
            &proxy_path,
            serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
        )
        .expect("write proxy stats");
        let recv_path = dir.join("recv-report.json");
        std::fs::write(
            &recv_path,
            serde_json::to_string(&passing_recv_report(1000)).unwrap(),
        )
        .expect("write recv report");
        let send_path = dir.join("send-report.json");
        std::fs::write(
            &send_path,
            serde_json::to_string(&cell_metrics(1000)).unwrap(),
        )
        .expect("write send report");

        let out_path = dir.join("soak-results.json");
        let err = run(
            &rss_path,
            &config_path,
            &exits_path,
            &proxy_path,
            &recv_path,
            &send_path,
            0,
            None,
            &[],
            None,
            None,
            &out_path,
        )
        .expect_err("a header-only rss.csv must be a hard error, not a clean pass");
        assert!(
            err.contains("zero data rows"),
            "error must name the problem: {err}"
        );
        assert!(
            !out_path.exists(),
            "run must not write --out on this error path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `report soak` judges a run against a DECLARED config — if
    /// that file is missing, there is nothing to judge against, so
    /// `run` must error (naming the missing path) and write no
    /// `--out`, exactly like the other precondition failures above.
    #[test]
    fn run_without_config_file_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-soak-no-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let rss_path = dir.join("rss.csv");
        std::fs::write(
            &rss_path,
            "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
        )
        .expect("write rss.csv");

        let exits_path = dir.join("exits.json");
        std::fs::write(
            &exits_path,
            serde_json::to_string(&six_clean_exits()).unwrap(),
        )
        .expect("write exits.json");

        let proxy_path = dir.join("proxy-stats.json");
        std::fs::write(
            &proxy_path,
            serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
        )
        .expect("write proxy stats");
        let recv_path = dir.join("recv-report.json");
        std::fs::write(
            &recv_path,
            serde_json::to_string(&passing_recv_report(1000)).unwrap(),
        )
        .expect("write recv report");
        let send_path = dir.join("send-report.json");
        std::fs::write(
            &send_path,
            serde_json::to_string(&cell_metrics(1000)).unwrap(),
        )
        .expect("write send report");

        let out_path = dir.join("soak-results.json");
        let err = run(
            &rss_path,
            &dir.join("missing-config.json"),
            &exits_path,
            &proxy_path,
            &recv_path,
            &send_path,
            21600,
            None,
            &[],
            None,
            None,
            &out_path,
        )
        .unwrap_err();
        assert!(err.contains("missing-config.json"), "{err}");
        assert!(
            !out_path.exists(),
            "no results may be written on a config error"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Twin of the test above for `--exits`: `report soak` judges a
    /// run against DECLARED worker exit statuses too — if that file
    /// is missing, `run` must error (naming the missing path) and
    /// write no `--out`, exactly like a missing `--config`.
    #[test]
    fn run_without_exits_file_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "tst-interop-soak-no-exits-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let rss_path = dir.join("rss.csv");
        std::fs::write(
            &rss_path,
            "elapsed_s,leg,process,pid,rss_kb\n0,srt,send,1,1000\n30,srt,send,2,1000\n",
        )
        .expect("write rss.csv");

        let config_path = dir.join("soak-config.json");
        std::fs::write(
            &config_path,
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .expect("write soak-config.json");

        let proxy_path = dir.join("proxy-stats.json");
        std::fs::write(
            &proxy_path,
            serde_json::to_string(&proxy_stats(1000, 0, 0.0, None, 0)).unwrap(),
        )
        .expect("write proxy stats");
        let recv_path = dir.join("recv-report.json");
        std::fs::write(
            &recv_path,
            serde_json::to_string(&passing_recv_report(1000)).unwrap(),
        )
        .expect("write recv report");
        let send_path = dir.join("send-report.json");
        std::fs::write(
            &send_path,
            serde_json::to_string(&cell_metrics(1000)).unwrap(),
        )
        .expect("write send report");

        let out_path = dir.join("soak-results.json");
        let err = run(
            &rss_path,
            &config_path,
            &dir.join("missing-exits.json"),
            &proxy_path,
            &recv_path,
            &send_path,
            21600,
            None,
            &[],
            None,
            None,
            &out_path,
        )
        .unwrap_err();
        assert!(err.contains("missing-exits.json"), "{err}");
        assert!(
            !out_path.exists(),
            "no results may be written on an exits error"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (Critical fix regression) `build_soak_results` itself, given
    /// a completely empty `rss_samples`, errors rather than
    /// returning a vacuously-passing `SoakResults` — the pure-
    /// function-level twin of the file-driven test above.
    #[test]
    fn empty_rss_samples_is_a_hard_error() {
        let err = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: Vec::new(),
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(0.0, 30.0),
            worker_exits: six_clean_exits(),
        })
        .expect_err("empty rss_samples must be a hard error, not a vacuous pass");
        assert!(err.contains("zero data rows"), "{err}");
    }

    /// (Critical fix regression) `rss.csv` has real data, but one
    /// entire expected process ("recv") never appears for the `srt`
    /// leg — a partial-sampler-death shape distinct from the
    /// completely-empty case above. Must produce a named, FAILING,
    /// non-provisional `rss_data_present_srt_recv` verdict (not just
    /// a silently-shorter `rss_slopes` list) and flip `overall_pass`
    /// false, while the two processes that DO have data still get
    /// their normal, passing verdicts.
    #[test]
    fn missing_one_process_entirely_fails_with_a_named_verdict() {
        let mut samples = Vec::new();
        for i in 0..20 {
            let t = i as f64 * 60.0;
            samples.push(rss_row("srt", "send", t, Some(50_000)));
            samples.push(rss_row("srt", "proxy", t, Some(20_000)));
            // "recv" never appears at all.
        }
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(1140.0, 60.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        let missing = results
            .verdicts
            .iter()
            .find(|v| v.name == "rss_data_present_srt_recv")
            .expect("a named verdict must flag the entirely-missing process");
        assert!(!missing.pass);
        assert!(!missing.provisional);
        assert!(!results.overall_pass);

        // The two processes that DO have data are unaffected —
        // this isn't a blanket "any gap anywhere fails everything"
        // check, just the missing combination.
        let send_slope = results
            .verdicts
            .iter()
            .find(|v| v.name == "rss_slope_srt_send")
            .expect("send verdict must still be present");
        assert!(send_slope.pass);
        assert!(
            !results
                .verdicts
                .iter()
                .any(|v| v.name.starts_with("rss_data_present_srt_send")),
            "a process that DOES have data must not also get a missing-data verdict"
        );
    }

    /// (Folds Minor 6; contract updated by the completeness-verdicts
    /// wave) Exactly one post-warmup sample per process — distinct
    /// from the entirely-missing case above: this data IS present,
    /// just too thin for a real regression. Under the OLD contract
    /// this regressed to a 0.0 slope; under the NEW one, a single
    /// post-warmup sample can't clear `rss_sample_coverage_*`
    /// (fewer than 2 distinct timestamps), so it's reported as
    /// missing evidence — no `rss_slope_*` verdict at all — rather
    /// than a (misleadingly confident-looking) flat 0.0 slope. A
    /// single present sample must still NOT trip the OLDER,
    /// separate `rss_data_present_*` missing-data check (that one
    /// is about zero samples, not "not enough for a confident
    /// slope").
    #[test]
    fn single_post_warmup_sample_is_missing_evidence_not_zero_slope() {
        // Two ticks per process: one at t=0 (pre-warmup, with a
        // declared 6000s run -> warmup_s=min(1800, 1000)=1000s,
        // excluding it) and one at t=6000 (post-warmup, the lone
        // surviving sample).
        let mut samples = Vec::new();
        for process in ["send", "proxy", "recv"] {
            samples.push(rss_row("srt", process, 0.0, Some(10_000)));
            samples.push(rss_row("srt", process, 6000.0, Some(10_500)));
        }
        let results = build_soak_results(SoakInputs {
            publish_legs: Vec::new(),
            whep_legs: Vec::new(),
            rss_samples: samples,
            legs: one_leg(LegArtifacts {
                proxy_stats: proxy_stats(1000, 0, 0.0, None, 0),
                recv_report: passing_recv_report(1000),
                send_metrics: cell_metrics(1000),
                outage_period_s: None,
            }),
            rss_slope_threshold_kb_per_hour: None,
            config: cfg(6000.0, 6000.0),
            worker_exits: six_clean_exits(),
        })
        .expect("non-empty rss_samples must not error");

        assert!(
            results.rss_slopes.is_empty(),
            "a single post-warmup sample must not produce a slope: {:?}",
            results.rss_slopes
        );
        for process in ["send", "proxy", "recv"] {
            let v = results
                .verdicts
                .iter()
                .find(|v| v.name == format!("rss_sample_coverage_srt_{process}"))
                .unwrap_or_else(|| panic!("coverage verdict for {process} must be present"));
            assert!(!v.pass, "{}", v.detail);
        }
        assert!(
            !results
                .verdicts
                .iter()
                .any(|v| v.name.starts_with("rss_slope_")),
            "no rss_slope_* verdict must be emitted when coverage fails"
        );
        assert!(
            !results
                .verdicts
                .iter()
                .any(|v| v.name.starts_with("rss_data_present_")),
            "a single present sample is not 'missing data'"
        );
        assert!(!results.overall_pass);
    }

    #[test]
    fn parses_soak_config_json() {
        let cfg = parse_soak_config(
            r#"{"expected_duration_s": 259200, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .expect("valid config parses");
        assert_eq!(cfg.expected_duration_s, 259200.0);
        assert_eq!(cfg.rss_cadence_s, 30.0);
        assert!(cfg.expected_worker_exits.is_empty());
    }

    #[test]
    fn soak_config_rejects_nonpositive_duration_or_cadence() {
        let e = parse_soak_config(
            r#"{"expected_duration_s": 0, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(e.contains("expected_duration_s"), "{e}");
        let e = parse_soak_config(
            r#"{"expected_duration_s": 100, "rss_cadence_s": -1, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(e.contains("rss_cadence_s"), "{e}");
    }

    /// `sampler_end_slack_s >= expected_duration_s` makes the
    /// achievable RSS span non-positive (it is `expected_duration_s`
    /// minus the slack minus two cadences) before a single sample is
    /// even examined — every real run would fail `duration_coverage`
    /// unconditionally, so this is a config error, not a run outcome.
    #[test]
    fn soak_config_rejects_slack_that_consumes_the_run() {
        let e = parse_soak_config(
            r#"{"expected_duration_s": 180, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 180, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(
            e.contains("sampler_end_slack_s") && e.contains("expected_duration_s"),
            "{e}"
        );
        assert!(e.contains("180"), "{e}");
        // Strictly greater than is rejected too.
        let e = parse_soak_config(
            r#"{"expected_duration_s": 180, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 200, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(
            e.contains("sampler_end_slack_s") && e.contains("expected_duration_s"),
            "{e}"
        );
    }

    /// A config can pass the previous test's simple `slack <
    /// duration` check yet still leave `duration_coverage`'s minimum
    /// accepted span (`expected_duration_s - sampler_end_slack_s -
    /// 2*rss_cadence_s`) at or below zero, which passes vacuously —
    /// this is `--hours 0.02` (72s) at the real 30s/35s cadence/slack
    /// soak.sh always uses: 72 - 35 - 60 = -23.
    #[test]
    fn soak_config_rejects_run_shorter_than_slack_plus_two_cadences() {
        let e = parse_soak_config(
            r#"{"expected_duration_s": 72, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(
            e.contains("expected_duration_s")
                && e.contains("sampler_end_slack_s")
                && e.contains("rss_cadence_s"),
            "{e}"
        );
        assert!(e.contains("72"), "{e}");
        // The real smoke config (3600/30/0.1667/35) must still validate.
        parse_soak_config(
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}}"#,
        )
        .expect("the real smoke config must still validate");
        // The `cfg(1140.0, 60.0)` test helper's parameters, run through
        // the real parser, must still validate too.
        parse_soak_config(
            r#"{"expected_duration_s": 1140, "rss_cadence_s": 60, "warmup_fraction": 0.16666666666666666,
                "sampler_end_slack_s": 0, "expected_worker_exits": {}}"#,
        )
        .expect("the cfg(1140.0, 60.0) helper's parameters must still validate");
    }

    /// A config can leave `min_span_s` comfortably positive yet still
    /// imply fewer than 2 post-warmup samples once the warmup window
    /// (`min(1800, expected_duration_s * warmup_fraction)`) and the
    /// cadence are accounted for — `rss_sample_coverage_*` needs >= 2
    /// distinct timestamps to compute a gap at all.
    #[test]
    fn soak_config_rejects_fewer_than_two_implied_post_warmup_samples() {
        // duration 1000, cadence 400, slack 0: min_span = 1000-0-800 =
        // 200 > 0 (passes the duration check), but warmup_s =
        // min(1800, 1000*0.5) = 500, so expected_samples =
        // floor((1000-0-500)/400) = 1 < 2.
        let e = parse_soak_config(
            r#"{"expected_duration_s": 1000, "rss_cadence_s": 400, "warmup_fraction": 0.5,
                "sampler_end_slack_s": 0, "expected_worker_exits": {}}"#,
        )
        .unwrap_err();
        assert!(
            e.contains("expected_duration_s")
                && e.contains("rss_cadence_s")
                && e.contains("warmup"),
            "{e}"
        );
    }

    /// A key in `expected_worker_exits` outside the six `WORKER_ROLES`
    /// is a config typo (a renamed role, a misspelled one) that must
    /// be rejected at parse time rather than silently doing nothing —
    /// the entry would never match anything in `exits.json` either.
    #[test]
    fn soak_config_rejects_unknown_expected_role() {
        let e = parse_soak_config(
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "expected_worker_exits": {"bogus-role": 137}}"#,
        )
        .unwrap_err();
        assert!(e.contains("bogus-role"), "{e}");
    }

    #[test]
    fn parses_worker_exits_json() {
        let exits = parse_worker_exits(r#"{"srt-send": 0, "srt-recv": 1, "rist-proxy": 143}"#)
            .expect("valid exits parse");
        assert_eq!(exits["srt-recv"], 1);
        assert_eq!(exits.len(), 3);
        assert!(parse_worker_exits("{}").unwrap().is_empty());
        assert!(parse_worker_exits("not json").is_err());
    }

    #[test]
    fn corruption_verdicts_pass_from_a_clean_attribution_report() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        let a = crate::corrupt::AttributionReport {
            injected: 12,
            detectable: 10,
            resolved: 12,
            ..Default::default()
        };
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(a);
        let r = build_soak_results(inputs).unwrap();
        for n in [
            "corruption_attributed_srt",
            "corruption_detected_srt",
            "corruption_recovered_srt",
        ] {
            let v = r
                .verdicts
                .iter()
                .find(|v| v.name == n)
                .unwrap_or_else(|| panic!("{n} missing"));
            assert!(v.pass && !v.provisional, "{n}: {}", v.detail);
        }
    }

    #[test]
    fn corruption_verdicts_fail_on_unexplained_undetected_unrecovered() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        let a = crate::corrupt::AttributionReport {
            injected: 3,
            unexplained_events: vec!["x".into()],
            unexplained_nonconformant: 1,
            undetected: vec!["y".into()],
            undetected_count: 1,
            unrecovered: vec!["z".into()],
            unrecovered_count: 1,
            ..Default::default()
        };
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(a);
        let r = build_soak_results(inputs).unwrap();
        for n in [
            "corruption_attributed_srt",
            "corruption_detected_srt",
            "corruption_recovered_srt",
        ] {
            assert!(!r.verdicts.iter().find(|v| v.name == n).unwrap().pass);
        }
        assert!(!r.overall_pass);
    }

    /// The `corruption_attributed` verdict must PASS/FAIL on, and
    /// report, the TRUE number of unexplained events rather than the
    /// length of the capped sample. `AttributionReport::
    /// unexplained_events` stops growing at `MAX_SAMPLES`, so on a
    /// long leg the two numbers diverge without bound: a reader of
    /// `soak-results.json` would size the damage from the cap instead
    /// of from the run.
    #[test]
    fn corruption_attributed_detail_reports_the_true_unexplained_count() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        // A sample truncated at 2 entries standing in for 500 real
        // unexplained events (1000 seen, 500 attributed).
        let a = crate::corrupt::AttributionReport {
            injected: 40,
            events: 1000,
            attributed_events: 500,
            unexplained_events: vec!["first one".into(), "second one".into()],
            unexplained_nonconformant: 500,
            ..Default::default()
        };
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(a);
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "corruption_attributed_srt")
            .unwrap();
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("500 unexplained"),
            "detail must name the true count, not the sample length: {}",
            v.detail
        );
        assert!(
            !v.detail.contains("2 unexplained"),
            "detail must not report the capped sample length: {}",
            v.detail
        );
        // The quoted example still comes from the sample.
        assert!(v.detail.contains("first one"), "{}", v.detail);
    }

    /// Build a corruption-enabled run whose SRT leg logged `sent`
    /// injections, of which the receiver ingested `ingested` and
    /// resolved `resolved`.
    fn corruption_inputs(sent: u64, ingested: u64, resolved: u64) -> SoakInputs {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        inputs.legs[0].1.send_metrics.corruption = Some(crate::corrupt::CorruptionStats {
            injections: sent,
            ..Default::default()
        });
        inputs.legs[0].1.recv_report.metrics.corruption_attribution =
            Some(crate::corrupt::AttributionReport {
                injected: ingested,
                resolved,
                ..Default::default()
            });
        inputs
    }

    /// The three corruption verdicts all pass on an empty finding
    /// list, so a run that injected NOTHING passes all of them while
    /// proving nothing at all. `corruption_coverage` is what stops
    /// that, and it has to fail on a tap that did no work.
    #[test]
    fn corruption_coverage_fails_a_run_with_no_injections() {
        let r = build_soak_results(corruption_inputs(0, 0, 0)).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(!v.provisional);
        // …while the three finding verdicts do pass vacuously, which
        // is exactly why this one exists.
        assert!(verdict(&r, "corruption_attributed_srt").pass);
        assert!(!r.overall_pass);
    }

    /// A receiver that read only part of the sender's log judges only
    /// part of the run, and every finding verdict would still pass.
    #[test]
    fn corruption_coverage_fails_when_the_receiver_missed_injections() {
        let r = build_soak_results(corruption_inputs(500, 480, 480)).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("500"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    /// Ingested in full but mostly unresolved: the receiver has the
    /// log and cannot place it on its own timeline, so the findings
    /// are drawn from a fraction of the injections.
    #[test]
    fn corruption_coverage_fails_when_most_injections_never_resolved() {
        let r = build_soak_results(corruption_inputs(500, 500, 400)).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("80.0%"), "{}", v.detail);
    }

    /// A healthy leg: every injection ingested, all but a handful
    /// resolved (outage-era strandings are expected and bounded).
    #[test]
    fn corruption_coverage_passes_a_healthy_leg() {
        let r = build_soak_results(corruption_inputs(500, 500, 480)).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("96.0%"), "{}", v.detail);
    }

    /// 999 of 1000 resolved is 99.9%, never "100.0%" — the percentage
    /// is floored, not rounded — and the unresolved count must be
    /// named in the text, not left implicit in the percentage.
    #[test]
    fn coverage_detail_names_unresolved_and_never_rounds_to_full() {
        // sent=1001 (not 1000) so the unrelated ingested-percentage
        // field doesn't also land on a coincidental "100.0%" — this
        // test is about the resolved stat alone.
        let mut inputs = corruption_inputs(1001, 1000, 999);
        inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap()
            .unresolved = 1;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(v.detail.contains("1 unresolved"), "{}", v.detail);
        assert!(!v.detail.contains("100.0%"), "{}", v.detail);
    }

    /// The detected verdict's detail names both halves of the credit,
    /// so a reader can see what the receiver reported vs what only the
    /// harness's raw reader caught.
    #[test]
    fn corruption_detected_detail_splits_demux_from_reader_credit() {
        let mut inputs = corruption_inputs(500, 500, 480);
        let a = inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap();
        a.detectable = 400;
        a.detected_by_demux = 350;
        a.detected_by_reader_only = 50;
        a.detected_by_reader_only_per_class = BTreeMap::from([("garbage".to_string(), 50)]);
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_detected_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail
                .contains("350 noticed by the demuxer, 50 only by the harness's raw reader"),
            "{}",
            v.detail
        );
        assert!(v.detail.contains("\"garbage\": 50"), "{}", v.detail);
    }

    /// The ingested half is a FLOOR, not an equality. Over 72 hours a
    /// single unlucky interleaving at teardown must not fail the
    /// whole run — but a leg that read nine injections in ten did not
    /// judge it.
    #[test]
    fn corruption_coverage_tolerates_a_short_tail_read_but_not_a_gap() {
        // 99.5% ingested: one injection missed out of 200.
        let ok = build_soak_results(corruption_inputs(200, 199, 199)).unwrap();
        let v = verdict(&ok, "corruption_coverage_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("99.5%"), "{}", v.detail);

        // 98%: four missed out of 200, which is a gap, not a tail.
        let bad = build_soak_results(corruption_inputs(200, 196, 196)).unwrap();
        let v = verdict(&bad, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("98.0%"), "{}", v.detail);
        assert!(!bad.overall_pass);
    }

    /// 100 of 101 ingested clears the 99 % floor, so an interior write
    /// failure passed `corruption_coverage` as if it were a teardown
    /// tail read. The sender's own latch must fail it.
    #[test]
    fn corruption_coverage_fails_when_the_sender_lost_a_log_line() {
        let mut inputs = corruption_inputs(101, 100, 100);
        inputs.legs[0]
            .1
            .send_metrics
            .corruption
            .as_mut()
            .unwrap()
            .log_write_failed = true;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail
                .contains("sender reported a corruption-log write failure"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);

        // Control: the same 100/101 WITHOUT the latch is the tolerated tail read.
        let r = build_soak_results(corruption_inputs(101, 100, 100)).unwrap();
        assert!(verdict(&r, "corruption_coverage_srt").pass);

        // The close-time shape (`Corrupter::close`'s final flush
        // failing on a buffering writer): every line the receiver saw
        // parsed and resolved, so ingestion is a PERFECT 101/101 and
        // no ratio can reveal the loss. The latch is the only
        // evidence, and it must still fail the verdict.
        let mut inputs = corruption_inputs(101, 101, 101);
        inputs.legs[0]
            .1
            .send_metrics
            .corruption
            .as_mut()
            .unwrap()
            .log_write_failed = true;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("100.0%"), "{}", v.detail);
        assert!(
            v.detail
                .contains("sender reported a corruption-log write failure"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    /// The counters the verdicts gate on are `#[serde(default)]`, so
    /// a recv report written before these counters existed
    /// deserializes with every one of them zero while its sample list
    /// still holds real findings. Judging on the counter alone would read such a
    /// report as clean — verified against a 1-hour smoke run's
    /// artifacts, where re-judging them flipped both
    /// `corruption_detected_*` failures to passes while the detail
    /// still quoted the finding.
    #[test]
    fn a_pre_wave_report_with_no_counters_is_still_judged_by_its_findings() {
        // Exactly what serde produces for an archived report: the
        // lists are present, every new counter defaults to 0.
        let archived: crate::corrupt::AttributionReport = serde_json::from_str(
            r#"{"injected":500,"detectable":400,"resolved":500,"unresolved":0,
                "events":100,"attributed_events":100,"attributed_nonconformant":0,
                "attributed_discontinuities":0,
                "unexplained_events":["OtherNonConformant on pid Some(4113) at packet 7"],
                "first_unexplained_nonconformant":null,
                "first_unexplained_discontinuity":null,
                "undetected":["body_flip on pid 0x1000 at packet 2938427"],
                "unrecovered":["drop on pid 0x1011 at packet 99"],
                "resyncs":0,"injected_fraction":0.0,
                "attribution_window":500,"recovery_bound":600}"#,
        )
        .expect("archived report deserializes");
        assert_eq!(archived.undetected_count, 0, "the counter really is absent");
        assert_eq!(archived.undetected_total(), 1, "but the finding is not");

        let mut inputs = corruption_inputs(500, 500, 500);
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(archived);
        let r = build_soak_results(inputs).unwrap();
        for name in [
            "corruption_detected_srt",
            "corruption_recovered_srt",
            "corruption_attributed_srt",
        ] {
            let v = verdict(&r, name);
            assert!(
                !v.pass,
                "{name} must fail on an archived finding: {}",
                v.detail
            );
            // …and say so with the same number it judged on.
            assert!(v.detail.contains(" 1 "), "{name}: {}", v.detail);
        }
        assert!(!r.overall_pass);
    }

    fn spec(rate: u32, min_gap: u64, classes: &[crate::corrupt::Class]) -> CorruptionDeclaration {
        CorruptionDeclaration {
            rate_per_10k: rate,
            min_gap,
            classes: classes.to_vec(),
        }
    }

    /// The spec in the sender's own log header must be the spec the
    /// config declared. A run that quietly injected at a different
    /// rate, or from a narrower class set, passes every finding
    /// verdict while its published evidence describes a different
    /// experiment.
    #[test]
    fn corruption_declared_matches_the_log_header_spec() {
        let classes = crate::corrupt::Class::ALL.to_vec();
        let mut inputs = corruption_inputs(500, 500, 500);
        inputs.config.corruption_spec = Some(spec(5, 1000, &classes));
        let attr = inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap();
        attr.rate_per_10k = 5;
        attr.min_gap = 1000;
        attr.classes = classes;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_declared_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(!v.provisional);
    }

    #[test]
    fn corruption_declared_fails_on_a_rate_the_sender_did_not_use() {
        let classes = crate::corrupt::Class::ALL.to_vec();
        let mut inputs = corruption_inputs(500, 500, 500);
        inputs.config.corruption_spec = Some(spec(5, 1000, &classes));
        let attr = inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap();
        attr.rate_per_10k = 50;
        attr.min_gap = 1000;
        attr.classes = classes;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("50/10k"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    #[test]
    fn corruption_declared_fails_on_a_narrower_class_set() {
        let mut inputs = corruption_inputs(500, 500, 500);
        inputs.config.corruption_spec = Some(spec(5, 1000, &crate::corrupt::Class::ALL));
        let attr = inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap();
        attr.rate_per_10k = 5;
        attr.min_gap = 1000;
        attr.classes = vec![crate::corrupt::Class::BodyFlip];
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "corruption_declared_srt").pass);
    }

    /// An archived config that declared the tap on but carried no
    /// spec is not retroactively a failure.
    #[test]
    fn corruption_declared_passes_an_undeclared_spec() {
        let r = build_soak_results(corruption_inputs(500, 500, 500)).unwrap();
        let v = verdict(&r, "corruption_declared_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("no corruption spec declared"),
            "{}",
            v.detail
        );
    }

    /// Under lossy judgement every corruption verdict passes a leg whose
    /// findings were ALL written off as transport loss. Both soak legs
    /// are ARQ, so excusals are bounded by the outage schedule:
    /// `K x windows + C` with K = the profile's media PIDs and C = 8.
    #[test]
    fn corruption_excusal_budget_fails_a_leg_that_excused_everything() {
        let mut inputs = corruption_inputs(500, 500, 480);
        inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap()
            .undetected_lost = 500;
        let r = build_soak_results(inputs).unwrap();
        for n in [
            "attributed",
            "detected",
            "recovered",
            "coverage",
            "declared",
        ] {
            assert!(
                verdict(&r, &format!("corruption_{n}_srt")).pass,
                "{n} passes vacuously"
            );
        }
        let v = verdict(&r, "corruption_excusal_budget_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(!v.provisional);
        // No profile declared and no outage: K falls back to 2, W = 0,
        // so the whole budget is the one-off base plus one hour of rate.
        assert!(
            v.detail
                .contains("budget 24 (K=2 media PIDs x 0 outage window(s) + 8 + 16/h x 1.00h)"),
            "{}",
            v.detail
        );
        assert!(v.detail.contains("500 excused"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    #[test]
    fn corruption_excusal_budget_passes_inside_the_budget_and_counts_media_pids() {
        let mut inputs = corruption_inputs(500, 500, 480);
        {
            let a = inputs.legs[0]
                .1
                .recv_report
                .metrics
                .corruption_attribution
                .as_mut()
                .unwrap();
            // Exactly at the budget (K=4 x 0 windows + BASE): the
            // comparison is `<=`, so the boundary must pass.
            a.undetected_lost = 10;
            a.unrecovered_lost = 7;
            a.unexplained_transport_loss = 7;
        }
        inputs.legs[0].1.recv_report.profile = Some("two-program".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_excusal_budget_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("K=4 media PIDs"), "{}", v.detail);
        assert!(v.detail.contains("24 excused"), "{}", v.detail);
        assert!(v.detail.contains("against budget 24"), "{}", v.detail);
    }

    /// The excusals an ARQ leg legitimately accrues are per HOUR of
    /// run, not per outage window (measured: `/tmp/soak-resmoke`'s
    /// rist leg excused 9 in one hour with ZERO outage windows), so
    /// the budget has to grow with the run. The same excusal count
    /// that blows a short run's budget is inside a long one's.
    #[test]
    fn excusal_budget_grows_with_the_run_duration() {
        // K=2, W=0 either way: only the rate term moves.
        assert_eq!(excusal_budget(2, 0, 3600.0, 0.0), 8 + 16);
        assert_eq!(excusal_budget(2, 0, 72.0 * 3600.0, 0.0), 8 + 16 * 72);
        // Partial hours round UP, so a 3541 s smoke run still gets a
        // whole hour's allowance.
        assert_eq!(excusal_budget(2, 0, 3541.0, 0.0), 8 + 16);
        // Outage windows still contribute on top.
        assert_eq!(excusal_budget(4, 11, 72.0 * 3600.0, 0.0), 44 + 8 + 16 * 72);
        // No measured span (single sample / sampler never ticked):
        // fall back to the declared duration rather than collapsing
        // the rate term and failing a leg for a telemetry gap.
        assert_eq!(excusal_budget(2, 0, 0.0, 2.0 * 3600.0), 8 + 32);

        // End to end: 40 excusals fail a 1-hour run and pass a 3-hour
        // one, from the same artifacts.
        let build = |span_s: f64| {
            let n = (span_s / 30.0) as usize + 1;
            let mut inputs = corruption_inputs(500, 500, 480);
            inputs.rss_samples = flat_series(n, 30.0);
            inputs.config.expected_duration_s = span_s;
            let a = inputs.legs[0]
                .1
                .recv_report
                .metrics
                .corruption_attribution
                .as_mut()
                .unwrap();
            a.unexplained_transport_loss = 40;
            build_soak_results(inputs).unwrap()
        };
        let short = build(3600.0);
        let v = verdict(&short, "corruption_excusal_budget_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("budget 24"), "{}", v.detail);

        let long = build(3.0 * 3600.0);
        let v = verdict(&long, "corruption_excusal_budget_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("budget 56"), "{}", v.detail);
    }

    /// The demuxer's own unexplained discontinuities are held to the
    /// same budget — a demuxer emitting spurious CC jumps under lossy
    /// judgement used to pass every corruption verdict. The rate term
    /// applies to this clause too, not only to `excused`.
    #[test]
    fn corruption_excusal_budget_bounds_unexplained_discontinuities() {
        let mut inputs = corruption_inputs(500, 500, 480);
        inputs.legs[0].1.recv_report.metrics.discontinuities = 1_000;
        inputs.legs[0]
            .1
            .recv_report
            .metrics
            .corruption_attribution
            .as_mut()
            .unwrap()
            .attributed_discontinuities = 100;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "corruption_excusal_budget_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("900 unexplained discontinuit"),
            "{}",
            v.detail
        );
        // …and it is the SAME budget the excusal count is held to,
        // rate term included.
        assert!(v.detail.contains("+ 8 + 16/h x 1.00h"), "{}", v.detail);
    }

    /// A leg declared `rich` whose receiver judged it in compact mode
    /// ran no rich-KLV oracle at all — and every rich verdict is
    /// skipped rather than failed, so nothing else catches it.
    #[test]
    fn klv_declared_fails_a_rich_leg_with_no_rich_block() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        inputs.config.legs.get_mut("srt").unwrap().klv_set = Some("rich".into());
        assert!(inputs.legs[0].1.recv_report.metrics.klv_rich.is_none());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(!v.provisional);
        assert!(!r.overall_pass);
    }

    #[test]
    fn klv_declared_passes_a_rich_leg_that_was_judged_rich() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        let leg = inputs.config.legs.get_mut("srt").unwrap();
        leg.klv_set = Some("rich".into());
        leg.klv_seed = Some(3);
        inputs.legs[0].1.recv_report.metrics.klv_rich = Some(crate::report_types::KlvRichMetrics {
            records: 6110,
            ..Default::default()
        });
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_declared_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("6110"), "{}", v.detail);
    }

    /// The other direction: a compact declaration with a rich block
    /// is just as much a drift between the recipe and the run.
    #[test]
    fn klv_declared_fails_a_compact_leg_that_was_judged_rich() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        inputs.config.legs.get_mut("srt").unwrap().klv_set = Some("compact".into());
        inputs.legs[0].1.recv_report.metrics.klv_rich =
            Some(crate::report_types::KlvRichMetrics::default());
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "klv_declared_srt").pass);
    }

    #[test]
    fn parse_soak_config_rejects_an_unknown_klv_set_and_an_invalid_spec() {
        let base = |extra: &str| {
            format!(
                r#"{{"expected_duration_s":3600,"rss_cadence_s":30,"warmup_fraction":0.1667,
                    "sampler_end_slack_s":35{extra}}}"#
            )
        };
        let bad_set = base(r#","legs":{"srt":{"profile":"baseline","klv_set":"lavish"}}"#);
        let e = parse_soak_config(&bad_set).unwrap_err();
        assert!(e.contains("lavish"), "{e}");

        let bad_spec = base(
            r#","corruption":true,"corruption_spec":{"rate_per_10k":0,"min_gap":1000,"classes":["drop"]}"#,
        );
        let e = parse_soak_config(&bad_spec).unwrap_err();
        assert!(e.contains("corruption_spec"), "{e}");

        // …and the well-formed pair parses.
        let ok = base(
            r#","corruption":true,"corruption_spec":{"rate_per_10k":5,"min_gap":1000,"classes":["drop","body_flip"]},"legs":{"srt":{"profile":"baseline","klv_set":"rich","klv_seed":3}}"#,
        );
        let cfg = parse_soak_config(&ok).unwrap();
        assert_eq!(cfg.corruption_spec.unwrap().rate_per_10k, 5);
        assert_eq!(cfg.legs["srt"].klv_set.as_deref(), Some("rich"));
        assert_eq!(cfg.legs["srt"].klv_seed, Some(3));
    }

    /// A run declared tap-off still emits the whole family, so a
    /// reader diffing two runs' verdict sets sees the same names.
    #[test]
    fn a_tap_disabled_run_still_emits_the_coverage_verdict() {
        let r = build_soak_results(healthy_inputs()).unwrap();
        let v = verdict(&r, "corruption_coverage_srt");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("disabled"), "{}", v.detail);
    }

    /// The shape a long lossy leg actually produces: the sample list
    /// is EMPTY (the run's excused continuity jumps used to fill it,
    /// and a sample that never had room for the real finding stays
    /// empty) while the uncapped counters say something was
    /// unexplained. Gating on the list would pass this run.
    #[test]
    fn corruption_attributed_fails_on_a_counted_finding_with_an_empty_sample() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        let a = crate::corrupt::AttributionReport {
            injected: 5000,
            resolved: 5000,
            events: 12_000,
            attributed_events: 8_000,
            unexplained_events: vec![],
            unexplained_resyncs: 1,
            unexplained_transport_loss: 3_999,
            ..Default::default()
        };
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(a);
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "corruption_attributed_srt")
            .unwrap();
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("1 resync"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    /// A PASSING `corruption_attributed` must not claim unexplained
    /// events in its own detail. Lossy mode removes the
    /// discontinuity-family events it excused from
    /// `unexplained_events` (which is what the verdict passes on) but
    /// leaves them inside `events - attributed_events`, so the
    /// displayed count has to subtract `unexplained_transport_loss`
    /// too or the string contradicts the verdict beside it. This is
    /// the exact shape a real soak leg produced: 251 events, 249
    /// attributed, 2 excused.
    #[test]
    fn a_passing_corruption_attributed_detail_reports_zero_unexplained() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        let a = crate::corrupt::AttributionReport {
            injected: 287,
            resolved: 287,
            events: 251,
            attributed_events: 249,
            // Empty: Lossy moved both leftovers to the counter below.
            unexplained_events: vec![],
            unexplained_transport_loss: 2,
            ..Default::default()
        };
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = Some(a);
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "corruption_attributed_srt")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("0 unexplained"),
            "a passing verdict must report zero unexplained: {}",
            v.detail
        );
        assert!(
            v.detail.contains("2 excused as transport loss"),
            "the excused events must still be named: {}",
            v.detail
        );
    }

    #[test]
    fn corruption_declared_but_no_attribution_in_the_recv_report_fails() {
        let mut inputs = healthy_inputs();
        inputs.config.corruption = true;
        inputs.legs[0].1.recv_report.metrics.corruption_attribution = None;
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "corruption_attributed_srt")
            .unwrap();
        assert!(!v.pass && v.detail.contains("no attribution"));
    }

    #[test]
    fn corruption_off_in_config_yields_passing_disabled_verdicts() {
        let inputs = healthy_inputs(); // corruption: false
        let r = build_soak_results(inputs).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "corruption_attributed_srt")
            .unwrap();
        assert!(v.pass && v.detail.contains("disabled"));
    }

    /// Declare a profile (and optionally a schedule) for the `srt` leg
    /// of a [`healthy_inputs`] run.
    fn declare_srt(inputs: &mut SoakInputs, profile: &str, schedule: Option<ScheduleDeclaration>) {
        inputs.config.legs.insert(
            "srt".to_string(),
            LegDeclaration {
                profile: profile.to_string(),
                schedule,
                klv_set: None,
                klv_seed: None,
                reconnect_mode: None,
                publisher: None,
                publisher_drop_period_s: None,
                publisher_generations: None,
                source_sha256: None,
                remuxed: None,
                klv_records_per_generation: None,
                klv_placement: None,
                mount_kind: None,
                viewer: None,
                viewer_reconnect_s: None,
                viewer_connections: None,
            },
        );
    }

    /// Give the `srt` leg's proxy stats a real scheduled-mode echo:
    /// the generated table for `(seed, phases)` plus per-phase
    /// counters whose drop counts match each phase's own `loss_pct`,
    /// so the co-resident `drop_rate_consistent_with_impairment_srt`
    /// verdict stays passing and these tests probe only the
    /// declaration check.
    fn echo_srt_schedule(inputs: &mut SoakInputs, seed: u64, phases: u32, phase_s: u64) {
        const PER_PHASE_PACKETS: u64 = 100_000;
        let table = crate::impair::generate_schedule(seed, phases);
        inputs.legs[0].1.proxy_stats.config.schedule = Some(crate::proxy::ScheduleEcho {
            seed,
            phases,
            phase_s,
            table: table.clone(),
        });
        inputs.legs[0].1.proxy_stats.phases = table
            .iter()
            .map(|p| {
                let dropped = (PER_PHASE_PACKETS as f64 * p.loss_pct / 100.0).round() as u64;
                crate::proxy::PhaseCounters {
                    index: p.index,
                    forwarded: PER_PHASE_PACKETS - dropped,
                    dropped,
                    duped: 0,
                    outage_dropped: 0,
                }
            })
            .collect();
        let stats = &mut inputs.legs[0].1.proxy_stats;
        stats.forwarded = stats.phases.iter().map(|c| c.forwarded).sum();
        stats.dropped = stats.phases.iter().map(|c| c.dropped).sum();
    }

    #[test]
    fn profile_declared_passes_when_the_recv_report_names_the_declared_profile() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "h265-klv", None);
        inputs.legs[0].1.recv_report.profile = Some("h265-klv".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("h265-klv"), "{}", v.detail);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    /// The hole this verdict exists for: a run that exercised one wire
    /// shape while its own declared evidence claims another.
    #[test]
    fn profile_declared_fails_when_recv_judged_a_different_profile() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "h265-klv", None);
        inputs.legs[0].1.recv_report.profile = Some("baseline".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_srt");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains("h265-klv") && v.detail.contains("baseline"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    /// A declared profile with a recv report that carries none is a
    /// harness mismatch, not a silent pass.
    #[test]
    fn profile_declared_fails_when_the_recv_report_carries_no_profile() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        inputs.legs[0].1.recv_report.profile = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("no profile"), "{}", v.detail);
    }

    /// An archived config predating the declaration passes, saying so.
    #[test]
    fn profile_declared_passes_undeclared_with_a_named_detail() {
        let inputs = healthy_inputs(); // no `legs` entries at all
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("no declaration"), "{}", v.detail);
    }

    #[test]
    fn schedule_declared_passes_when_the_proxy_echo_matches() {
        let mut inputs = healthy_inputs();
        declare_srt(
            &mut inputs,
            "baseline",
            Some(ScheduleDeclaration {
                seed: 3,
                phases: 4,
                phase_s: 153,
            }),
        );
        inputs.legs[0].1.recv_report.profile = Some("baseline".to_string());
        echo_srt_schedule(&mut inputs, 3, 4, 153);
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "schedule_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    #[test]
    fn schedule_declared_fails_on_any_differing_field() {
        for (seed, phases, phase_s) in [(4u64, 4u32, 153u64), (3, 5, 153), (3, 4, 200)] {
            let mut inputs = healthy_inputs();
            declare_srt(
                &mut inputs,
                "baseline",
                Some(ScheduleDeclaration {
                    seed: 3,
                    phases: 4,
                    phase_s: 153,
                }),
            );
            // Stamped so `profile_declared_srt` passes and this run's
            // only failing verdict is the schedule one.
            inputs.legs[0].1.recv_report.profile = Some("baseline".to_string());
            echo_srt_schedule(&mut inputs, seed, phases, phase_s);
            let r = build_soak_results(inputs).unwrap();
            let v = verdict(&r, "schedule_declared_srt");
            assert!(!v.pass, "echo {seed}/{phases}/{phase_s}: {}", v.detail);
            assert!(!r.overall_pass);
            assert_eq!(
                r.verdicts
                    .iter()
                    .filter(|v| !v.provisional && !v.pass)
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["schedule_declared_srt"],
                "only the schedule verdict may fail here"
            );
        }
    }

    /// Both one-sided cases: a declared schedule the proxy never ran,
    /// and a schedule the proxy ran that nothing declared.
    #[test]
    fn schedule_declared_fails_when_only_one_side_has_a_schedule() {
        let mut declared_only = healthy_inputs();
        declare_srt(
            &mut declared_only,
            "baseline",
            Some(ScheduleDeclaration {
                seed: 3,
                phases: 4,
                phase_s: 153,
            }),
        );
        declared_only.legs[0].1.recv_report.profile = Some("baseline".to_string());
        let r = build_soak_results(declared_only).unwrap();
        let v = verdict(&r, "schedule_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("fixed-impairment"), "{}", v.detail);

        let mut echoed_only = healthy_inputs();
        echo_srt_schedule(&mut echoed_only, 3, 4, 153);
        let r = build_soak_results(echoed_only).unwrap();
        let v = verdict(&r, "schedule_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("never declared"), "{}", v.detail);
    }

    /// Neither side has one: the fixed-impairment run, which passes.
    #[test]
    fn schedule_declared_passes_when_neither_side_has_a_schedule() {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        inputs.legs[0].1.recv_report.profile = Some("baseline".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "schedule_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    /// Declare the `srt` leg's reconnect mode and say what its send
    /// report observed, on an otherwise [`healthy_inputs`] run.
    /// `observed: None` is a managed send report that carries the
    /// counters but no mode — the shape written before the mode was
    /// recorded.
    fn reconnect_mode_inputs(declared: Option<&str>, observed: Option<&str>) -> SoakInputs {
        let mut inputs = healthy_inputs();
        declare_srt(&mut inputs, "baseline", None);
        inputs.legs[0].1.recv_report.profile = Some("baseline".to_string());
        inputs
            .config
            .legs
            .get_mut("srt")
            .expect("declared above")
            .reconnect_mode = declared.map(str::to_string);
        inputs.legs[0].1.send_metrics.managed_send = Some(ManagedSendStats {
            reconnect_attempts: 3,
            reconnect_successes: 2,
            gap_messages_dropped: 41,
            gap_bytes_dropped: 53_956,
            gap_buffer_capacity: 256,
            gap_len_at_exit: 5,
            reconnect_mode: observed.map(str::to_string),
            overflow_policy: Some("drop_oldest".to_string()),
        });
        inputs
    }

    #[test]
    fn reconnect_mode_declared_passes_when_the_sender_ran_the_declared_mode() {
        for mode in ["blocking", "background"] {
            let r = build_soak_results(reconnect_mode_inputs(Some(mode), Some(mode))).unwrap();
            let v = verdict(&r, "reconnect_mode_declared_srt");
            assert!(v.pass && !v.provisional, "{}", v.detail);
            assert!(v.detail.contains(mode), "{}", v.detail);
            assert!(r.overall_pass, "{:?}", r.verdicts);
        }
    }

    /// The hole this verdict exists for: a run launched in one mode
    /// whose evidence says it exercised the other.
    #[test]
    fn reconnect_mode_declared_fails_when_the_sender_ran_the_other_mode() {
        let r = build_soak_results(reconnect_mode_inputs(Some("background"), Some("blocking")))
            .unwrap();
        let v = verdict(&r, "reconnect_mode_declared_srt");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains("background") && v.detail.contains("blocking"),
            "{}",
            v.detail
        );
        assert_eq!(
            r.verdicts
                .iter()
                .filter(|v| !v.provisional && !v.pass)
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>(),
            vec!["reconnect_mode_declared_srt"],
            "only the reconnect-mode verdict may fail here"
        );
        assert!(!r.overall_pass);
    }

    /// Declared, but nothing in the send report to check it against:
    /// once with a managed block that carries no mode, once with no
    /// managed block at all (the sender was never launched
    /// `--managed`). Neither is a silent pass.
    #[test]
    fn reconnect_mode_declared_fails_when_the_send_report_carries_no_mode() {
        let r = build_soak_results(reconnect_mode_inputs(Some("background"), None)).unwrap();
        let v = verdict(&r, "reconnect_mode_declared_srt");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("no reconnect mode"), "{}", v.detail);
        assert!(!r.overall_pass);

        let mut unmanaged = reconnect_mode_inputs(Some("background"), None);
        unmanaged.legs[0].1.send_metrics.managed_send = None;
        let r = build_soak_results(unmanaged).unwrap();
        let v = verdict(&r, "reconnect_mode_declared_srt");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("no managed_send"), "{}", v.detail);
    }

    /// A config that declares no mode — a leg whose sender is not
    /// managed, or an archive written before the declaration existed
    /// — is not applicable, which this report spells as a passing
    /// verdict that says so. With and without a leg entry.
    #[test]
    fn reconnect_mode_declared_is_not_applicable_when_nothing_was_declared() {
        let r = build_soak_results(reconnect_mode_inputs(None, None)).unwrap();
        let v = verdict(&r, "reconnect_mode_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("not applicable"), "{}", v.detail);
        assert!(r.overall_pass, "{:?}", r.verdicts);

        let r = build_soak_results(healthy_inputs()).unwrap(); // no `legs` entries
        let v = verdict(&r, "reconnect_mode_declared_srt");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("not applicable"), "{}", v.detail);
    }

    /// The counters ride along into the leg's results beside its
    /// access-unit totals, and gate nothing: a leg that evicted from
    /// its gap buffer still passes.
    #[test]
    fn managed_send_counters_are_recorded_on_the_leg_and_never_gate() {
        let r = build_soak_results(reconnect_mode_inputs(
            Some("background"),
            Some("background"),
        ))
        .unwrap();
        let leg = &r.legs[0];
        assert_eq!((leg.send_video_aus, leg.recv_video_aus), (1000, 1000));
        let managed = leg.managed_send.as_ref().expect("recorded on the leg");
        assert_eq!(
            (
                managed.gap_messages_dropped,
                managed.gap_bytes_dropped,
                managed.gap_len_at_exit
            ),
            (41, 53_956, 5)
        );
        assert!(r.overall_pass, "{:?}", r.verdicts);
        assert!(
            r.limitations
                .iter()
                .any(|l| l.contains("gap_messages_dropped") && l.contains("recorded")),
            "the report must say what the gap counters do not cover: {:?}",
            r.limitations
        );
        assert!(
            r.limitations
                .iter()
                .any(|l| l.contains("gap_len_at_exit") && l.contains("not gated")),
            "the report must name the stranded backlog beside the drop counters: {:?}",
            r.limitations
        );

        // A leg with no managed sender records none.
        let r = build_soak_results(healthy_inputs()).unwrap();
        assert!(r.legs[0].managed_send.is_none());
    }

    /// A `soak-config.json` and a `soak-results.json` leg exactly as
    /// written before the reconnect mode was declared or the managed
    /// counters were carried onto the leg: both must still load.
    #[test]
    fn archives_written_before_the_reconnect_mode_still_load() {
        let cfg = parse_soak_config(
            r#"{"expected_duration_s": 259200, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}, "corruption": true,
                "corruption_spec": {"rate_per_10k": 5, "min_gap": 1000, "classes": ["drop"]},
                "legs": {"srt": {"profile": "misp",
                                 "schedule": {"seed": 11, "phases": 12, "phase_s": 21600},
                                 "klv_set": "rich", "klv_seed": 11},
                         "rist": {"profile": "audio",
                                  "schedule": {"seed": 11, "phases": 12, "phase_s": 21600},
                                  "klv_set": "rich", "klv_seed": 11}}}"#,
        )
        .expect("an archived config must parse");
        assert!(cfg.legs["srt"].reconnect_mode.is_none());
        assert!(cfg.legs["rist"].reconnect_mode.is_none());

        let leg: LegResult = serde_json::from_str(
            r#"{"leg": "srt", "outage_period_s": 21600, "outage_dur_s": 90,
                "expected_outage_windows": 12, "proxy_forwarded": 10, "proxy_dropped": 1,
                "loss_pct": 2.0, "observed_drop_fraction": 0.02,
                "expected_drop_fraction": 0.02, "drop_fraction_tolerance": 0.01,
                "phases": 12, "recv_pass": true, "recv_failures": [],
                "send_video_aus": 100, "recv_video_aus": 98,
                "corruption_injected": 3}"#,
        )
        .expect("an archived leg result must parse");
        assert!(leg.managed_send.is_none());
    }

    /// Both modes validate, `null` validates (a leg with no managed
    /// sender), and anything else is a typo caught before launch.
    #[test]
    fn soak_config_rejects_an_unknown_declared_reconnect_mode() {
        let config = |mode: &str| {
            format!(
                r#"{{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                    "sampler_end_slack_s": 35,
                    "legs": {{"srt": {{"profile": "baseline", "reconnect_mode": {mode}}}}}}}"#
            )
        };
        for (mode, want) in [
            (r#""blocking""#, Some("blocking")),
            (r#""background""#, Some("background")),
            ("null", None),
        ] {
            let cfg = parse_soak_config(&config(mode))
                .unwrap_or_else(|e| panic!("reconnect_mode {mode} must validate: {e}"));
            assert_eq!(cfg.legs["srt"].reconnect_mode.as_deref(), want);
        }
        for mode in [r#""backgroud""#, r#""Blocking""#, r#""""#] {
            let e = parse_soak_config(&config(mode)).unwrap_err();
            assert!(
                e.contains("legs.srt.reconnect_mode") && e.contains("blocking"),
                "{mode}: {e}"
            );
        }
    }

    /// A declaration naming a profile that doesn't exist can never
    /// match any recv report, so it is a config typo — caught at parse
    /// time, before a multi-day run launches against it.
    #[test]
    fn soak_config_rejects_an_unknown_declared_profile_or_leg() {
        let e = parse_soak_config(
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "legs": {"srt": {"profile": "nope"}}}"#,
        )
        .unwrap_err();
        assert!(e.contains("nope") && e.contains("legs.srt.profile"), "{e}");

        let e = parse_soak_config(
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1,
                "sampler_end_slack_s": 35, "legs": {"quic": {"profile": "baseline"}}}"#,
        )
        .unwrap_err();
        assert!(e.contains("quic"), "{e}");

        // The real shape soak.sh writes must validate.
        let cfg = parse_soak_config(
            r#"{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                "sampler_end_slack_s": 35, "expected_worker_exits": {}, "corruption": true,
                "legs": {"srt": {"profile": "misp", "schedule": {"seed": 3, "phases": 4, "phase_s": 900}},
                         "rist": {"profile": "audio", "schedule": null}}}"#,
        )
        .expect("soak.sh's own config shape must validate");
        assert_eq!(cfg.legs["srt"].profile, "misp");
        assert_eq!(
            cfg.legs["srt"].schedule.as_ref().unwrap().phases,
            4,
            "the declared schedule must survive the round trip"
        );
        assert!(cfg.legs["rist"].schedule.is_none());
    }

    /// The `rtsp-publish` leg's declaration as `soak.sh` writes it.
    fn publish_declaration(generations: Option<u64>) -> LegDeclaration {
        publish_declaration_as(generations, false)
    }

    /// KLV records in one generation of the `rtsp-publish-es` source.
    const ES_RECORDS_PER_GENERATION: u64 = 600;

    /// A publish leg's declaration as `soak.sh` writes it: the MP2T
    /// `rtsp-publish` leg, or (`es`) the elementary `rtsp-publish-es`
    /// leg, whose mount re-muxes and so declares a KLV count and the
    /// placement oracle instead of a source digest.
    fn publish_declaration_as(generations: Option<u64>, es: bool) -> LegDeclaration {
        LegDeclaration {
            profile: if es { "klv-sync" } else { "baseline" }.to_string(),
            schedule: None,
            klv_set: None,
            klv_seed: None,
            reconnect_mode: None,
            publisher: Some("gst-launch-1.0 filesrc ! tsparse ! rtspclientsink".to_string()),
            publisher_drop_period_s: Some(600),
            publisher_generations: generations,
            source_sha256: (!es).then(|| SOURCE_SHA.to_string()),
            remuxed: es.then_some(true),
            klv_records_per_generation: es.then_some(ES_RECORDS_PER_GENERATION),
            klv_placement: es.then_some(true),
            mount_kind: None,
            viewer: None,
            viewer_reconnect_s: None,
            viewer_connections: None,
        }
    }

    /// The digest every `publish_inputs` recv report carries, and the
    /// one its declaration names.
    const SOURCE_SHA: &str = "5f1c0e8a6b3d2f4e9a7c1b0d8e6f4a2c3b5d7e9f1a0c2e4b6d8f0a1c3e5b7d9f";

    /// [`healthy_inputs`] plus a healthy `rtsp-publish` leg: RSS for
    /// its two processes, clean exits for its two roles, a declared
    /// generation count, and a passing recv report whose mount counted
    /// `generation` ended publishers (`None` = no `publish_mount`
    /// block at all).
    fn publish_inputs(generation: Option<u64>, launched: Option<u64>) -> SoakInputs {
        let mut inputs = healthy_inputs();
        add_publish_leg(&mut inputs, "rtsp-publish", generation, launched);
        inputs
    }

    /// [`healthy_inputs`] plus a healthy `rtsp-publish-es` leg: the
    /// elementary publisher's mount counted `generation` ended
    /// publishers, and the receiver got every one of the
    /// `ES_RECORDS_PER_GENERATION x generation` KLV records, each in
    /// place.
    fn es_inputs(generation: u64) -> SoakInputs {
        let mut inputs = healthy_inputs();
        add_publish_leg(
            &mut inputs,
            "rtsp-publish-es",
            Some(generation),
            Some(generation),
        );
        inputs
    }

    /// The healthy publish leg `leg` (`"rtsp-publish"` or
    /// `"rtsp-publish-es"`) added to `inputs`: RSS for its two
    /// processes, clean exits for its two roles, its declaration, and
    /// a passing recv report.
    fn add_publish_leg(
        inputs: &mut SoakInputs,
        leg: &str,
        generation: Option<u64>,
        launched: Option<u64>,
    ) {
        let es = leg == "rtsp-publish-es";
        for i in 0..121 {
            let t = i as f64 * 30.0;
            inputs
                .rss_samples
                .push(rss_row(leg, "recv", t, Some(40_000)));
            inputs
                .rss_samples
                .push(rss_row(leg, "publisher", t, Some(4_000)));
        }
        for process in ["recv", "publisher"] {
            inputs.worker_exits.insert(format!("{leg}-{process}"), 0);
        }
        inputs
            .config
            .legs
            .insert(leg.to_string(), publish_declaration_as(launched, es));
        let mut recv_report = passing_recv_report(1000);
        if es {
            let generations = generation.unwrap_or(0);
            let records = ES_RECORDS_PER_GENERATION * generations;
            recv_report.profile = Some("klv-sync".to_string());
            recv_report.metrics.klv_records = records;
            recv_report.metrics.klv_placement = Some(KlvPlacementMetrics {
                records,
                out_of_tolerance: 0,
                max_abs_error_ticks: 90,
                generations,
                unexpected_resets: 0,
                tolerance_ticks: 3003,
                first_problem: None,
                // Video and KLV reset at every boundary, with the PCR jump
                // and the two PTS anomalies the demuxer raises there.
                generation_resets_excused: 2 * generations.saturating_sub(1),
                timing_anomalies_excused: 3 * generations.saturating_sub(1),
            });
            recv_report.publish_mount = generation.map(|generation| PublishMountReport {
                generation,
                aus_emitted: 1,
                klv_units_dropped: 0,
                ..PublishMountReport::default()
            });
        } else {
            recv_report.profile = Some("baseline".to_string());
            recv_report.metrics.stream_sha256 = SOURCE_SHA.to_string();
            recv_report.publish_mount = generation.map(|generation| PublishMountReport {
                generation,
                ..PublishMountReport::default()
            });
        }
        inputs.publish_legs.push((
            leg.to_string(),
            PublishLegArtifacts {
                recv_report,
                source_sha256_file: None,
            },
        ));
    }

    /// Six generations declared: a floor of all six. A mount that
    /// counted exactly the floor passes, and the leg's
    /// not-applicable verdicts pass with their reason, so the whole
    /// run passes.
    #[test]
    fn publisher_generations_passes_at_the_floor() {
        let r = build_soak_results(publish_inputs(Some(6), Some(6))).unwrap();
        let v = verdict(&r, "publisher_generations_rtsp-publish");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("floor of 6"), "{}", v.detail);
        assert!(v.detail.contains("worker_exits"), "{}", v.detail);
        for name in [
            "reconnect_mode_declared_rtsp-publish",
            "schedule_declared_rtsp-publish",
            "corruption_attributed_rtsp-publish",
        ] {
            let v = verdict(&r, name);
            assert!(
                v.pass && v.detail.contains("not applicable"),
                "{name}: {}",
                v.detail
            );
        }
        assert!(verdict(&r, "rss_sample_coverage_rtsp-publish_publisher").pass);
        assert!(verdict(&r, "worker_exits").pass);
        assert_eq!(r.publish_legs.len(), 1);
        assert_eq!(r.publish_legs[0].min_generations, Some(6));
        let v = verdict(&r, "delivery_complete_rtsp-publish");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains("declared in soak-config.json"),
            "{}",
            v.detail
        );
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    /// The `rtsp-publish-es` leg's three own numbers.
    fn es_mut(inputs: &mut SoakInputs) -> &mut VerifyReport {
        &mut inputs
            .publish_legs
            .iter_mut()
            .find(|(leg, _)| leg == "rtsp-publish-es")
            .expect("es_inputs adds the leg")
            .1
            .recv_report
    }

    /// Every verdict the elementary leg emits passes when the KLV
    /// count, the mount's counters and the placement block agree with
    /// the declaration — the digest oracle never runs on it.
    #[test]
    fn es_leg_passes_when_counts_match_and_placement_is_clean() {
        let r = build_soak_results(es_inputs(6)).unwrap();
        let es: Vec<_> = r
            .verdicts
            .iter()
            .filter(|v| v.name.ends_with("_rtsp-publish-es"))
            .collect();
        // recv_invariants, profile/klv/schedule/reconnect_mode_declared,
        // the six corruption verdicts, publisher_generations,
        // delivery_complete and klv_placement.
        assert_eq!(es.len(), 14, "{es:?}");
        for v in &es {
            assert!(v.pass, "{}: {}", v.name, v.detail);
        }
        let v = verdict(&r, "delivery_complete_rtsp-publish-es");
        assert!(v.detail.contains("3600"), "{}", v.detail);
        let v = verdict(&r, "klv_placement_rtsp-publish-es");
        assert!(!v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains(
                "the generation budget excused 10 backward PTS step(s) and 15 timing anomaly event(s)"
            ),
            "{}",
            v.detail
        );
        assert!(verdict(&r, "worker_exits").pass);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    #[test]
    fn es_leg_delivery_fails_on_a_short_klv_count() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs).metrics.klv_records = ES_RECORDS_PER_GENERATION * 6 - 1;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish-es");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("3599"), "{}", v.detail);
        assert!(v.detail.contains("3600"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    #[test]
    fn es_leg_delivery_fails_on_a_dropped_klv_unit() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs)
            .publish_mount
            .as_mut()
            .unwrap()
            .klv_units_dropped = 1;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish-es");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("klv_units_dropped 1"), "{}", v.detail);
        assert!(!r.overall_pass);

        // A mount that emitted no access unit delivered nothing either.
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs)
            .publish_mount
            .as_mut()
            .unwrap()
            .aus_emitted = 0;
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "delivery_complete_rtsp-publish-es").pass);
    }

    #[test]
    fn es_leg_placement_fails_on_out_of_tolerance() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs)
            .metrics
            .klv_placement
            .as_mut()
            .unwrap()
            .out_of_tolerance = 2;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_placement_rtsp-publish-es");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("2 out of tolerance"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    #[test]
    fn es_leg_placement_fails_without_the_block() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs).metrics.klv_placement = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_placement_rtsp-publish-es");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("--klv-placement"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    #[test]
    fn es_leg_placement_fails_when_generations_disagree() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs)
            .metrics
            .klv_placement
            .as_mut()
            .unwrap()
            .generations = 5;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_placement_rtsp-publish-es");
        assert!(!v.pass, "{}", v.detail);
        assert!(!r.overall_pass);

        // An unexpected reset fails it as well.
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs)
            .metrics
            .klv_placement
            .as_mut()
            .unwrap()
            .unexpected_resets = 1;
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "klv_placement_rtsp-publish-es").pass);
    }

    /// A remuxed leg's bytes change in flight, so a source digest
    /// would be a promise the leg cannot keep, and without a per-
    /// generation count there is nothing to judge delivery against.
    /// The three fields mean nothing on a transport leg.
    #[test]
    fn remuxed_declaration_rejects_a_source_digest_and_needs_a_count() {
        let ok = publish_declaration_as(Some(6), true);
        parse_soak_config(&config_with_publish_leg("rtsp-publish-es", ok.clone()))
            .expect("a soak.sh-shaped elementary leg must validate");

        let mut with_digest = ok.clone();
        with_digest.source_sha256 = Some(SOURCE_SHA.to_string());
        let err = parse_soak_config(&config_with_publish_leg("rtsp-publish-es", with_digest))
            .expect_err("a remuxed leg with a source digest must be rejected");
        assert!(err.contains("source_sha256"), "{err}");

        for placement in [None, Some(false)] {
            let mut no_placement = ok.clone();
            no_placement.klv_placement = placement;
            let err = parse_soak_config(&config_with_publish_leg("rtsp-publish-es", no_placement))
                .expect_err("the elementary leg must declare the placement oracle");
            assert!(err.contains("klv_placement"), "{err}");
        }

        for count in [None, Some(0)] {
            let mut no_count = ok.clone();
            no_count.klv_records_per_generation = count;
            let err = parse_soak_config(&config_with_publish_leg("rtsp-publish-es", no_count))
                .expect_err("a remuxed leg without a KLV count must be rejected");
            assert!(err.contains("klv_records_per_generation"), "{err}");
        }

        let transport = |edit: fn(&mut LegDeclaration)| {
            let mut decl = publish_declaration(None);
            decl.publisher = None;
            decl.publisher_drop_period_s = None;
            decl.source_sha256 = None;
            edit(&mut decl);
            parse_soak_config(&config_with_publish_leg("srt", decl))
        };
        assert!(transport(|_| {}).is_ok());
        for edit in [
            (|d: &mut LegDeclaration| d.remuxed = Some(true)) as fn(&mut LegDeclaration),
            |d| d.klv_records_per_generation = Some(600),
            |d| d.klv_placement = Some(true),
        ] {
            let err = transport(edit).expect_err("a transport leg cannot declare these");
            assert!(err.contains("only a publish leg"), "{err}");
        }
    }

    /// A KLV count is what a remuxed leg's delivery is judged
    /// against; declared on a leg that is not remuxed it would be
    /// silently ignored, so it is rejected. The placement oracle does
    /// not depend on the re-mux and stays allowed.
    #[test]
    fn a_klv_count_without_remuxed_is_rejected() {
        let mut decl = publish_declaration(Some(6));
        decl.klv_records_per_generation = Some(600);
        let err = parse_soak_config(&config_with_publish_leg("rtsp-publish", decl))
            .expect_err("a KLV count on a leg that is not remuxed must be rejected");
        assert!(err.contains("klv_records_per_generation"), "{err}");
        assert!(err.contains("remuxed"), "{err}");

        let mut decl = publish_declaration(Some(6));
        decl.klv_placement = Some(true);
        assert!(parse_soak_config(&config_with_publish_leg("rtsp-publish", decl)).is_ok());
    }

    #[test]
    fn es_leg_delivery_fails_without_a_publish_mount_block() {
        let mut inputs = es_inputs(6);
        es_mut(&mut inputs).publish_mount = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish-es");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("no publish_mount block"), "{}", v.detail);
    }

    #[test]
    fn es_leg_delivery_fails_without_declared_generations() {
        let mut inputs = es_inputs(6);
        inputs
            .config
            .legs
            .get_mut("rtsp-publish-es")
            .unwrap()
            .publisher_generations = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish-es");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("publisher_generations"), "{}", v.detail);
    }

    #[test]
    fn es_leg_placement_fails_without_declared_generations() {
        let mut inputs = es_inputs(6);
        inputs
            .config
            .legs
            .get_mut("rtsp-publish-es")
            .unwrap()
            .publisher_generations = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "klv_placement_rtsp-publish-es");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("against none declared"), "{}", v.detail);
    }

    /// The MP2T leg declares no placement oracle: its verdict is
    /// emitted and passes, saying why.
    #[test]
    fn mp2t_leg_placement_verdict_is_not_applicable() {
        let r = build_soak_results(publish_inputs(Some(6), Some(6))).unwrap();
        let v = verdict(&r, "klv_placement_rtsp-publish");
        assert!(v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("not applicable"), "{}", v.detail);
        // The digest oracle still judges it.
        assert!(
            verdict(&r, "delivery_complete_rtsp-publish")
                .detail
                .contains("stream_sha256")
        );
    }

    #[test]
    fn memory_gate_is_validated_and_recorded() {
        let with_gate = |gate: serde_json::Value| {
            let mut v: serde_json::Value = serde_json::from_str(&config_with_publish_leg(
                "rtsp-publish",
                publish_declaration(Some(6)),
            ))
            .unwrap();
            v["memory_gate"] = gate;
            parse_soak_config(&v.to_string())
        };
        let err = with_gate(serde_json::json!({
            "predicted_kb": 2_000_000, "budget_kb": 1_000_000,
            "headroom": 1.5, "mem_total_kb": 4_000_000
        }))
        .expect_err("a prediction over the budget must be rejected");
        assert!(err.contains("memory_gate"), "{err}");
        let err = with_gate(serde_json::json!({
            "predicted_kb": 500_000, "budget_kb": 1_000_000,
            "headroom": 0.9, "mem_total_kb": 4_000_000
        }))
        .expect_err("a headroom under 1 must be rejected");
        assert!(err.contains("headroom"), "{err}");

        // Inside the budget on its own, over it once the headroom is applied.
        let err = with_gate(serde_json::json!({
            "predicted_kb": 800_000, "budget_kb": 1_000_000,
            "headroom": 1.5, "mem_total_kb": 4_000_000
        }))
        .expect_err("a prediction over the budget after headroom must be rejected");
        assert!(err.contains("predicted_kb"), "{err}");
        assert!(err.contains("headroom"), "{err}");

        let cfg = with_gate(serde_json::json!({
            "predicted_kb": 512_000, "budget_kb": 1_024_000,
            "headroom": 1.5, "mem_total_kb": 4_000_000
        }))
        .expect("a gate inside its budget must validate");
        let gate = cfg.memory_gate.clone().expect("the block must parse");
        assert_eq!(gate.predicted_kb, 512_000);

        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs.config.memory_gate = Some(gate);
        let r = build_soak_results(inputs).unwrap();
        let line = r
            .limitations
            .iter()
            .find(|l| l.starts_with("memory gate:"))
            .unwrap_or_else(|| panic!("{:?}", r.limitations));
        assert!(line.contains("500 MB"), "{line}");
        assert!(line.contains("1000 MB"), "{line}");
        assert!(line.contains("1.5"), "{line}");
        assert!(r.overall_pass, "{:?}", r.verdicts);

        // No block: no line, and an older config still parses.
        let r = build_soak_results(publish_inputs(Some(6), Some(6))).unwrap();
        assert!(!r.limitations.iter().any(|l| l.starts_with("memory gate:")));
    }

    #[test]
    fn worker_roles_cover_both_publish_legs() {
        assert!(WORKER_ROLES.contains(&"rtsp-publish-es-recv"));
        assert!(WORKER_ROLES.contains(&"rtsp-publish-es-publisher"));
        assert!(PUBLISH_LEGS.contains(&"rtsp-publish-es"));
        assert!(KNOWN_LEGS.contains(&"rtsp-publish-es"));
    }

    #[test]
    fn worker_roles_cover_the_whep_leg() {
        assert!(WORKER_ROLES.contains(&"whep-serve"));
        assert!(WORKER_ROLES.contains(&"whep-viewer"));
        assert!(KNOWN_LEGS.contains(&"whep"));
        assert!(WHEP_LEGS.contains(&"whep"));
        assert_eq!(leg_processes("whep"), &["serve", "viewer"]);
    }

    /// The whep leg's declaration as `soak.sh` writes it.
    fn whep_declaration(connections: Option<u64>) -> LegDeclaration {
        LegDeclaration {
            profile: "baseline".to_string(),
            schedule: None,
            klv_set: Some("compact".to_string()),
            klv_seed: None,
            reconnect_mode: None,
            publisher: None,
            publisher_drop_period_s: None,
            publisher_generations: None,
            source_sha256: None,
            remuxed: None,
            klv_records_per_generation: None,
            klv_placement: None,
            mount_kind: Some("mp2t".to_string()),
            viewer: Some("google-chrome --headless=new ...".to_string()),
            viewer_reconnect_s: Some(600),
            viewer_connections: connections,
        }
    }

    /// One healthy connection of `seconds` samples: frames rise 30 a
    /// second from the first sample, both carriages saw the same 10 Hz
    /// record set, ended by the loop's timeout.
    fn whep_connection(index: u64, seconds: u64) -> WhepConnection {
        WhepConnection {
            index,
            started_utc: String::new(),
            browser_rc: WHEP_BROWSER_TIMED_OUT,
            frames: (1..=seconds).map(|i| i * 30).collect(),
            audio: Vec::new(),
            klv_sei: seconds * 10,
            klv_dc: seconds * 10,
            klv_sei_set_sha256: "a".repeat(64),
            klv_dc_set_sha256: "a".repeat(64),
            klv_unpaired: Vec::new(),
            klv_copies_disagreeing: 0,
            page_errors: 0,
            console_errors: 0,
            first_error: None,
        }
    }

    /// [`healthy_inputs`] plus a healthy whep leg: RSS for its two
    /// processes, clean exits for its two roles, a declaration of
    /// `declared` connections and a viewer report of `connections`
    /// connections the server also counted.
    fn whep_inputs(connections: u64, declared: Option<u64>) -> SoakInputs {
        let mut inputs = healthy_inputs();
        for i in 0..121 {
            let t = i as f64 * 30.0;
            inputs
                .rss_samples
                .push(rss_row("whep", "serve", t, Some(60_000)));
            inputs
                .rss_samples
                .push(rss_row("whep", "viewer", t, Some(4_000)));
        }
        for process in ["serve", "viewer"] {
            inputs.worker_exits.insert(format!("whep-{process}"), 0);
        }
        inputs
            .config
            .legs
            .insert("whep".to_string(), whep_declaration(declared));
        let viewer = WhepViewerReport {
            reconnect_s: 600,
            connection_s: 597,
            connections: (1..=connections).map(|k| whep_connection(k, 596)).collect(),
        };
        // An hour served at 30 frames/s and 10 records/s: the rates the
        // volume floors hold each connection to.
        let serve = WhepServeReport {
            profile: Some("baseline".to_string()),
            mount_kind: Some("mp2t".to_string()),
            seconds: 3600.0,
            viewer_connected_before_push: true,
            mount: WhepMountCounters {
                frames_pushed: 108_000,
                klv_records_pushed: 36_000,
                ..Default::default()
            },
            server: WhepServerCounters {
                sessions_total: connections,
                ..Default::default()
            },
        };
        inputs
            .whep_legs
            .push(("whep".to_string(), WhepLegArtifacts { viewer, serve }));
        inputs
    }

    #[test]
    fn whep_leg_passes_when_every_connection_plays_and_the_counts_agree() {
        let r = build_soak_results(whep_inputs(6, Some(6))).unwrap();
        for name in [
            "whep_frames_continuous_whep",
            "whep_klv_sei_equals_dc_whep",
            "whep_reconnects_observed_whep",
            "profile_declared_whep",
            "rss_sample_coverage_whep_serve",
            "rss_sample_coverage_whep_viewer",
        ] {
            let v = verdict(&r, name);
            assert!(v.pass && !v.provisional, "{name}: {}", v.detail);
        }
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(
            v.detail.contains("5 reconnect(s) against 5"),
            "{}",
            v.detail
        );
        // The floors show their arithmetic: 0.9 x 30 x (596 - 5) = 15957
        // frames and 0.9 x 10 x 591 = 5319 records.
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(
            v.detail.contains("17880 frames against a floor of 15957"),
            "{}",
            v.detail
        );
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(
            v.detail.contains("5960 records against a floor of 5319"),
            "{}",
            v.detail
        );
        for name in [
            "schedule_declared_whep",
            "reconnect_mode_declared_whep",
            "corruption_attributed_whep",
        ] {
            let v = verdict(&r, name);
            assert!(
                v.pass && v.detail.contains("not applicable"),
                "{name}: {}",
                v.detail
            );
        }
        assert_eq!(r.whep_legs.len(), 1);
        assert_eq!(r.whep_legs[0].connections_observed, 6);
        assert_eq!(r.whep_legs[0].frames_decoded, 6 * 596 * 30);
        assert_eq!(r.whep_legs[0].klv_sei_records, 6 * 5960);
        assert!(verdict(&r, "worker_exits").pass);
        assert!(r.overall_pass, "{:?}", r.verdicts);
    }

    #[test]
    fn whep_frames_continuous_fails_on_a_stall_over_two_seconds() {
        let mut inputs = whep_inputs(3, Some(3));
        // Two samples without a rise are a 2 s gap and pass.
        let frames = &mut inputs.whep_legs[0].1.viewer.connections[1].frames;
        frames[100] = frames[99];
        frames[101] = frames[99];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("longest stall 2 s"), "{}", v.detail);

        // A third is over 2 s and fails, naming the connection.
        let mut inputs = whep_inputs(3, Some(3));
        let frames = &mut inputs.whep_legs[0].1.viewer.connections[1].frames;
        frames[100] = frames[99];
        frames[101] = frames[99];
        frames[102] = frames[99];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail
                .contains("connection 2: FRAMES stalled at 3000 for over 2 s from sample 101"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    #[test]
    fn whep_frames_continuous_fails_a_late_first_frame_a_crash_or_an_error() {
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[0];
        for f in c.frames.iter_mut().take(WHEP_FIRST_FRAME_SAMPLES) {
            *f = 0;
        }
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail
                .contains("connection 1: no decoded frame within the first 5"),
            "{}",
            v.detail
        );

        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.browser_rc = 1;
        c.frames.truncate(100);
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(
            v.detail.contains("connection 2: browser exit 1"),
            "{}",
            v.detail
        );
        assert!(
            v.detail
                .contains("connection 2: only 100 FRAMES sample(s) over a planned 597 s"),
            "{}",
            v.detail
        );

        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[0];
        c.page_errors = 1;
        c.first_error = Some("ERROR whep: 503".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("first: ERROR whep: 503"), "{}", v.detail);
    }

    #[test]
    fn whep_klv_sei_equals_dc_allows_unpaired_records_at_the_edges_only() {
        let unpaired = |via: &str, from_head: u64, from_tail: u64| WhepUnpairedRecord {
            via: via.to_string(),
            from_head,
            from_tail,
        };
        // Two SEI-only records right after the data channel opened and
        // one data-channel-only record at the kill: all at the edges.
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.klv_dc = c.klv_sei - 1;
        c.klv_dc_set_sha256 = "b".repeat(64);
        c.klv_unpaired = vec![
            unpaired("sei", 0, 5959),
            unpaired("sei", 2, 5957),
            unpaired("dc", 5958, 0),
        ];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("3 unpaired only at"), "{}", v.detail);
        assert_eq!(r.whep_legs[0].klv_unpaired_at_edges, 3);

        // One mid-connection record on the SEI only: a real loss.
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.klv_dc = c.klv_sei - 1;
        c.klv_dc_set_sha256 = "b".repeat(64);
        c.klv_unpaired = vec![unpaired("sei", 3, 5956)];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail
                .contains("connection 2: a record via sei (3 record(s) before it, 5956 after)"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);

        // The tail allowance is exactly WHEP_KLV_EDGE_TAIL records: the
        // second-to-last record passes, the one before it fails.
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.klv_sei = c.klv_dc - 1;
        c.klv_sei_set_sha256 = "b".repeat(64);
        c.klv_unpaired = vec![unpaired("dc", 5958, 1)];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(v.pass, "{}", v.detail);
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.klv_sei = c.klv_dc - 1;
        c.klv_sei_set_sha256 = "b".repeat(64);
        c.klv_unpaired = vec![unpaired("dc", 5957, 2)];
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail
                .contains("a record via dc (5957 record(s) before it, 2 after)"),
            "{}",
            v.detail
        );

        // Differing sets with nothing to locate the difference fail too.
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[0];
        c.klv_dc_set_sha256 = "b".repeat(64);
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("lists no unpaired record"),
            "{}",
            v.detail
        );
    }

    /// A connection that ticks at 1 frame/s, or that saw a tenth of the
    /// served records on both carriages, fails on volume; the floor is
    /// read from the serve report, so a report with no window fails too.
    #[test]
    fn whep_content_verdicts_hold_each_connection_to_the_served_rate() {
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[1];
        c.frames = (1..=596).collect();
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_frames_continuous_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains(
                "connection 2: 596 frames decoded over 596 sample(s), below the floor of 15957"
            ),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);

        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[0];
        c.klv_sei = 596;
        c.klv_dc = 596;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains(
                "connection 1: 596 record(s) via SEI, 596 via the data channel over 596 \
                 sample(s), below the floor of 5319"
            ),
            "{}",
            v.detail
        );

        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs[0].1.serve.seconds = 0.0;
        let r = build_soak_results(inputs).unwrap();
        for name in ["whep_frames_continuous_whep", "whep_klv_sei_equals_dc_whep"] {
            let v = verdict(&r, name);
            assert!(!v.pass, "{name}: {}", v.detail);
            assert!(v.detail.contains("names no window"), "{name}: {}", v.detail);
        }
    }

    /// Every server-side drop counter is gated at 0, each named in the
    /// verdict it belongs to.
    #[test]
    fn whep_server_drop_counters_fail_the_leg_by_name() {
        for (counter, verdict_name) in [
            ("klv_records_dropped_unbound", "whep_klv_sei_equals_dc_whep"),
            ("klv_sei_dropped_oversize", "whep_klv_sei_equals_dc_whep"),
            ("klv_dc_dropped_oversize", "whep_klv_sei_equals_dc_whep"),
            (
                "viewer_packets_dropped_queue_full",
                "whep_frames_continuous_whep",
            ),
            ("demux_nonconformant", "whep_frames_continuous_whep"),
        ] {
            let mut inputs = whep_inputs(2, Some(2));
            let m = &mut inputs.whep_legs[0].1.serve.mount;
            match counter {
                "klv_records_dropped_unbound" => m.klv_records_dropped_unbound = 1,
                "klv_sei_dropped_oversize" => m.klv_sei_dropped_oversize = 1,
                "klv_dc_dropped_oversize" => m.klv_dc_dropped_oversize = 1,
                "viewer_packets_dropped_queue_full" => m.viewer_packets_dropped_queue_full = 1,
                "demux_nonconformant" => m.demux_nonconformant = 1,
                _ => unreachable!(),
            }
            let r = build_soak_results(inputs).unwrap();
            let v = verdict(&r, verdict_name);
            assert!(!v.pass && !v.provisional, "{counter}: {}", v.detail);
            assert!(
                v.detail
                    .contains(&format!("mount counter {counter} is 1, not 0")),
                "{counter}: {}",
                v.detail
            );
            assert!(!r.overall_pass);
        }
    }

    /// A copy pair disagreeing on (PTS, RTP timestamp) fails the
    /// connection it was seen in: both copies come from one binding.
    #[test]
    fn whep_klv_sei_equals_dc_fails_on_a_disagreeing_copy_pair() {
        let mut inputs = whep_inputs(3, Some(3));
        inputs.whep_legs[0].1.viewer.connections[1].klv_copies_disagreeing = 1;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail
                .contains("connection 2: 1 record(s) whose SEI and data-channel copies"),
            "{}",
            v.detail
        );
        assert_eq!(r.whep_legs[0].klv_copies_disagreeing, 1);
        assert!(!r.overall_pass);
    }

    /// An empty connection list is a failure of both content verdicts,
    /// not a vacuous pass, and the viewer report's own window must be
    /// the slot less the relaunch gap.
    #[test]
    fn whep_verdicts_refuse_an_empty_report_and_a_self_stated_window() {
        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs[0].1.viewer.connections.clear();
        let r = build_soak_results(inputs).unwrap();
        for name in ["whep_frames_continuous_whep", "whep_klv_sei_equals_dc_whep"] {
            let v = verdict(&r, name);
            assert!(!v.pass, "{name}: {}", v.detail);
            assert!(
                v.detail.contains("lists no connection"),
                "{name}: {}",
                v.detail
            );
        }

        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs[0].1.viewer.connection_s = 0;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail
                .contains("each browser given 0 s (NOT = the slot less the 3 s"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn whep_klv_sei_equals_dc_fails_when_a_carriage_delivered_nothing() {
        let mut inputs = whep_inputs(2, Some(2));
        let c = &mut inputs.whep_legs[0].1.viewer.connections[0];
        c.klv_sei = 0;
        c.klv_sei_set_sha256 = c.klv_dc_set_sha256.clone();
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_klv_sei_equals_dc_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("connection 1: 0 record(s) via SEI"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn whep_reconnects_observed_holds_both_sides_to_the_declaration() {
        // The viewer made one connection too few.
        let r = build_soak_results(whep_inputs(5, Some(6))).unwrap();
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail
                .contains("5 browser connection(s) = 4 reconnect(s) against 5 expected"),
            "{}",
            v.detail
        );

        // The server saw a session the viewer loop did not make.
        let mut inputs = whep_inputs(6, Some(6));
        inputs.whep_legs[0].1.serve.server.sessions_total = 7;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("counted 7 session(s)"), "{}", v.detail);

        // The loop ran a different slot than declared.
        let mut inputs = whep_inputs(6, Some(6));
        inputs.whep_legs[0].1.viewer.reconnect_s = 300;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("300s slot against the declared 600s"),
            "{}",
            v.detail
        );

        // No declaration: nothing to judge against.
        let r = build_soak_results(whep_inputs(6, None)).unwrap();
        let v = verdict(&r, "whep_reconnects_observed_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("no viewer_connections"), "{}", v.detail);
    }

    #[test]
    fn profile_declared_whep_reads_the_servers_counters() {
        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs[0].1.serve.profile = Some("klv-sync".to_string());
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("served \"klv-sync\" / \"mp2t\""),
            "{}",
            v.detail
        );

        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs[0].1.serve.profile = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "profile_declared_whep");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("name no profile"), "{}", v.detail);
    }

    /// The whep leg's roles are `serve` and the viewer LOOP: a run
    /// missing either fails `worker_exits` by name, and one missing the
    /// loop's RSS fails its data-present verdict.
    #[test]
    fn whep_leg_requires_its_serve_and_viewer_roles() {
        let mut inputs = whep_inputs(2, Some(2));
        inputs.worker_exits.remove("whep-viewer");
        inputs
            .rss_samples
            .retain(|s| !(s.leg == "whep" && s.process == "viewer"));
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(!v.pass && v.detail.contains("whep-viewer"), "{}", v.detail);
        assert!(!verdict(&r, "rss_data_present_whep_viewer").pass);
    }

    /// A declared whep leg whose reports were not supplied fails
    /// `leg_evidence_whep`, like any other declared leg.
    #[test]
    fn a_declared_whep_leg_without_reports_fails_leg_evidence() {
        let mut inputs = whep_inputs(2, Some(2));
        inputs.whep_legs.clear();
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "leg_evidence_whep").pass);
        assert!(
            !r.verdicts.iter().any(|v| v.name.starts_with("whep_")),
            "{:?}",
            r.verdicts
        );
    }

    #[test]
    fn parse_soak_config_validates_the_whep_declaration() {
        let base = |legs: &str| {
            format!(
                r#"{{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                     "sampler_end_slack_s": 35, "legs": {{{legs}}}}}"#
            )
        };
        // The three whep fields come from `extra`, so each case names
        // its own values.
        let whep = |extra: &str| {
            base(&format!(
                r#""whep": {{"profile": "baseline", "schedule": null, "klv_set": "compact",
                    "reconnect_mode": null, "viewer": "google-chrome" {extra}}}"#
            ))
        };
        let good = r#", "mount_kind": "mp2t", "viewer_reconnect_s": 600, "viewer_connections": 6"#;
        assert!(parse_soak_config(&whep(good)).is_ok());
        let e = parse_soak_config(&whep(
            r#", "mount_kind": "mp2t", "viewer_reconnect_s": 600, "viewer_connections": 1"#,
        ))
        .unwrap_err();
        assert!(e.contains("viewer_connections >= 2"), "{e}");
        let e = parse_soak_config(&whep(
            r#", "mount_kind": "mp2t", "viewer_reconnect_s": 600"#,
        ))
        .unwrap_err();
        assert!(e.contains("viewer_connections >= 2"), "{e}");
        let e = parse_soak_config(&whep(
            r#", "mount_kind": "mp2t", "viewer_reconnect_s": 0, "viewer_connections": 6"#,
        ))
        .unwrap_err();
        assert!(e.contains("viewer_reconnect_s > 0"), "{e}");
        let e = parse_soak_config(&whep(
            r#", "mount_kind": "hls", "viewer_reconnect_s": 600, "viewer_connections": 6"#,
        ))
        .unwrap_err();
        assert!(e.contains("mount_kind"), "{e}");
        let e = parse_soak_config(&whep(
            r#", "viewer_reconnect_s": 600, "viewer_connections": 6"#,
        ))
        .unwrap_err();
        assert!(e.contains("mount_kind"), "{e}");
        // The viewer fields on a transport leg are a typo.
        let e = parse_soak_config(&base(
            r#""srt": {"profile": "baseline", "viewer_reconnect_s": 600}"#,
        ))
        .unwrap_err();
        assert!(e.contains("declares a viewer"), "{e}");
        // A publisher field on the whep leg is one too.
        let e =
            parse_soak_config(&whep(&format!("{good}, \"publisher_generations\": 6"))).unwrap_err();
        assert!(e.contains("declares a publisher"), "{e}");
    }

    #[test]
    fn publisher_generations_fails_one_below_the_floor() {
        let r = build_soak_results(publish_inputs(Some(5), Some(6))).unwrap();
        let v = verdict(&r, "publisher_generations_rtsp-publish");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("5 publisher(s)"), "{}", v.detail);
        assert!(!r.overall_pass);
    }

    /// A recv report with no `publish_mount` block says nothing about
    /// how many publishers came and went: a failure, not a pass.
    #[test]
    fn publisher_generations_fails_without_a_publish_mount_block() {
        let r = build_soak_results(publish_inputs(None, Some(6))).unwrap();
        let v = verdict(&r, "publisher_generations_rtsp-publish");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("no publish_mount block"), "{}", v.detail);
    }

    #[test]
    fn publisher_generations_fails_without_a_declared_count() {
        let r = build_soak_results(publish_inputs(Some(9), None)).unwrap();
        let v = verdict(&r, "publisher_generations_rtsp-publish");
        assert!(!v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("no publisher_generations"),
            "{}",
            v.detail
        );
    }

    /// A run without the publish leg emits no publish verdict at all
    /// and never asks for its roles.
    #[test]
    fn a_run_without_the_publish_leg_has_no_publisher_verdicts() {
        let r = build_soak_results(healthy_inputs()).unwrap();
        assert!(
            !r.verdicts.iter().any(|v| v.name.ends_with("_rtsp-publish")),
            "{:?}",
            r.verdicts
        );
        assert!(r.publish_legs.is_empty());
        assert!(verdict(&r, "worker_exits").pass);
    }

    /// The publish leg's roles are `recv` and the publisher LOOP: a
    /// run missing either fails `worker_exits` by name, and one
    /// missing the loop's RSS fails its data-present verdict.
    #[test]
    fn publish_leg_requires_its_recv_and_publisher_roles() {
        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs.worker_exits.remove("rtsp-publish-publisher");
        inputs
            .rss_samples
            .retain(|s| !(s.leg == "rtsp-publish" && s.process == "publisher"));
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(
            !v.pass && v.detail.contains("rtsp-publish-publisher"),
            "{}",
            v.detail
        );
        assert!(!verdict(&r, "rss_data_present_rtsp-publish_publisher").pass);
    }

    /// The not-applicable verdicts are not free passes: a publish leg
    /// declared with a schedule or a reconnect mode has nothing that
    /// could have run either, and fails.
    #[test]
    fn publish_leg_declaring_a_schedule_or_mode_fails_those_verdicts() {
        let mut inputs = publish_inputs(Some(6), Some(6));
        let decl = inputs.config.legs.get_mut("rtsp-publish").unwrap();
        decl.schedule = Some(ScheduleDeclaration {
            seed: 1,
            phases: 4,
            phase_s: 900,
        });
        decl.reconnect_mode = Some("background".to_string());
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "schedule_declared_rtsp-publish").pass);
        assert!(!verdict(&r, "reconnect_mode_declared_rtsp-publish").pass);
    }

    #[test]
    fn rss_csv_takes_publisher_rows_only_on_a_publish_leg() {
        let header = "elapsed_s,leg,process,pid,rss_kb\n";
        assert!(parse_rss_csv(&format!("{header}0,rtsp-publish,publisher,7,100\n")).is_ok());
        assert!(parse_rss_csv(&format!("{header}0,rtsp-publish,send,7,100\n")).is_err());
        assert!(parse_rss_csv(&format!("{header}0,srt,publisher,7,100\n")).is_err());
    }

    fn config_with_publish_leg(leg: &str, decl: LegDeclaration) -> String {
        let mut legs = serde_json::Map::new();
        legs.insert(leg.to_string(), serde_json::to_value(decl).unwrap());
        serde_json::json!({
            "expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
            "sampler_end_slack_s": 35, "expected_worker_exits": {}, "legs": legs
        })
        .to_string()
    }

    /// The declaration is validated at launch: a publish leg needs a
    /// drop period and at least two generations, and a transport leg
    /// cannot declare a publisher.
    #[test]
    fn soak_config_validates_the_publish_leg_declaration() {
        let cfg = parse_soak_config(&config_with_publish_leg(
            "rtsp-publish",
            publish_declaration(Some(6)),
        ))
        .expect("a soak.sh-shaped publish leg must validate");
        assert_eq!(cfg.legs["rtsp-publish"].publisher_generations, Some(6));
        assert!(
            parse_soak_config(&config_with_publish_leg(
                "rtsp-publish",
                publish_declaration(Some(2))
            ))
            .is_ok()
        );
        for bad in [None, Some(0), Some(1)] {
            let err = parse_soak_config(&config_with_publish_leg(
                "rtsp-publish",
                publish_declaration(bad),
            ))
            .expect_err("fewer than two generations must be rejected");
            assert!(err.contains("publisher_generations"), "{err}");
        }
        let mut no_period = publish_declaration(Some(6));
        no_period.publisher_drop_period_s = None;
        let err = parse_soak_config(&config_with_publish_leg("rtsp-publish", no_period))
            .expect_err("a publish leg without a period must be rejected");
        assert!(err.contains("publisher_drop_period_s"), "{err}");
        let err = parse_soak_config(&config_with_publish_leg(
            "srt",
            publish_declaration(Some(6)),
        ))
        .expect_err("a transport leg declaring a publisher must be rejected");
        assert!(err.contains("only a publish leg"), "{err}");
    }

    /// Short periods: the floor is the declared count, never a count
    /// re-derived from the run length (which could exceed what the run
    /// launched). 7200 s at 60 s and 3600 s at 30 s both launch 120
    /// generations: 120 or more ended passes, 119 fails.
    #[test]
    fn short_period_runs_are_judged_against_the_declared_count() {
        for duration_s in [7200.0, 3600.0] {
            for (ended, pass) in [(121, true), (120, true), (119, false)] {
                let mut inputs = publish_inputs(Some(ended), Some(120));
                inputs.config.expected_duration_s = duration_s;
                let r = build_soak_results(inputs).unwrap();
                let v = verdict(&r, "publisher_generations_rtsp-publish");
                assert_eq!(v.pass, pass, "{duration_s}s, {ended} ended: {}", v.detail);
                assert!(v.detail.contains("floor of 120"), "{}", v.detail);
            }
        }
    }

    /// A recv report written before `publish_mount` existed (the key
    /// absent) still loads, and the leg is judged as missing evidence
    /// rather than `report soak` erroring.
    #[test]
    fn a_recv_report_without_publish_mount_loads_and_fails_the_leg() {
        let mut older = passing_recv_report(1000);
        older.profile = Some("baseline".to_string());
        let json = serde_json::to_string(&older).unwrap();
        assert!(!json.contains("publish_mount"), "{json}");
        let recv_report: VerifyReport =
            serde_json::from_str(&json).expect("the older shape must parse");
        assert!(recv_report.publish_mount.is_none());
        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs.publish_legs[0].1 = PublishLegArtifacts {
            recv_report,
            source_sha256_file: None,
        };
        let r = build_soak_results(inputs).expect("must judge, not error");
        let v = verdict(&r, "publisher_generations_rtsp-publish");
        assert!(
            !v.pass && v.detail.contains("no publish_mount block"),
            "{}",
            v.detail
        );
    }

    /// The case the count floors cannot see: the last generation's
    /// bytes never reached the receiver, so the mount still counted
    /// every publisher and the rest of the stream cleared the floors,
    /// but the received digest differs from the declared source's.
    #[test]
    fn delivery_complete_fails_on_a_different_received_digest() {
        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs.publish_legs[0].1.recv_report.metrics.stream_sha256 = "0".repeat(64);
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains(SOURCE_SHA), "{}", v.detail);
        assert!(verdict(&r, "publisher_generations_rtsp-publish").pass);
        assert!(!r.overall_pass);
    }

    /// A config written before `source_sha256` existed: the digest
    /// comes from the `source.sha256` file beside the recv report, and
    /// the verdict says where it came from.
    #[test]
    fn delivery_complete_falls_back_to_the_source_sha256_artefact() {
        let undeclared_with_file = |file: String| {
            let mut inputs = publish_inputs(Some(6), Some(6));
            inputs
                .config
                .legs
                .get_mut("rtsp-publish")
                .unwrap()
                .source_sha256 = None;
            inputs.publish_legs[0].1.source_sha256_file = Some(file);
            build_soak_results(inputs).unwrap()
        };
        let r = undeclared_with_file(SOURCE_SHA.to_string());
        let v = verdict(&r, "delivery_complete_rtsp-publish");
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("declared via the source.sha256 artefact"),
            "{}",
            v.detail
        );
        assert!(r.overall_pass, "{:?}", r.verdicts);

        let r = undeclared_with_file("1".repeat(64));
        assert!(!verdict(&r, "delivery_complete_rtsp-publish").pass);
        assert!(!r.overall_pass);
    }

    /// With neither a declared digest nor the file, nothing says what
    /// the receiver should have got: a failure, not a pass.
    #[test]
    fn delivery_complete_fails_without_any_source_digest() {
        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs
            .config
            .legs
            .get_mut("rtsp-publish")
            .unwrap()
            .source_sha256 = None;
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "delivery_complete_rtsp-publish");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(
            v.detail.contains("no source digest declared"),
            "{}",
            v.detail
        );
        assert!(!r.overall_pass);
    }

    /// A declared digest must be what `sha256sum` prints, and only a
    /// publish leg has one.
    #[test]
    fn soak_config_validates_the_source_digest() {
        let mut bad = publish_declaration(Some(6));
        bad.source_sha256 = Some(SOURCE_SHA.to_uppercase());
        let err = parse_soak_config(&config_with_publish_leg("rtsp-publish", bad))
            .expect_err("an upper-case digest must be rejected");
        assert!(err.contains("source_sha256"), "{err}");
        let mut on_srt = publish_declaration(None);
        on_srt.publisher = None;
        on_srt.publisher_drop_period_s = None;
        let err = parse_soak_config(&config_with_publish_leg("srt", on_srt))
            .expect_err("a transport leg declaring a source digest must be rejected");
        assert!(err.contains("only a publish leg"), "{err}");
    }

    /// A leg the config declares but whose report was never supplied
    /// is not silently skipped: its own verdicts and roles would
    /// otherwise vanish and the run could pass without it.
    #[test]
    fn a_declared_leg_without_artefacts_fails_the_run() {
        let mut inputs = publish_inputs(Some(6), Some(6));
        inputs.publish_legs.clear();
        let r = build_soak_results(inputs).unwrap();
        let v = verdict(&r, "leg_evidence_rtsp-publish");
        assert!(!v.pass && !v.provisional, "{}", v.detail);
        assert!(v.detail.contains("no report supplied"), "{}", v.detail);
        assert!(!r.overall_pass);

        // Same for a transport leg.
        let mut inputs = healthy_inputs();
        inputs.config.legs.insert(
            "rist".to_string(),
            LegDeclaration {
                profile: "baseline".to_string(),
                schedule: None,
                klv_set: None,
                klv_seed: None,
                reconnect_mode: None,
                publisher: None,
                publisher_drop_period_s: None,
                publisher_generations: None,
                source_sha256: None,
                remuxed: None,
                klv_records_per_generation: None,
                klv_placement: None,
                mount_kind: None,
                viewer: None,
                viewer_reconnect_s: None,
                viewer_connections: None,
            },
        );
        let r = build_soak_results(inputs).unwrap();
        assert!(!verdict(&r, "leg_evidence_rist").pass);
        assert!(!r.overall_pass);
    }
}

#[cfg(test)]
mod soak_script_literals {
    use super::{RSS_SLOPE_THRESHOLD_KB_PER_HOUR, WARMUP_FRACTION};

    /// The gate's constants are copies of literals in `soak.sh`; a change on
    /// either side fails here.
    #[test]
    fn the_rss_constants_match_soak_sh() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("interop")
            .join("soak.sh");
        let text = std::fs::read_to_string(script).unwrap();
        let threshold = format!(
            "RSS_SLOPE_THRESHOLD={}",
            RSS_SLOPE_THRESHOLD_KB_PER_HOUR as u64
        );
        assert_eq!(
            f64::from(RSS_SLOPE_THRESHOLD_KB_PER_HOUR as u32),
            RSS_SLOPE_THRESHOLD_KB_PER_HOUR
        );
        assert!(text.contains(&threshold), "soak.sh has no {threshold:?}");
        let warmup = format!("warmup_fraction: {WARMUP_FRACTION}");
        assert!(text.contains(&warmup), "soak.sh has no {warmup:?}");
    }
}
