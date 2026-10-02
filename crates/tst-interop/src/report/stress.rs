//! `report step` / `report stress`: the stress harness's verdicts.
//!
//! One STEP is one load level of the sweep (N process-triple streams
//! of one transport at one `--au-scale`) held for a fixed window, or
//! the 24 h hold. `build_step_results` judges a step from the sampler
//! CSVs (`rss.csv`, `proc.csv`, `host.csv` — the same shapes `soak.sh`
//! writes) plus the per-stream send/recv/proxy reports; `run_step` is
//! its file-driven wrapper. `build_stress_results` folds every step's
//! `step-results.json` into one `stress-results.json` with the ceiling
//! rule (last PASS before the first FAIL on each axis).
//!
//! Resource verdicts exclude a declared warm-up and use percentiles,
//! never averages. A missing or malformed artifact is an `Err`, never a
//! vacuous PASS (the same stance as `super::soak`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::soak::{RssSample, linear_regression_slope};
use crate::proxy::ProxyStats;
use crate::report_types::{CellMetrics, VerifyReport};

const PROC_HEADER: &str = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds";
const HOST_HEADER: &str = "elapsed_s,load1,load5,load15,procs_running,mem_available_kb";
const RSS_HEADER: &str = "elapsed_s,leg,process,pid,rss_kb";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcSample {
    pub elapsed_s: f64,
    pub leg: String,
    pub process: String,
    pub pid: u32,
    pub utime_ticks: Option<u64>,
    pub stime_ticks: Option<u64>,
    pub threads: Option<u64>,
    pub fds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    pub elapsed_s: f64,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub procs_running: Option<u64>,
    pub mem_available_kb: Option<u64>,
}

fn opt<T: std::str::FromStr>(field: &str, name: &str, line_no: usize) -> Result<Option<T>, String> {
    if field.is_empty() {
        return Ok(None);
    }
    field
        .parse::<T>()
        .map(Some)
        .map_err(|_| format!("line {line_no}: {name} must be numeric or empty, got {field:?}"))
}

fn csv_rows<'a>(
    text: &'a str,
    header: &str,
    columns: usize,
    file: &str,
) -> Result<Vec<(usize, Vec<&'a str>)>, String> {
    let mut lines = text.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(format!("{file}: empty file (expected a header line)"));
    };
    if first.trim() != header {
        return Err(format!(
            "{file} line 1: expected header `{header}`, got: {first:?}"
        ));
    }
    let mut rows = Vec::new();
    for (idx, raw) in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != columns {
            return Err(format!(
                "{file} line {}: expected {columns} fields, got {}: {line:?}",
                idx + 1,
                fields.len()
            ));
        }
        rows.push((idx + 1, fields));
    }
    Ok(rows)
}

pub fn parse_proc_csv(text: &str) -> Result<Vec<ProcSample>, String> {
    csv_rows(text, PROC_HEADER, 8, "proc.csv")?
        .into_iter()
        .map(|(n, f)| {
            Ok(ProcSample {
                elapsed_s: f[0]
                    .parse()
                    .map_err(|_| format!("proc.csv line {n}: bad elapsed_s"))?,
                leg: f[1].to_string(),
                process: f[2].to_string(),
                pid: f[3]
                    .parse()
                    .map_err(|_| format!("proc.csv line {n}: bad pid"))?,
                utime_ticks: opt(f[4], "utime_ticks", n)?,
                stime_ticks: opt(f[5], "stime_ticks", n)?,
                threads: opt(f[6], "threads", n)?,
                fds: opt(f[7], "fds", n)?,
            })
        })
        .collect()
}

/// Parse the stress harness's own `rss.csv`. Same shape as
/// `super::soak::parse_rss_csv` (same header, same columns) but WITHOUT
/// that function's `KNOWN_LEGS` allowlist (`srt`/`rist` only, sized for
/// `soak.sh`'s fixed two-leg design): a stress step runs N concurrent
/// streams of one transport, each with its own leg (`<transport>-<k>`,
/// see `StreamArtifacts::leg`), so soak's allowlist would reject every
/// real stress artifact. `RssSample` itself is still the shared type —
/// only the parsing/validation is duplicated here.
pub fn parse_rss_csv(text: &str) -> Result<Vec<RssSample>, String> {
    csv_rows(text, RSS_HEADER, 5, "rss.csv")?
        .into_iter()
        .map(|(n, f)| {
            Ok(RssSample {
                elapsed_s: f[0]
                    .parse()
                    .map_err(|_| format!("rss.csv line {n}: bad elapsed_s"))?,
                leg: f[1].to_string(),
                process: f[2].to_string(),
                pid: f[3]
                    .parse()
                    .map_err(|_| format!("rss.csv line {n}: bad pid"))?,
                rss_kb: opt(f[4], "rss_kb", n)?,
            })
        })
        .collect()
}

pub fn parse_host_csv(text: &str) -> Result<Vec<HostSample>, String> {
    csv_rows(text, HOST_HEADER, 6, "host.csv")?
        .into_iter()
        .map(|(n, f)| {
            Ok(HostSample {
                elapsed_s: f[0]
                    .parse()
                    .map_err(|_| format!("host.csv line {n}: bad elapsed_s"))?,
                load1: opt(f[1], "load1", n)?,
                load5: opt(f[2], "load5", n)?,
                load15: opt(f[3], "load15", n)?,
                procs_running: opt(f[4], "procs_running", n)?,
                mem_available_kb: opt(f[5], "mem_available_kb", n)?,
            })
        })
        .collect()
}

/// One `send: heartbeat ...` line a `tst-interop send` prints every
/// 60 s. `reconnects`/`gap_len` are printed only by a managed sender.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub elapsed_s: u64,
    pub reconnects: Option<u64>,
    pub gap_len: Option<u64>,
}

/// Every heartbeat in a send log. Any other line (start-up chatter,
/// warnings) is ignored, as is a heartbeat line without a numeric
/// `elapsed_s` — a send log is free-form, not a declared artifact.
pub fn parse_send_heartbeats(log: &str) -> Vec<Heartbeat> {
    log.lines()
        .filter_map(|line| line.strip_prefix("send: heartbeat "))
        .filter_map(|rest| {
            let mut elapsed_s = None;
            let mut reconnects = None;
            let mut gap_len = None;
            for (k, v) in rest.split_whitespace().filter_map(|kv| kv.split_once('=')) {
                match k {
                    "elapsed_s" => elapsed_s = v.parse().ok(),
                    "reconnects" => reconnects = v.parse().ok(),
                    "gap_len" => gap_len = v.parse().ok(),
                    _ => {}
                }
            }
            Some(Heartbeat {
                elapsed_s: elapsed_s?,
                reconnects,
                gap_len,
            })
        })
        .collect()
}

/// One line of the hold's `restart-events.log`, written by `stress.sh`
/// each time it kills and relaunches a receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartEvent {
    pub elapsed_s: f64,
    /// `<leg>-recv`.
    pub role: String,
}

/// Parse `<elapsed_s> RESTART role=<leg>-recv old_pid=N new_pid=N`
/// lines. Blank lines are skipped; anything else malformed is an `Err`
/// naming the line — this file is the evidence the restarts happened.
pub fn parse_restart_events(text: &str) -> Result<Vec<RestartEvent>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || format!("restart-events.log line {}: malformed: {line:?}", i + 1);
        let f: Vec<&str> = line.split_whitespace().collect();
        let [elapsed, "RESTART", role, old_pid, new_pid] = f.as_slice() else {
            return Err(bad());
        };
        let elapsed_s: f64 = elapsed.parse().map_err(|_| bad())?;
        let role = role.strip_prefix("role=").ok_or_else(bad)?;
        for (field, key) in [(old_pid, "old_pid="), (new_pid, "new_pid=")] {
            field
                .strip_prefix(key)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(bad)?;
        }
        if role.is_empty() || !elapsed_s.is_finite() {
            return Err(bad());
        }
        out.push(RestartEvent {
            elapsed_s,
            role: role.to_string(),
        });
    }
    Ok(out)
}

/// Which axis of the sweep a step belongs to — the ceiling rule walks
/// each axis separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Streams,
    Bitrate,
    Hold,
}

/// The step dir's `config.json`, written by `stress.sh` BEFORE launch,
/// so a step is judged against what was declared, not what happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDeclaration {
    pub transport: String,
    pub axis: Axis,
    pub streams: u32,
    pub au_scale: u32,
    /// Samples before this elapsed time are start-up noise and excluded
    /// from every resource verdict.
    pub warmup_s: f64,
    /// Length of the judged window (it follows the warm-up).
    pub hold_s: f64,
    /// Seconds between sampler ticks (30 in production, 5 in smoke) —
    /// the denominator of `sample_coverage`.
    pub sample_cadence_s: f64,
    pub vcpus: u32,
    /// `getconf CLK_TCK` on the host — converts `/proc` ticks to seconds.
    pub clk_tck: u64,
    pub nominal_mbps_per_stream: f64,
    pub managed: bool,
    pub outage_period_s: Option<u64>,
    pub outage_dur_s: Option<u64>,
    pub restart_period_s: Option<u64>,
}

/// Pass/fail limits for one step. `rss_slope_kb_per_hour` defaults to
/// 0.0, meaning "unset": there is no safe universal value, so
/// `build_step_results` refuses to judge until the caller sets one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepThresholds {
    pub rss_slope_kb_per_hour: f64,
    pub fd_delta_max: u64,
    pub thread_delta_max: u64,
    pub cpu_headroom_max: f64,
    pub delivery_slack: f64,
    pub queue_depth_fraction: f64,
}

impl Default for StepThresholds {
    fn default() -> Self {
        Self {
            rss_slope_kb_per_hour: 0.0,
            fd_delta_max: 2,
            thread_delta_max: 1,
            cpu_headroom_max: 0.80,
            delivery_slack: 0.7,
            queue_depth_fraction: 0.9,
        }
    }
}

/// One stream's reports: what the sender pushed, what the receiver
/// verified, and (when present) the proxy's counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtifacts {
    pub index: u32,
    /// The stream's leg name as the sampler CSVs and `exits.json` spell
    /// it (`<transport>-<k>`). Carried rather than derived from the
    /// step's transport: the hold step mixes transports.
    pub leg: String,
    pub send: CellMetrics,
    pub recv: VerifyReport,
    pub proxy: Option<ProxyStats>,
}

/// Everything `build_step_results` judges, already parsed — the
/// function itself does no I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInputs {
    pub decl: StepDeclaration,
    pub thresholds: StepThresholds,
    pub rss: Vec<RssSample>,
    pub proc: Vec<ProcSample>,
    pub host: Vec<HostSample>,
    pub worker_exits: BTreeMap<String, i32>,
    pub streams: Vec<StreamArtifacts>,
    /// The hold's receiver restarts (`restart-events.log`); empty for a
    /// sweep step. A restarted leg's `recv-report.json` covers only the
    /// segment after its LAST restart, so `delivery_complete` scales
    /// that stream's expected AUs to the segment.
    #[serde(default)]
    pub restarts: Vec<RestartEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepVerdict {
    pub name: String,
    pub pass: bool,
    pub observed: f64,
    pub threshold: f64,
    pub detail: String,
}

/// Per-stream figures recorded for the ceiling report. Nothing here is
/// gated; the verdicts are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFigures {
    pub index: u32,
    pub leg: String,
    pub cpu_seconds: BTreeMap<String, f64>,
    pub rss_kb_p99: BTreeMap<String, u64>,
    pub threads_max: BTreeMap<String, u64>,
    pub fds_max: BTreeMap<String, u64>,
    pub recv_video_aus: u64,
    pub send_video_aus: u64,
    /// For a leg that was restarted, the receiver report covers only
    /// the last segment while the divisor is the full hold, so this
    /// figure is understated by the same fraction `delivery_complete`
    /// corrects for; it is recorded, never gated.
    pub wire_mbps: f64,
    pub reconnects: Option<u64>,
    /// Recorded from the proxy's stats, never gated: the sweep's link
    /// is clean and drop-rate judging belongs to soak.
    pub proxy_forwarded: Option<u64>,
    pub proxy_dropped: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResults {
    pub decl: StepDeclaration,
    pub thresholds: StepThresholds,
    pub pass: bool,
    pub verdicts: Vec<StepVerdict>,
    pub failing: Vec<String>,
    pub per_stream: Vec<StreamFigures>,
    pub aggregate_cpu_fraction: f64,
    pub cpu_fraction_per_stream: f64,
    pub aggregate_wire_mbps: f64,
    pub samples_used: usize,
}

/// The processes one stream runs, as named in the sampler CSVs and in
/// `exits.json` (`<leg>-<role>`). A stream without proxy stats (TCP:
/// send connects straight to recv) has no proxy process to expect.
fn roles(s: &StreamArtifacts) -> &'static [&'static str] {
    if s.proxy.is_some() {
        &["send", "proxy", "recv"]
    } else {
        &["send", "recv"]
    }
}

/// Same floor as soak's `MIN_SAMPLE_COVERAGE`: below it a resource
/// verdict rests on too few points to mean anything.
const MIN_SAMPLE_COVERAGE: f64 = 0.9;

/// A sampler row keyed by `(leg, process, pid)` at an elapsed time.
trait Sampled {
    fn elapsed_s(&self) -> f64;
    fn leg(&self) -> &str;
    fn process(&self) -> &str;
    fn pid(&self) -> u32;
}

impl Sampled for ProcSample {
    fn elapsed_s(&self) -> f64 {
        self.elapsed_s
    }
    fn leg(&self) -> &str {
        &self.leg
    }
    fn process(&self) -> &str {
        &self.process
    }
    fn pid(&self) -> u32 {
        self.pid
    }
}

impl Sampled for RssSample {
    fn elapsed_s(&self) -> f64 {
        self.elapsed_s
    }
    fn leg(&self) -> &str {
        &self.leg
    }
    fn process(&self) -> &str {
        &self.process
    }
    fn pid(&self) -> u32 {
        self.pid
    }
}

/// One pid's post-warm-up samples of a `(leg, process)`. A process the
/// hold restarts has one segment per pid; every other process has one.
struct Segment<'a, S> {
    pid: u32,
    samples: Vec<&'a S>,
}

type Groups<'a, S> = BTreeMap<(String, String), Vec<Segment<'a, S>>>;

/// Samples grouped by `(leg, process)`, split into one segment per pid
/// (in order of each pid's first row). Each segment drops the rows
/// within `warmup_s` of THAT pid's first row, so a restarted process's
/// start-up ramp is excluded the same way the original's is; for a pid
/// first sampled at ~0 s this is the plain `elapsed_s >= warmup_s`
/// rule. A segment left empty is kept (verdicts name it as skipped);
/// a `(leg, process)` whose every segment is empty is left out, as a
/// process with no post-warm-up row always was.
fn by_process<S: Sampled>(samples: &[S], warmup_s: f64) -> Groups<'_, S> {
    // One pass into (leg, process, pid) → (first elapsed_s, rows): the
    // 24 h hold's CSVs run to ~550k rows.
    type PidRows<'s, S> = BTreeMap<(&'s str, &'s str, u32), (f64, Vec<&'s S>)>;
    let mut by_pid: PidRows<'_, S> = BTreeMap::new();
    for s in samples {
        let (t0, rows) = by_pid
            .entry((s.leg(), s.process(), s.pid()))
            .or_insert((s.elapsed_s(), Vec::new()));
        *t0 = t0.min(s.elapsed_s());
        rows.push(s);
    }
    let mut order: Vec<_> = by_pid.into_iter().collect();
    order.sort_by(|a, b| a.1.0.total_cmp(&b.1.0));
    let mut groups: Groups<'_, S> = BTreeMap::new();
    for ((leg, process, pid), (t0, rows)) in order {
        let samples = rows
            .into_iter()
            .filter(|s| s.elapsed_s() >= t0 + warmup_s)
            .collect();
        groups
            .entry((leg.to_string(), process.to_string()))
            .or_default()
            .push(Segment { pid, samples });
    }
    groups.retain(|_, segs| segs.iter().any(|seg| !seg.samples.is_empty()));
    groups
}

/// CPU seconds one process burned between its first and last
/// post-warm-up rows that carry BOTH tick fields. `None` with fewer
/// than two such rows (a process that died early writes empty fields).
fn process_cpu_seconds(samples: &[&ProcSample], clk_tck: u64) -> Option<f64> {
    let mut usable = samples
        .iter()
        .filter_map(|s| match (s.utime_ticks, s.stime_ticks) {
            (Some(u), Some(st)) => Some(u + st),
            _ => None,
        });
    let first = usable.next()?;
    let last = usable.last()?;
    Some(last.saturating_sub(first) as f64 / clk_tck as f64)
}

/// A `(leg, process)`'s CPU seconds summed over its pid segments (each
/// pid's ticks count from its own start). `None` when no segment has a
/// figure; the pids without one are returned for the detail.
fn segments_cpu_seconds(segs: &[Segment<'_, ProcSample>], clk_tck: u64) -> (Option<f64>, Vec<u32>) {
    let mut total = None;
    let mut missing = Vec::new();
    for seg in segs {
        match process_cpu_seconds(&seg.samples, clk_tck) {
            Some(cpu_s) => *total.get_or_insert(0.0) += cpu_s,
            None => missing.push(seg.pid),
        }
    }
    (total, missing)
}

/// Judge one step. Errors (never a vacuous PASS) when the thresholds
/// are unset, the declaration is degenerate, or there is no evidence.
pub fn build_step_results(inputs: StepInputs) -> Result<StepResults, String> {
    let StepInputs {
        decl,
        thresholds,
        rss,
        proc,
        host: _host,
        worker_exits,
        streams,
        restarts,
    } = inputs;
    if thresholds.rss_slope_kb_per_hour <= 0.0 {
        return Err(
            "thresholds.rss_slope_kb_per_hour must be > 0 (a stress step has no provisional mode)"
                .into(),
        );
    }
    // Each of these is a divisor below; a zero would turn a verdict
    // into an infinity or NaN that compares as a silent pass or fail.
    if decl.hold_s <= 0.0 || decl.sample_cadence_s <= 0.0 || decl.vcpus == 0 || decl.clk_tck == 0 {
        return Err(format!(
            "config.json: hold_s, sample_cadence_s, vcpus and clk_tck must all be > 0 \
             (got {}, {}, {}, {})",
            decl.hold_s, decl.sample_cadence_s, decl.vcpus, decl.clk_tck
        ));
    }
    if streams.is_empty() {
        return Err("no stream reports — refusing to judge a step with no evidence".into());
    }
    if proc.is_empty() || rss.is_empty() {
        return Err("proc.csv/rss.csv have zero data rows — the sampler never ticked".into());
    }

    let proc_groups = by_process(&proc, decl.warmup_s);
    let rss_groups = by_process(&rss, decl.warmup_s);

    let mut verdicts = Vec::new();
    verdicts.push(verdict_worker_exits(&worker_exits, &streams));
    verdicts.push(verdict_recv_invariants(&streams));
    verdicts.push(verdict_delivery_complete(
        &streams,
        &restarts,
        decl.warmup_s + decl.hold_s,
        thresholds.delivery_slack,
    ));
    let (cpu_verdict, cpu_fraction) =
        verdict_cpu_headroom(&decl, &proc_groups, thresholds.cpu_headroom_max);
    verdicts.push(cpu_verdict);
    verdicts.extend(verdict_rss_slopes(
        &decl,
        &rss_groups,
        thresholds.rss_slope_kb_per_hour,
    ));
    verdicts.extend(verdict_flat(
        &proc_groups,
        "fd_count_flat",
        |s| s.fds,
        thresholds.fd_delta_max,
    ));
    verdicts.extend(verdict_flat(
        &proc_groups,
        "thread_count_flat",
        |s| s.threads,
        thresholds.thread_delta_max,
    ));
    verdicts.push(verdict_sample_coverage(&decl, &rss, &streams));

    let per_stream = stream_figures(&decl, &proc_groups, &rss_groups, &streams);
    let failing: Vec<String> = verdicts
        .iter()
        .filter(|v| !v.pass)
        .map(|v| v.name.clone())
        .collect();
    let aggregate_wire_mbps = per_stream.iter().map(|s| s.wire_mbps).sum();
    Ok(StepResults {
        pass: failing.is_empty(),
        cpu_fraction_per_stream: cpu_fraction / decl.streams.max(1) as f64,
        aggregate_cpu_fraction: cpu_fraction,
        aggregate_wire_mbps,
        samples_used: proc_groups
            .values()
            .flatten()
            .map(|seg| seg.samples.len())
            .sum(),
        decl,
        thresholds,
        verdicts,
        failing,
        per_stream,
    })
}

/// Every role of every stream must have exited 0. A missing role is a
/// failure too: an absent status is not evidence of a clean exit.
fn verdict_worker_exits(exits: &BTreeMap<String, i32>, streams: &[StreamArtifacts]) -> StepVerdict {
    let mut required = Vec::new();
    for s in streams {
        for role in roles(s) {
            required.push(format!("{}-{role}", s.leg));
        }
    }
    let mut problems = Vec::new();
    for role in &required {
        match exits.get(role) {
            None => problems.push(format!("{role}: no exit status recorded")),
            Some(&0) => {}
            Some(&status) => problems.push(format!("{role}: exit {status} (expected 0)")),
        }
    }
    for (role, &status) in exits {
        if status != 0 && !required.contains(role) {
            problems.push(format!("{role}: exit {status} (undeclared worker)"));
        }
    }
    StepVerdict {
        name: "worker_exits".into(),
        pass: problems.is_empty(),
        observed: problems.len() as f64,
        threshold: 0.0,
        detail: if problems.is_empty() {
            "every worker exited 0".into()
        } else {
            problems.join("; ")
        },
    }
}

fn verdict_recv_invariants(streams: &[StreamArtifacts]) -> StepVerdict {
    let failing: Vec<String> = streams
        .iter()
        .filter(|s| !s.recv.pass)
        .map(|s| format!("{}: {}", s.leg, s.recv.failures.join(", ")))
        .collect();
    StepVerdict {
        name: "recv_invariants".into(),
        pass: failing.is_empty(),
        observed: failing.len() as f64,
        threshold: 0.0,
        detail: if failing.is_empty() {
            "every receiver passed its profile invariants".into()
        } else {
            failing.join("; ")
        },
    }
}

/// Upper bound on a stream's received ÷ expected AUs. Above it the
/// receiver counted more than the sender could have sent into its
/// window: its report did not reset across a restart, or the
/// accounting is wrong — never a delivery pass.
const DELIVERY_RATIO_MAX: f64 = 1.2;

/// Received ÷ expected video AUs per stream: the worst must reach
/// `slack`, and none may exceed `DELIVERY_RATIO_MAX`. A stream that
/// sent nothing scores 0: no traffic is not delivery.
///
/// Expected is the sender's count, except on a leg the hold restarted:
/// the relaunched receiver writes `recv-report.json` afresh, so it
/// covers only the time after the leg's LAST restart `t_last`. The
/// sender and the restart clock both run `run_s` = warm-up + hold from
/// the same start, and the sender paces at a constant rate, so that
/// segment should hold `send × (run_s − t_last) / run_s` AUs (to within
/// a frame or two).
fn verdict_delivery_complete(
    streams: &[StreamArtifacts],
    restarts: &[RestartEvent],
    run_s: f64,
    slack: f64,
) -> StepVerdict {
    let mut min_ratio = f64::INFINITY;
    let mut max_ratio = f64::NEG_INFINITY;
    let mut notes = Vec::new();
    let mut over = Vec::new();
    let mut scaled = Vec::new();
    for s in streams {
        let role = format!("{}-recv", s.leg);
        let t_last = restarts
            .iter()
            .filter(|ev| ev.role == role)
            .map(|ev| ev.elapsed_s)
            .reduce(f64::max);
        let fraction = t_last.map_or(1.0, |t| ((run_s - t) / run_s).max(0.0));
        let expected = s.send.video_aus as f64 * fraction;
        if let Some(t) = t_last {
            scaled.push(format!(
                "{}: last restart at {t} s, expecting {fraction:.4} of {} sent AUs",
                s.leg, s.send.video_aus
            ));
        }
        let ratio = if s.send.video_aus == 0 {
            notes.push(format!("{}: sender reported 0 video AUs", s.leg));
            0.0
        } else if expected <= 0.0 {
            // expected ≤ 0 ⇔ t_last ≥ run_s: the restart came at or
            // after the end of the run.
            notes.push(format!(
                "{}: last restart at {} s leaves no segment of the {run_s} s run to judge",
                s.leg,
                t_last.unwrap_or_default()
            ));
            0.0
        } else {
            let ratio = s.recv.metrics.video_aus as f64 / expected;
            if ratio < slack {
                notes.push(format!(
                    "{}: {}/{expected:.0} AUs = {ratio:.4}",
                    s.leg, s.recv.metrics.video_aus
                ));
            } else if ratio > DELIVERY_RATIO_MAX {
                over.push(format!(
                    "{}: recv exceeds expected — report did not reset or accounting mismatch \
                     ({}/{expected:.0} AUs = {ratio:.4} > {DELIVERY_RATIO_MAX})",
                    s.leg, s.recv.metrics.video_aus
                ));
            }
            ratio
        };
        min_ratio = min_ratio.min(ratio);
        max_ratio = max_ratio.max(ratio);
    }
    let low = min_ratio < slack;
    notes.extend(over.iter().cloned());
    let mut detail = if notes.is_empty() {
        format!("delivered {min_ratio:.4}..{max_ratio:.4} of expected AUs across streams")
    } else {
        notes.join("; ")
    };
    if !scaled.is_empty() {
        detail.push_str(&format!("; restarted: {}", scaled.join("; ")));
    }
    // A shortfall is reported against `slack`; otherwise an excess is
    // reported against the upper bound.
    let (observed, threshold) = if !low && !over.is_empty() {
        (max_ratio, DELIVERY_RATIO_MAX)
    } else {
        (min_ratio, slack)
    };
    StepVerdict {
        name: "delivery_complete".into(),
        pass: !low && over.is_empty(),
        observed,
        threshold,
        detail,
    }
}

/// Total CPU of every sampled process over the post-warm-up window, as
/// a fraction of the host's cores. A restarted process contributes the
/// sum of its pid segments' tick deltas. Returns the verdict and the
/// fraction.
///
/// CPU burned during a restarted pid's own warm-up (the first
/// `warmup_s` after its first sample) is excluded while the wall-clock
/// window still spans it, so a restarted process's fraction is
/// understated: under 1 % over a 24 h hold, about a third of the
/// restarted receiver's CPU in the 6-minute smoke.
fn verdict_cpu_headroom(
    decl: &StepDeclaration,
    groups: &Groups<'_, ProcSample>,
    max: f64,
) -> (StepVerdict, f64) {
    let warm_t = groups
        .values()
        .flatten()
        .flat_map(|seg| seg.samples.iter().map(|s| s.elapsed_s));
    let (lo, hi) = warm_t.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), t| {
        (lo.min(t), hi.max(t))
    });
    let window_s = hi - lo;
    let mut total_cpu_s = 0.0;
    let mut usable = 0usize;
    let mut unusable = Vec::new();
    for ((leg, process), segs) in groups {
        let (cpu_s, missing) = segments_cpu_seconds(segs, decl.clk_tck);
        if let Some(cpu_s) = cpu_s {
            total_cpu_s += cpu_s;
            usable += 1;
        }
        for pid in missing {
            unusable.push(format!("{leg}/{process} pid {pid}"));
        }
    }
    // Either case would report 0 % CPU from no measurement at all — a
    // vacuous pass, so it fails instead.
    let no_figure = if !window_s.is_finite() || window_s <= 0.0 {
        Some("post-warm-up window is empty — no CPU figure".to_string())
    } else if usable == 0 {
        Some(format!(
            "no process has 2 usable tick samples — no CPU figure: {}",
            unusable.join(", ")
        ))
    } else {
        None
    };
    if let Some(detail) = no_figure {
        let verdict = StepVerdict {
            name: "cpu_headroom".into(),
            pass: false,
            observed: 0.0,
            threshold: max,
            detail,
        };
        return (verdict, 0.0);
    }
    let fraction = total_cpu_s / window_s / decl.vcpus as f64;
    let mut detail = format!(
        "{total_cpu_s:.1} CPU-s over {window_s:.0} s on {} vCPUs",
        decl.vcpus
    );
    if !unusable.is_empty() {
        detail.push_str(&format!(
            "; < 2 usable samples (counted as 0): {}",
            unusable.join(", ")
        ));
    }
    let verdict = StepVerdict {
        name: "cpu_headroom".into(),
        pass: fraction <= max,
        observed: fraction,
        threshold: max,
        detail,
    };
    (verdict, fraction)
}

/// Split a `(leg, process)`'s segments into the judgeable ones (≥ 2
/// usable values) and the rest. When none is judgeable the segments
/// with ≥ 1 value are returned instead, so a process sampled once is
/// judged as it always was (delta 0, a PASS); `skipped` names every
/// segment left out. Serves the flat verdicts only: the RSS slope
/// judges segments by time span instead (`verdict_rss_slopes`).
fn judgeable<T>(per_seg: &[(u32, Vec<T>)]) -> (Vec<&(u32, Vec<T>)>, Vec<String>) {
    let mut judged: Vec<&(u32, Vec<T>)> = per_seg.iter().filter(|(_, v)| v.len() >= 2).collect();
    if judged.is_empty() {
        judged = per_seg.iter().filter(|(_, v)| !v.is_empty()).collect();
    }
    let skipped = per_seg
        .iter()
        .filter(|seg| !judged.iter().any(|j| j.0 == seg.0))
        .map(|(pid, v)| format!("pid {pid} ({} usable)", v.len()))
        .collect();
    (judged, skipped)
}

fn skipped_note(skipped: &[String]) -> String {
    if skipped.is_empty() {
        String::new()
    } else {
        format!(
            "; skipped (< 2 post-warm-up samples): {}",
            skipped.join(", ")
        )
    }
}

/// RSS growth per process in KB/hour, fit per pid segment (pooling a
/// restarted process's pids would fit a sawtooth).
///
/// A short span cannot resolve a small slope from noise, so each
/// segment's allowance is `threshold × 3600 / span` when its judged
/// span (last − first post-warm-up elapsed_s) is under an hour. A
/// segment spanning less than two sampler cadences is skipped and
/// named, unless no segment is long enough (then FAIL "insufficient
/// samples"). The verdict reports the segment furthest over (or
/// nearest to) its own allowance.
fn verdict_rss_slopes(
    decl: &StepDeclaration,
    groups: &Groups<'_, RssSample>,
    threshold: f64,
) -> Vec<StepVerdict> {
    let min_span_s = 2.0 * decl.sample_cadence_s;
    groups
        .iter()
        .map(|((leg, process), segs)| {
            let name = format!("rss_slope_{leg}_{process}");
            let mut judged = Vec::new();
            let mut skipped = Vec::new();
            let mut n_points = 0usize;
            for seg in segs {
                let points: Vec<(f64, f64)> = seg
                    .samples
                    .iter()
                    .filter_map(|s| s.rss_kb.map(|kb| (s.elapsed_s, kb as f64)))
                    .collect();
                n_points += points.len();
                let span_s = match (points.first(), points.last()) {
                    (Some(a), Some(b)) => b.0 - a.0,
                    _ => 0.0,
                };
                if points.len() < 2 || span_s < min_span_s {
                    skipped.push(format!(
                        "pid {} ({} samples over {span_s:.0} s)",
                        seg.pid,
                        points.len()
                    ));
                    continue;
                }
                let per_hour: Vec<(f64, f64)> =
                    points.iter().map(|&(t, kb)| (t / 3600.0, kb)).collect();
                let slope = linear_regression_slope(&per_hour);
                let allowed = if span_s < 3600.0 {
                    threshold * 3600.0 / span_s
                } else {
                    threshold
                };
                judged.push((seg.pid, slope, allowed, points.len(), span_s));
            }
            let skipped_note = if skipped.is_empty() {
                String::new()
            } else {
                format!(
                    "; skipped (span < 2 × {} s cadence): {}",
                    decl.sample_cadence_s,
                    skipped.join(", ")
                )
            };
            let Some(&(worst_pid, slope, allowed, n, span_s)) = judged
                .iter()
                .max_by(|a, b| (a.1 / a.2).total_cmp(&(b.1 / b.2)))
            else {
                return StepVerdict {
                    name,
                    pass: false,
                    observed: 0.0,
                    threshold,
                    detail: format!("insufficient samples ({n_points}){skipped_note}"),
                };
            };
            let mut detail = format!(
                "{slope:.1} KB/h over {n} samples ({span_s:.0} s) of pid {worst_pid} \
                 (allowed {allowed:.1})"
            );
            if judged.len() > 1 {
                let all: Vec<String> = judged
                    .iter()
                    .map(|(pid, sl, al, _, _)| format!("pid {pid} {sl:.1}/{al:.1}"))
                    .collect();
                detail.push_str(&format!("; per pid (slope/allowed): {}", all.join(", ")));
            }
            detail.push_str(&skipped_note);
            StepVerdict {
                name,
                pass: judged.iter().all(|j| j.1 <= j.2),
                observed: slope,
                threshold: allowed,
                detail,
            }
        })
        .collect()
}

/// Median of a non-empty slice (mean of the middle two for an even
/// length).
fn median(v: &[u64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    let m = s.len() / 2;
    if s.len() % 2 == 1 {
        s[m] as f64
    } else {
        (s[m - 1] as f64 + s[m] as f64) / 2.0
    }
}

/// The growth of one per-process counter (fds, threads) must stay
/// within `delta_max`: a steady-state process does not accumulate them.
/// Growth is `median(last 3 post-warm-up values) − median(first 3)`
/// (fewer than 3 → what exists), not `max − min`: a reconnect's worker
/// threads come and go, and a leak detector must not trip on them. The
/// peak is reported for information only. Judged per pid segment (a
/// restarted process starts its counters over); the verdict judges the
/// segment with the largest delta.
fn verdict_flat(
    groups: &Groups<'_, ProcSample>,
    prefix: &str,
    field: impl Fn(&ProcSample) -> Option<u64>,
    delta_max: u64,
) -> Vec<StepVerdict> {
    groups
        .iter()
        .map(|((leg, process), segs)| {
            let per_seg: Vec<(u32, Vec<u64>)> = segs
                .iter()
                .map(|seg| {
                    (
                        seg.pid,
                        seg.samples.iter().filter_map(|s| field(s)).collect(),
                    )
                })
                .collect();
            let name = format!("{prefix}_{leg}_{process}");
            let (judged, skipped) = judgeable(&per_seg);
            // (pid, start median, end median, peak, samples)
            let ranges: Vec<(u32, f64, f64, u64, usize)> = judged
                .iter()
                .filter_map(|(pid, v)| {
                    let k = v.len().min(3);
                    Some((
                        *pid,
                        median(&v[..k]),
                        median(&v[v.len() - k..]),
                        *v.iter().max()?,
                        v.len(),
                    ))
                })
                .collect();
            let Some(&(worst_pid, start, end, peak, n)) = ranges
                .iter()
                .max_by(|a, b| (a.2 - a.1).total_cmp(&(b.2 - b.1)))
            else {
                return StepVerdict {
                    name,
                    pass: false,
                    observed: 0.0,
                    threshold: delta_max as f64,
                    detail: format!("no post-warm-up samples{}", skipped_note(&skipped)),
                };
            };
            let delta = end - start;
            let mut detail = format!(
                "start {start}, end {end} (medians of first/last 3), peak={peak} over {n} samples \
                 of pid {worst_pid}"
            );
            if ranges.len() > 1 {
                let all: Vec<String> = ranges
                    .iter()
                    .map(|(pid, start, end, _, _)| format!("pid {pid} Δ{}", end - start))
                    .collect();
                detail.push_str(&format!("; per pid: {}", all.join(", ")));
            }
            detail.push_str(&skipped_note(&skipped));
            StepVerdict {
                name,
                pass: delta <= delta_max as f64,
                observed: delta,
                threshold: delta_max as f64,
                detail,
            }
        })
        .collect()
}

/// Post-warm-up RSS ticks observed per `(leg, process)` ÷ the ticks the
/// hold should have produced. Every declared stream's roles are
/// expected even if the sampler never wrote a row for them. This counts
/// sampler ticks, not judgeable samples: rows of every pid at or after
/// the step's warm-up count, so a restarted process's own warm-up does
/// not read as missing ticks.
fn verdict_sample_coverage(
    decl: &StepDeclaration,
    rss: &[RssSample],
    streams: &[StreamArtifacts],
) -> StepVerdict {
    let expected = decl.hold_s / decl.sample_cadence_s;
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for s in streams {
        for role in roles(s) {
            counts.insert((s.leg.clone(), role.to_string()), 0);
        }
    }
    for s in rss
        .iter()
        .filter(|s| s.elapsed_s >= decl.warmup_s && s.rss_kb.is_some())
    {
        *counts
            .entry((s.leg.clone(), s.process.clone()))
            .or_default() += 1;
    }
    let mut min_coverage = f64::INFINITY;
    let mut low = Vec::new();
    for ((leg, process), n) in &counts {
        let coverage = *n as f64 / expected;
        if coverage < MIN_SAMPLE_COVERAGE {
            low.push(format!("{leg}/{process}: {n} of {expected:.0}"));
        }
        min_coverage = min_coverage.min(coverage);
    }
    StepVerdict {
        name: "sample_coverage".into(),
        pass: min_coverage >= MIN_SAMPLE_COVERAGE,
        observed: min_coverage,
        threshold: MIN_SAMPLE_COVERAGE,
        detail: if low.is_empty() {
            format!("every process has ≥ {MIN_SAMPLE_COVERAGE} of {expected:.0} expected ticks")
        } else {
            low.join("; ")
        },
    }
}

fn read_to_string(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> Result<T, String> {
    serde_json::from_str(&read_to_string(path)?)
        .map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn run_step(
    step_dir: &std::path::Path,
    thresholds: StepThresholds,
) -> Result<StepResults, String> {
    judge_step_dir(step_dir, thresholds, Vec::new())
}

/// `run_step`'s body; `run_hold` passes its parsed restart events.
fn judge_step_dir(
    step_dir: &std::path::Path,
    thresholds: StepThresholds,
    restarts: Vec<RestartEvent>,
) -> Result<StepResults, String> {
    let decl: StepDeclaration = read_json(&step_dir.join("config.json"))?;
    let rss = parse_rss_csv(&read_to_string(&step_dir.join("rss.csv"))?)?;
    let proc = parse_proc_csv(&read_to_string(&step_dir.join("proc.csv"))?)?;
    let host = parse_host_csv(&read_to_string(&step_dir.join("host.csv"))?)?;
    let worker_exits =
        super::soak::parse_worker_exits(&read_to_string(&step_dir.join("exits.json"))?)?;
    let mut streams = Vec::with_capacity(decl.streams as usize);
    for i in 0..decl.streams {
        let sdir = step_dir.join("streams").join(i.to_string());
        let leg_path = sdir.join("leg.txt");
        let leg = if leg_path.exists() {
            read_to_string(&leg_path)?.trim().to_string()
        } else {
            format!("{}-{}", decl.transport, i)
        };
        let proxy_path = sdir.join("proxy-stats.json");
        streams.push(StreamArtifacts {
            index: i,
            leg,
            send: read_json(&sdir.join("send-report.json"))?,
            recv: read_json(&sdir.join("recv-report.json"))?,
            proxy: if proxy_path.exists() {
                Some(read_json(&proxy_path)?)
            } else {
                None
            },
        });
    }
    let results = build_step_results(StepInputs {
        decl,
        thresholds,
        rss,
        proc,
        host,
        worker_exits,
        streams,
        restarts,
    })?;
    let out = step_dir.join("step-results.json");
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&results).expect("serializes"),
    )
    .map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(results)
}

/// Nearest-rank 99th percentile.
fn p99(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (0.99 * values.len() as f64).ceil() as usize;
    Some(values[rank.saturating_sub(1)])
}

/// Per-stream figures; a restarted process's pids are folded together
/// (CPU summed per segment, p99 RSS and max threads/fds over every
/// post-warm-up sample of every pid).
fn stream_figures(
    decl: &StepDeclaration,
    proc_groups: &Groups<'_, ProcSample>,
    rss_groups: &Groups<'_, RssSample>,
    streams: &[StreamArtifacts],
) -> Vec<StreamFigures> {
    streams
        .iter()
        .map(|s| {
            let leg = s.leg.clone();
            let mut cpu_seconds = BTreeMap::new();
            let mut threads_max = BTreeMap::new();
            let mut fds_max = BTreeMap::new();
            for ((_, process), segs) in proc_groups.iter().filter(|((l, _), _)| *l == leg) {
                if let (Some(cpu_s), _) = segments_cpu_seconds(segs, decl.clk_tck) {
                    cpu_seconds.insert(process.clone(), cpu_s);
                }
                let samples = || segs.iter().flat_map(|seg| seg.samples.iter());
                if let Some(t) = samples().filter_map(|x| x.threads).max() {
                    threads_max.insert(process.clone(), t);
                }
                if let Some(f) = samples().filter_map(|x| x.fds).max() {
                    fds_max.insert(process.clone(), f);
                }
            }
            let mut rss_kb_p99 = BTreeMap::new();
            for ((_, process), segs) in rss_groups.iter().filter(|((l, _), _)| *l == leg) {
                let mut kb: Vec<u64> = segs
                    .iter()
                    .flat_map(|seg| seg.samples.iter())
                    .filter_map(|x| x.rss_kb)
                    .collect();
                if let Some(v) = p99(&mut kb) {
                    rss_kb_p99.insert(process.clone(), v);
                }
            }
            StreamFigures {
                index: s.index,
                leg,
                cpu_seconds,
                rss_kb_p99,
                threads_max,
                fds_max,
                recv_video_aus: s.recv.metrics.video_aus,
                send_video_aus: s.send.video_aus,
                wire_mbps: s.recv.metrics.bytes as f64 * 8.0 / decl.hold_s / 1e6,
                reconnects: s.recv.reconnects,
                proxy_forwarded: s.proxy.as_ref().map(|p| p.forwarded),
                proxy_dropped: s.proxy.as_ref().map(|p| p.dropped),
            }
        })
        .collect()
}

/// The step's position on its axis: `streams` for `Axis::Streams`/
/// `Axis::Hold` (the hold step is judged as a streams step — its stream
/// count is what was held), `au_scale` for `Axis::Bitrate`.
fn step_load(decl: &StepDeclaration) -> u32 {
    match decl.axis {
        Axis::Bitrate => decl.au_scale,
        Axis::Streams | Axis::Hold => decl.streams,
    }
}

fn axis_dir_name(axis: Axis) -> &'static str {
    match axis {
        Axis::Streams => "streams",
        Axis::Bitrate => "bitrate",
        Axis::Hold => "hold",
    }
}

/// One axis of the sweep (one transport × `streams` or `bitrate`), its
/// steps in ascending load order, and the ceiling rule's verdict on
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisResult {
    pub transport: String,
    pub axis: Axis,
    pub steps: Vec<StepResults>,
    pub ceiling: Option<u32>,
    pub first_fail: Option<u32>,
    pub first_fail_verdicts: Vec<String>,
}

/// The ceiling rule: `steps` must already be sorted ascending by load
/// (callers sort — this walks in order, it does not re-sort). The
/// ceiling is the load of the last PASS before the first FAIL (`None`
/// if the very first step fails); `first_fail` is the load of that
/// first FAIL (`None` if every step passes, in which case the ceiling
/// is the top of the ladder rather than a measured limit).
pub fn ceiling_of(steps: &[StepResults]) -> (Option<u32>, Option<u32>, Vec<String>) {
    let mut ceiling = None;
    for step in steps {
        if step.pass {
            ceiling = Some(step_load(&step.decl));
        } else {
            return (ceiling, Some(step_load(&step.decl)), step.failing.clone());
        }
    }
    (ceiling, None, Vec::new())
}

/// `hold/hold-config.json`, written by `stress.sh` before the 24 h
/// hold: how many streams per transport it holds, and the ceiling the
/// sweep found for each transport — declared ahead of time so the hold
/// can be checked against the sweep rather than against itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldDeclaration {
    pub n_hold: BTreeMap<String, u32>,
    pub ceilings_declared: BTreeMap<String, u32>,
    pub cpu_scale_factor: f64,
}

/// The hold step's own judged results, plus the sizing verdicts that
/// compare its declaration against the sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldResults {
    pub decl: HoldDeclaration,
    pub step: StepResults,
    pub hold_verdicts: Vec<StepVerdict>,
    pub pass: bool,
}

/// How long after a receiver restart the sender has to show a fresh
/// reconnect: the 60 s recovery budget plus one 60 s heartbeat cadence
/// (the reconnect may land just after a heartbeat).
pub const RESTART_RECOVERY_WINDOW_S: u64 = 120;

/// The hold-only verdicts, in order: `reconnect_count`,
/// `peer_restart_recovery`, `queue_depth_p99`. `send_logs` maps each
/// MANAGED stream's leg to its send log text.
pub fn hold_verdicts(
    decl: &StepDeclaration,
    step: &StepResults,
    send_logs: &BTreeMap<String, String>,
    restarts: &[RestartEvent],
    gap_capacity: u64,
    queue_depth_fraction: f64,
) -> Vec<StepVerdict> {
    let heartbeats: BTreeMap<&str, Vec<Heartbeat>> = send_logs
        .iter()
        .map(|(leg, log)| (leg.as_str(), parse_send_heartbeats(log)))
        .collect();
    vec![
        verdict_reconnect_count(decl, &step.per_stream),
        verdict_peer_restart_recovery(decl, &heartbeats, restarts),
        verdict_queue_depth_p99(&heartbeats, gap_capacity, queue_depth_fraction),
    ]
}

/// Mirrors soak: each SRT receiver must rebuild at least once per
/// outage window, less one (the last window may straddle the end).
/// Judged per stream, never on the sum, so one busy receiver cannot
/// mask one that never rebuilt: `observed` is the smallest per-stream
/// count, `threshold` is `windows − 1`, and the detail lists every
/// stream that falls short.
fn verdict_reconnect_count(decl: &StepDeclaration, per_stream: &[StreamFigures]) -> StepVerdict {
    let srt: Vec<&StreamFigures> = per_stream
        .iter()
        .filter(|s| s.leg.starts_with("srt-"))
        .collect();
    let name = "reconnect_count".to_string();
    let period = match decl.outage_period_s {
        Some(p) if p > 0 && !srt.is_empty() => p,
        _ => {
            return StepVerdict {
                name,
                pass: true,
                observed: 0.0,
                threshold: 0.0,
                detail: "not applicable: no outage schedule or no SRT stream".into(),
            };
        }
    };
    let windows = (decl.hold_s / period as f64).floor() as u64;
    let required = windows.saturating_sub(1);
    let counts: Vec<(&str, u64)> = srt
        .iter()
        .map(|s| (s.leg.as_str(), s.reconnects.unwrap_or(0)))
        .collect();
    let observed = counts.iter().map(|&(_, n)| n).min().unwrap_or(0);
    let shortfalls: Vec<String> = counts
        .iter()
        .filter(|&&(_, n)| n < required)
        .map(|&(leg, n)| format!("{leg}: {n} < {required}"))
        .collect();
    StepVerdict {
        name,
        pass: shortfalls.is_empty(),
        observed: observed as f64,
        threshold: required as f64,
        detail: if shortfalls.is_empty() {
            format!(
                "every one of {} SRT stream(s) reconnected \u{2265} {windows} outage windows − 1 = {required} times (min {observed})",
                srt.len()
            )
        } else {
            format!(
                "SRT streams below {windows} outage windows − 1 = {required} reconnects: {}",
                shortfalls.join(", ")
            )
        },
    }
}

/// Every receiver restart must be followed, within
/// `RESTART_RECOVERY_WINDOW_S`, by a sender heartbeat whose
/// `reconnects` exceeds the last value before the restart.
///
/// A restart later than `hold_s − RESTART_RECOVERY_WINDOW_S` is
/// unjudgeable: the hold ends before its recovery window does, so no
/// heartbeat can follow it. Such events are left out of both observed
/// and threshold and named in the detail. This matters most for the
/// short smoke hold, where the final restart routinely lands in the
/// last 120 s; counting it as unrecovered would fail every smoke run.
fn verdict_peer_restart_recovery(
    decl: &StepDeclaration,
    heartbeats: &BTreeMap<&str, Vec<Heartbeat>>,
    restarts: &[RestartEvent],
) -> StepVerdict {
    let name = "peer_restart_recovery".to_string();
    let window = RESTART_RECOVERY_WINDOW_S as f64;
    let (judgeable, unjudgeable): (Vec<&RestartEvent>, Vec<&RestartEvent>) = restarts
        .iter()
        .partition(|ev| ev.elapsed_s <= decl.hold_s - window);
    let unjudgeable_note = if unjudgeable.is_empty() {
        String::new()
    } else {
        format!(
            "; unjudgeable (within the final {RESTART_RECOVERY_WINDOW_S} s): {}",
            unjudgeable
                .iter()
                .map(|ev| format!("{}@{}s", ev.role, ev.elapsed_s))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if judgeable.is_empty() {
        let due = decl
            .restart_period_s
            .is_some_and(|p| decl.hold_s >= p as f64);
        let head = if due {
            "restarts were declared but none happened"
        } else {
            "not applicable: no restart was due inside the hold"
        };
        return StepVerdict {
            name,
            pass: !due,
            observed: 0.0,
            threshold: 0.0,
            detail: format!("{head}{unjudgeable_note}"),
        };
    }
    let mut unrecovered = Vec::new();
    for ev in &judgeable {
        let leg = ev.role.strip_suffix("-recv").unwrap_or(&ev.role);
        let hbs = heartbeats.get(leg).map(Vec::as_slice).unwrap_or(&[]);
        if hbs.is_empty() {
            unrecovered.push(format!(
                "{}@{}s (no heartbeats for leg {leg})",
                ev.role, ev.elapsed_s
            ));
            continue;
        }
        let before = hbs
            .iter()
            .filter(|h| h.elapsed_s as f64 <= ev.elapsed_s)
            .next_back()
            .and_then(|h| h.reconnects)
            .unwrap_or(0);
        let recovered = hbs.iter().any(|h| {
            let t = h.elapsed_s as f64;
            t > ev.elapsed_s && t <= ev.elapsed_s + window && h.reconnects.unwrap_or(0) > before
        });
        if !recovered {
            unrecovered.push(format!("{}@{}s", ev.role, ev.elapsed_s));
        }
    }
    let recovered = judgeable.len() - unrecovered.len();
    let head = if unrecovered.is_empty() {
        format!(
            "all {} judgeable restart(s) recovered within {RESTART_RECOVERY_WINDOW_S} s",
            judgeable.len()
        )
    } else {
        format!(
            "not recovered within {RESTART_RECOVERY_WINDOW_S} s: {}",
            unrecovered.join(", ")
        )
    };
    StepVerdict {
        name,
        pass: unrecovered.is_empty(),
        observed: recovered as f64,
        threshold: judgeable.len() as f64,
        detail: format!("{head}{unjudgeable_note}"),
    }
}

/// Nearest-rank p99 of `gap_len` pooled over every managed stream's
/// heartbeats must stay at or under `fraction × gap_capacity`.
fn verdict_queue_depth_p99(
    heartbeats: &BTreeMap<&str, Vec<Heartbeat>>,
    gap_capacity: u64,
    fraction: f64,
) -> StepVerdict {
    let mut gaps: Vec<u64> = heartbeats
        .values()
        .flatten()
        .filter_map(|h| h.gap_len)
        .collect();
    let threshold = fraction * gap_capacity as f64;
    match p99(&mut gaps) {
        Some(v) => StepVerdict {
            name: "queue_depth_p99".into(),
            pass: v as f64 <= threshold,
            observed: v as f64,
            threshold,
            detail: format!(
                "p99 gap_len {v} over {} heartbeat(s) vs {fraction} × capacity {gap_capacity}",
                gaps.len()
            ),
        },
        None => StepVerdict {
            name: "queue_depth_p99".into(),
            pass: false,
            observed: 0.0,
            threshold,
            detail: "no managed heartbeat found".into(),
        },
    }
}

/// Judge the hold dir: the step verdicts over it (restart-aware), then
/// the hold-only verdicts
/// from `hold-config.json`, `restart-events.log` (absent = no restarts)
/// and `logs/<leg>-send.log` for every stream whose `send-report.json`
/// carries `managed_send`. Writes `hold-results.json` on success.
pub fn run_hold(
    hold_dir: &std::path::Path,
    thresholds: StepThresholds,
) -> Result<HoldResults, String> {
    let restart_path = hold_dir.join("restart-events.log");
    let restarts = if restart_path.exists() {
        parse_restart_events(&read_to_string(&restart_path)?)?
    } else {
        Vec::new()
    };
    // The step verdicts need the restarts too: `delivery_complete`
    // judges a restarted leg on its last segment only.
    let step = judge_step_dir(hold_dir, thresholds.clone(), restarts.clone())?;
    let decl: HoldDeclaration = read_json(&hold_dir.join("hold-config.json"))?;
    let mut send_logs = BTreeMap::new();
    let mut capacities = Vec::new();
    for s in &step.per_stream {
        let send: CellMetrics = read_json(
            &hold_dir
                .join("streams")
                .join(s.index.to_string())
                .join("send-report.json"),
        )?;
        if let Some(m) = send.managed_send {
            capacities.push(m.gap_buffer_capacity);
            let log = read_to_string(&hold_dir.join("logs").join(format!("{}-send.log", s.leg)))?;
            send_logs.insert(s.leg.clone(), log);
        }
    }
    let gap_capacity = capacities.iter().copied().min().unwrap_or(0);
    let mut hold_verdicts = hold_verdicts(
        &step.decl,
        &step,
        &send_logs,
        &restarts,
        gap_capacity,
        thresholds.queue_depth_fraction,
    );
    if capacities.iter().any(|&c| c != gap_capacity) {
        if let Some(q) = hold_verdicts
            .iter_mut()
            .find(|v| v.name == "queue_depth_p99")
        {
            q.detail.push_str(&format!(
                "; managed streams declared differing gap capacities {capacities:?}, judged against the minimum"
            ));
        }
    }
    let pass = step.pass && hold_verdicts.iter().all(|v| v.pass);
    let results = HoldResults {
        decl,
        step,
        hold_verdicts,
        pass,
    };
    let out = hold_dir.join("hold-results.json");
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&results).expect("serializes"),
    )
    .map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(results)
}

/// The whole stress harness's verdict: one `AxisResult` per
/// transport/axis, the optional hold, folded into a single pass/fail
/// plus the reasons (if any) a ceiling should be read as provisional
/// rather than a measured limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StressResults {
    pub sweep: Vec<AxisResult>,
    pub hold: Option<HoldResults>,
    pub overall_pass: bool,
    pub limitations: Vec<String>,
}

/// max(1, floor(0.7 × ceiling)) — the hold must run well inside the
/// sweep's found ceiling, not at its edge; a ceiling of 1 still allows
/// a 1-stream hold (as `stress.sh` sizes it), rather than none.
fn max_hold_for_ceiling(ceiling: u32) -> u32 {
    ((0.7 * ceiling as f64).floor() as u32).max(1)
}

/// Fold the sweep's `AxisResult`s and the optional hold into one
/// verdict. Appends a `hold_sizing_declared` verdict to
/// `hold.hold_verdicts` (and folds its failure into `hold.pass`) before
/// computing `overall_pass`.
pub fn build_stress_results(sweep: Vec<AxisResult>, hold: Option<HoldResults>) -> StressResults {
    let all_have_ceiling = sweep.iter().all(|a| a.ceiling.is_some());
    let mut limitations = Vec::new();
    for axis in &sweep {
        if axis.first_fail.is_none() {
            limitations.push(format!(
                "{}/{}: never failed — the ceiling is the top of the ladder, not a measured limit",
                axis.transport,
                axis_dir_name(axis.axis)
            ));
        }
    }

    let mut hold = hold;
    if let Some(h) = hold.as_mut() {
        let mut problems = Vec::new();
        for (transport, &n_hold) in &h.decl.n_hold {
            let declared = h.decl.ceilings_declared.get(transport).copied();
            let computed = sweep
                .iter()
                .find(|a| &a.transport == transport && a.axis == Axis::Streams)
                .and_then(|a| a.ceiling);
            match (declared, computed) {
                (Some(d), Some(c)) if d == c => {
                    let max_hold = max_hold_for_ceiling(c);
                    if n_hold > max_hold {
                        problems.push(format!(
                            "{transport}: n_hold {n_hold} > max(1, floor(0.7 × ceiling {c})) = {max_hold}"
                        ));
                    }
                }
                (d, c) => problems.push(format!(
                    "{transport}: declared ceiling {d:?} does not match the sweep's computed streams ceiling {c:?}"
                )),
            }
        }
        let pass = problems.is_empty();
        h.hold_verdicts.push(StepVerdict {
            name: "hold_sizing_declared".into(),
            pass,
            observed: problems.len() as f64,
            threshold: 0.0,
            detail: if pass {
                "every held transport's n_hold is \u{2264} max(1, floor(0.7 \u{d7} its declared ceiling))"
                    .into()
            } else {
                problems.join("; ")
            },
        });
        if !pass {
            h.pass = false;
        }
    }

    let overall_pass = all_have_ceiling && hold.as_ref().map(|h| h.pass).unwrap_or(true);
    StressResults {
        sweep,
        hold,
        overall_pass,
        limitations,
    }
}

/// Fold every step of the sweep plus the optional hold into
/// `outdir/stress-results.json`.
pub fn run_stress(outdir: &std::path::Path) -> Result<StressResults, String> {
    let sweep_root = outdir.join("sweep");
    let mut sweep = Vec::new();
    for transport in sorted_dirs(&sweep_root)? {
        for axis_name in sorted_dirs(&sweep_root.join(&transport))? {
            let axis = match axis_name.as_str() {
                "streams" => Axis::Streams,
                "bitrate" => Axis::Bitrate,
                other => return Err(format!("sweep/{transport}/{other}: unknown axis directory")),
            };
            let axis_dir = sweep_root.join(&transport).join(&axis_name);
            let mut loads: Vec<u32> = sorted_dirs(&axis_dir)?
                .iter()
                .map(|d| {
                    d.parse::<u32>().map_err(|_| {
                        format!(
                            "{}: step directory name must be the load integer",
                            axis_dir.join(d).display()
                        )
                    })
                })
                .collect::<Result<_, _>>()?;
            loads.sort_unstable();
            let mut steps = Vec::new();
            for load in loads {
                steps.push(read_json::<StepResults>(
                    &axis_dir.join(load.to_string()).join("step-results.json"),
                )?);
            }
            let (ceiling, first_fail, first_fail_verdicts) = ceiling_of(&steps);
            sweep.push(AxisResult {
                transport: transport.clone(),
                axis,
                steps,
                ceiling,
                first_fail,
                first_fail_verdicts,
            });
        }
    }
    if sweep.is_empty() {
        return Err(format!(
            "{}: no sweep/<transport>/<axis>/<load>/step-results.json found",
            outdir.display()
        ));
    }
    let hold_path = outdir.join("hold").join("hold-results.json");
    let hold = if hold_path.exists() {
        Some(read_json::<HoldResults>(&hold_path)?)
    } else {
        None
    };
    let results = build_stress_results(sweep, hold);
    let out = outdir.join("stress-results.json");
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&results).expect("serializes"),
    )
    .map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(results)
}

fn sorted_dirs(dir: &std::path::Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::super::soak::tests::{cell_metrics, passing_recv_report, proxy_stats};
    use super::*;

    fn decl(streams: u32, scale: u32) -> StepDeclaration {
        StepDeclaration {
            transport: "srt".into(),
            axis: Axis::Streams,
            streams,
            au_scale: scale,
            warmup_s: 60.0,
            hold_s: 600.0,
            sample_cadence_s: 30.0,
            vcpus: 8,
            clk_tck: 100,
            nominal_mbps_per_stream: 1.7 * scale as f64,
            managed: true,
            outage_period_s: None,
            outage_dur_s: None,
            restart_period_s: None,
        }
    }
    fn thresholds() -> StepThresholds {
        StepThresholds {
            rss_slope_kb_per_hour: 1024.0,
            ..StepThresholds::default()
        }
    }
    fn proc_row(t: f64, leg: &str, process: &str, u: u64, s: u64, th: u64, fd: u64) -> ProcSample {
        ProcSample {
            elapsed_s: t,
            leg: leg.into(),
            process: process.into(),
            pid: 1,
            utime_ticks: Some(u),
            stime_ticks: Some(s),
            threads: Some(th),
            fds: Some(fd),
        }
    }
    fn rss_row(t: f64, leg: &str, process: &str, kb: u64) -> super::super::soak::RssSample {
        super::super::soak::RssSample {
            elapsed_s: t,
            leg: leg.into(),
            process: process.into(),
            pid: 1,
            rss_kb: Some(kb),
        }
    }
    /// One healthy stream: flat RSS/threads/fds, `cpu_ticks_per_tick`
    /// CPU per 30 s tick per process, 20 ticks, warm-up at 60 s.
    fn healthy_stream(
        leg: &str,
        cpu_ticks_per_tick: u64,
    ) -> (Vec<ProcSample>, Vec<super::super::soak::RssSample>) {
        let mut p = Vec::new();
        let mut r = Vec::new();
        for i in 0..22u64 {
            let t = i as f64 * 30.0;
            for proc_name in ["send", "proxy", "recv"] {
                p.push(proc_row(t, leg, proc_name, i * cpu_ticks_per_tick, 0, 4, 5));
                r.push(rss_row(t, leg, proc_name, 50_000));
            }
        }
        (p, r)
    }
    fn stream_artifacts(index: u32, sent: u64, received: u64) -> StreamArtifacts {
        StreamArtifacts {
            index,
            leg: format!("srt-{index}"),
            send: cell_metrics(sent),
            recv: passing_recv_report(received),
            proxy: Some(proxy_stats(sent, 0, 0.0, None, 0)),
        }
    }
    fn inputs(streams: u32, cpu_ticks_per_tick: u64) -> StepInputs {
        let mut proc = Vec::new();
        let mut rss = Vec::new();
        let mut arts = Vec::new();
        let mut exits = BTreeMap::new();
        for i in 0..streams {
            let leg = format!("srt-{i}");
            let (p, r) = healthy_stream(&leg, cpu_ticks_per_tick);
            proc.extend(p);
            rss.extend(r);
            arts.push(stream_artifacts(i, 18_000, 18_000));
            for role in ["send", "proxy", "recv"] {
                exits.insert(format!("{leg}-{role}"), 0);
            }
        }
        StepInputs {
            decl: decl(streams, 1),
            thresholds: thresholds(),
            rss,
            proc,
            host: Vec::new(),
            worker_exits: exits,
            streams: arts,
            restarts: Vec::new(),
        }
    }

    #[test]
    fn healthy_step_passes_every_verdict() {
        let r = build_step_results(inputs(4, 30)).unwrap(); // 30 ticks/30 s = 1 % of a core per process
        assert!(r.pass, "{:?}", r.failing);
        assert!(
            r.verdicts
                .iter()
                .any(|v| v.name == "cpu_headroom" && v.pass)
        );
        assert!(
            (r.aggregate_cpu_fraction - 0.12 / 8.0).abs() < 0.005,
            "{}",
            r.aggregate_cpu_fraction
        );
        assert_eq!(r.per_stream.len(), 4);
    }

    #[test]
    fn cpu_headroom_fails_at_saturation() {
        // 2700 ticks per 30 s tick per process = 90 % of a core each;
        // 4 streams × 3 processes × 0.9 = 10.8 cores of 8 → fraction 1.35.
        let r = build_step_results(inputs(4, 2700)).unwrap();
        assert!(!r.pass);
        assert_eq!(r.failing, vec!["cpu_headroom".to_string()]);
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "cpu_headroom")
            .unwrap();
        assert!(v.observed > 1.0 && v.threshold == 0.80);
    }

    #[test]
    fn cpu_headroom_ignores_rows_with_empty_ticks() {
        let mut inp = inputs(1, 30);
        // The recv died after tick 10: the sampler writes empty fields.
        for s in inp
            .proc
            .iter_mut()
            .filter(|s| s.process == "recv" && s.elapsed_s > 300.0)
        {
            s.utime_ticks = None;
            s.stime_ticks = None;
            s.threads = None;
            s.fds = None;
        }
        inp.worker_exits.insert("srt-0-recv".into(), 143);
        let r = build_step_results(inp).unwrap();
        assert!(!r.pass);
        assert!(r.failing.contains(&"worker_exits".to_string()));
        assert!(
            r.verdicts
                .iter()
                .any(|v| v.name == "cpu_headroom" && v.pass),
            "cpu must still be computed"
        );
    }

    #[test]
    fn fd_and_thread_growth_fail_their_flat_verdicts() {
        let mut inp = inputs(1, 30);
        for (i, s) in inp
            .proc
            .iter_mut()
            .filter(|s| s.process == "send")
            .enumerate()
        {
            s.fds = Some(5 + i as u64); // +1 fd per tick
            s.threads = Some(4 + (i as u64) / 10); // +1 thread per 10 ticks → delta 2
        }
        let r = build_step_results(inp).unwrap();
        assert!(r.failing.contains(&"fd_count_flat_srt-0_send".to_string()));
        assert!(
            r.failing
                .contains(&"thread_count_flat_srt-0_send".to_string())
        );
        assert!(
            !r.failing.iter().any(|f| f.contains("proxy")),
            "untouched processes stay green"
        );
    }

    /// Overwrite srt-0/send's post-warm-up thread counts with `pattern`,
    /// each value held for an equal share of the samples.
    fn send_threads_pattern(pattern: &[u64]) -> StepInputs {
        let mut inp = inputs(1, 30);
        let warmup = inp.decl.warmup_s;
        let mut post: Vec<&mut ProcSample> = inp
            .proc
            .iter_mut()
            .filter(|s| s.process == "send" && s.elapsed_s >= warmup)
            .collect();
        let n = post.len();
        assert!(
            n >= 2 * pattern.len(),
            "need >= 2 samples per pattern value"
        );
        for (k, s) in post.iter_mut().enumerate() {
            s.threads = Some(pattern[k * pattern.len() / n]);
        }
        inp
    }

    #[test]
    fn transient_thread_spike_passes_and_names_its_peak() {
        let r = build_step_results(send_threads_pattern(&[4, 4, 7, 7, 4, 4])).unwrap();
        let v = verdict(&r, "thread_count_flat_srt-0_send");
        assert!(v.pass, "{}", v.detail);
        assert_eq!(v.observed, 0.0, "{}", v.detail);
        assert!(v.detail.contains("peak=7"), "{}", v.detail);
    }

    #[test]
    fn steady_thread_growth_fails_its_flat_verdict() {
        let r = build_step_results(send_threads_pattern(&[4, 4, 5, 5, 6, 6])).unwrap();
        let v = verdict(&r, "thread_count_flat_srt-0_send");
        assert!(!v.pass, "{}", v.detail);
        assert_eq!(v.observed, 2.0, "{}", v.detail);
    }

    #[test]
    fn rss_slope_threshold_is_scaled_for_short_steps() {
        // +100 KB per 30 s tick = 12 MB/h: fails a 1024 KB/h threshold
        // unscaled, and the 570 s judged span scales the allowance to
        // 1024 × 3600/570 ≈ 6467 KB/h — still a fail. Then a +10 KB/tick ramp (1.2 MB/h)
        // must pass the scaled 6144 KB/h allowance.
        let mut inp = inputs(1, 30);
        for (i, s) in inp
            .rss
            .iter_mut()
            .filter(|s| s.process == "send")
            .enumerate()
        {
            s.rss_kb = Some(50_000 + 100 * i as u64);
        }
        let r = build_step_results(inp).unwrap();
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "rss_slope_srt-0_send")
            .unwrap();
        assert!(!v.pass);
        assert!(
            (v.threshold - 1024.0 * 3600.0 / 570.0).abs() < 1.0,
            "allowed = 1024 × 3600/570 (judged span 60..630 s) = {}",
            v.threshold
        );

        let mut inp = inputs(1, 30);
        for (i, s) in inp
            .rss
            .iter_mut()
            .filter(|s| s.process == "send")
            .enumerate()
        {
            s.rss_kb = Some(50_000 + 10 * i as u64);
        }
        let r = build_step_results(inp).unwrap();
        assert!(
            r.verdicts
                .iter()
                .find(|v| v.name == "rss_slope_srt-0_send")
                .unwrap()
                .pass
        );
    }

    #[test]
    fn delivery_below_slack_fails() {
        let mut inp = inputs(2, 30);
        inp.streams[1] = stream_artifacts(1, 18_000, 12_000); // 0.67 < 0.7
        let r = build_step_results(inp).unwrap();
        assert!(r.failing.contains(&"delivery_complete".to_string()));
        let v = r
            .verdicts
            .iter()
            .find(|v| v.name == "delivery_complete")
            .unwrap();
        assert!((v.observed - 0.6667).abs() < 0.001);
    }

    fn verdict<'a>(r: &'a StepResults, name: &str) -> &'a StepVerdict {
        r.verdicts
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("no verdict {name}"))
    }

    #[test]
    fn proxyless_stream_is_not_expected_to_have_a_proxy() {
        // TCP: send connects straight to recv — no proxy process, no
        // proxy rows, no proxy exit status, no proxy stats.
        let mut inp = inputs(1, 30);
        inp.proc.retain(|s| s.process != "proxy");
        inp.rss.retain(|s| s.process != "proxy");
        inp.worker_exits.remove("srt-0-proxy");
        inp.streams[0].proxy = None;
        let r = build_step_results(inp).unwrap();
        assert!(r.pass, "{:?}", r.failing);
        assert!(verdict(&r, "sample_coverage").pass);
        assert!(verdict(&r, "worker_exits").pass);
        assert_eq!(r.per_stream[0].proxy_forwarded, None);
    }

    #[test]
    fn per_stream_lookups_use_the_declared_leg() {
        // The hold step mixes transports: a stream's leg comes from its
        // artifacts, not from `decl.transport`.
        let mut inp = inputs(1, 30);
        inp.decl.transport = "all".into();
        let r = build_step_results(inp).unwrap();
        assert!(r.pass, "{:?}", r.failing);
        assert_eq!(r.per_stream[0].leg, "srt-0");
        assert_eq!(r.per_stream[0].cpu_seconds.len(), 3);
    }

    #[test]
    fn missing_exit_status_fails_worker_exits() {
        let mut inp = inputs(1, 30);
        inp.worker_exits.remove("srt-0-send");
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(!v.pass);
        assert!(
            v.detail.contains("srt-0-send: no exit status recorded"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn undeclared_nonzero_exit_fails_worker_exits() {
        let mut inp = inputs(1, 30);
        inp.worker_exits.insert("srt-9-send".into(), 1);
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "worker_exits");
        assert!(!v.pass);
        assert!(
            v.detail.contains("srt-9-send: exit 1 (undeclared worker)"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn coverage_shortfall_fails_sample_coverage() {
        // Keep every other post-warm-up row of srt-0/recv: 10 of 20.
        let mut inp = inputs(1, 30);
        inp.rss.retain(|s| {
            !(s.process == "recv" && s.elapsed_s >= 60.0 && (s.elapsed_s / 30.0) as u64 % 2 == 1)
        });
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "sample_coverage");
        assert!(!v.pass);
        assert!((v.observed - 0.5).abs() < 1e-9, "{}", v.observed);
        assert!(v.detail.contains("srt-0/recv: 10 of 20"), "{}", v.detail);
    }

    #[test]
    fn one_post_warmup_rss_row_is_insufficient() {
        let mut inp = inputs(1, 30);
        inp.rss
            .retain(|s| !(s.process == "send" && s.elapsed_s >= 60.0 && s.elapsed_s != 60.0));
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "rss_slope_srt-0_send");
        assert!(!v.pass);
        assert!(
            v.detail.contains("insufficient samples (1)"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn cpu_headroom_fails_without_a_measurement() {
        // Every proc row before warm-up: the window is empty.
        let mut inp = inputs(1, 30);
        inp.proc.retain(|s| s.elapsed_s < 60.0);
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "cpu_headroom");
        assert!(!v.pass);
        assert!(v.detail.contains("window is empty"), "{}", v.detail);
        assert_eq!(r.aggregate_cpu_fraction, 0.0);

        // A window, but no process with two rows carrying both tick
        // fields: 0 % would be a measurement of nothing.
        let mut inp = inputs(1, 30);
        for s in inp.proc.iter_mut() {
            s.stime_ticks = None;
        }
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "cpu_headroom");
        assert!(!v.pass);
        assert!(
            v.detail.contains("no process has 2 usable tick samples"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn sender_with_zero_aus_fails_delivery_with_one_note() {
        let mut inp = inputs(2, 30);
        inp.streams[1] = stream_artifacts(1, 0, 0);
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "delivery_complete");
        assert!(!v.pass);
        assert_eq!(v.observed, 0.0);
        assert_eq!(v.detail, "srt-1: sender reported 0 video AUs");
    }

    #[test]
    fn rss_threshold_must_be_positive() {
        let mut inp = inputs(1, 30);
        inp.thresholds.rss_slope_kb_per_hour = 0.0;
        assert!(
            build_step_results(inp)
                .unwrap_err()
                .contains("rss_slope_kb_per_hour")
        );
    }

    #[test]
    fn proc_csv_parses_rows_and_empty_fields() {
        let text = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n\
                    0,srt-0,send,100,12,3,4,5\n\
                    30,srt-0,send,100,,,,\n";
        let rows = parse_proc_csv(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].utime_ticks, Some(12));
        assert_eq!(rows[0].fds, Some(5));
        assert_eq!(rows[1].utime_ticks, None);
        assert_eq!(rows[1].leg, "srt-0");
    }

    #[test]
    fn proc_csv_rejects_wrong_header_and_short_rows() {
        assert!(
            parse_proc_csv("elapsed_s,leg\n")
                .unwrap_err()
                .contains("line 1")
        );
        let bad = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n0,srt,send,1,2\n";
        assert!(parse_proc_csv(bad).unwrap_err().contains("line 2"));
    }

    #[test]
    fn proc_csv_rejects_non_numeric_field() {
        let bad = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n\
                   0,srt-0,send,100,abc,3,4,5\n";
        let err = parse_proc_csv(bad).unwrap_err();
        assert!(
            err.contains("line 2") && err.contains("utime_ticks"),
            "{err}"
        );
    }

    fn write(dir: &std::path::Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn temp_step_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tst-stress-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn write_healthy_step(dir: &std::path::Path, streams: u32) {
        let inp = inputs(streams, 30);
        write(
            dir,
            "config.json",
            &serde_json::to_string(&inp.decl).unwrap(),
        );
        let mut proc_csv = format!("{PROC_HEADER}\n");
        for s in &inp.proc {
            proc_csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                s.elapsed_s,
                s.leg,
                s.process,
                s.pid,
                s.utime_ticks.unwrap(),
                s.stime_ticks.unwrap(),
                s.threads.unwrap(),
                s.fds.unwrap()
            ));
        }
        write(dir, "proc.csv", &proc_csv);
        let mut rss_csv = String::from("elapsed_s,leg,process,pid,rss_kb\n");
        for s in &inp.rss {
            rss_csv.push_str(&format!(
                "{},{},{},{},{}\n",
                s.elapsed_s,
                s.leg,
                s.process,
                s.pid,
                s.rss_kb.unwrap()
            ));
        }
        write(dir, "rss.csv", &rss_csv);
        write(
            dir,
            "host.csv",
            &format!("{HOST_HEADER}\n0,0.1,0.1,0.1,1,1000000\n"),
        );
        write(
            dir,
            "exits.json",
            &serde_json::to_string(&inp.worker_exits).unwrap(),
        );
        for a in &inp.streams {
            write(
                dir,
                &format!("streams/{}/send-report.json", a.index),
                &serde_json::to_string(&a.send).unwrap(),
            );
            write(
                dir,
                &format!("streams/{}/recv-report.json", a.index),
                &serde_json::to_string(&a.recv).unwrap(),
            );
        }
    }

    #[test]
    fn run_step_reads_a_step_dir_and_writes_results() {
        let dir = temp_step_dir("ok");
        write_healthy_step(&dir, 2);
        let r = run_step(&dir, thresholds()).unwrap();
        assert!(r.pass);
        let written: StepResults =
            serde_json::from_str(&std::fs::read_to_string(dir.join("step-results.json")).unwrap())
                .unwrap();
        assert_eq!(written.decl.streams, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn step_with_no_stream_reports_is_an_error() {
        let dir = temp_step_dir("nostreams");
        write_healthy_step(&dir, 2);
        std::fs::remove_dir_all(dir.join("streams")).unwrap();
        let err = run_step(&dir, thresholds()).unwrap_err();
        let expected = dir
            .join("streams")
            .join("0")
            .join("send-report.json")
            .display()
            .to_string();
        assert!(err.contains(&expected), "{err}");
        assert!(
            !dir.join("step-results.json").exists(),
            "no results file on error"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn step_missing_one_streams_recv_report_is_an_error() {
        let dir = temp_step_dir("onemissing");
        write_healthy_step(&dir, 2);
        std::fs::remove_file(dir.join("streams").join("1").join("recv-report.json")).unwrap();
        let expected = dir
            .join("streams")
            .join("1")
            .join("recv-report.json")
            .display()
            .to_string();
        assert!(
            run_step(&dir, thresholds())
                .unwrap_err()
                .contains(&expected)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_step_reads_leg_txt_when_present() {
        // A stream's leg.txt wins over the `<decl.transport>-<index>`
        // default: stream 0 here is "srt-7", not "srt-0" (the hold step
        // mixes transports and indices don't line up with legs there).
        let dir = temp_step_dir("legtxt");
        let inp = inputs(1, 30);
        write(
            &dir,
            "config.json",
            &serde_json::to_string(&inp.decl).unwrap(),
        );
        let (proc, rss) = healthy_stream("srt-7", 30);
        let mut proc_csv = format!("{PROC_HEADER}\n");
        for s in &proc {
            proc_csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                s.elapsed_s,
                s.leg,
                s.process,
                s.pid,
                s.utime_ticks.unwrap(),
                s.stime_ticks.unwrap(),
                s.threads.unwrap(),
                s.fds.unwrap()
            ));
        }
        write(&dir, "proc.csv", &proc_csv);
        let mut rss_csv = String::from("elapsed_s,leg,process,pid,rss_kb\n");
        for s in &rss {
            rss_csv.push_str(&format!(
                "{},{},{},{},{}\n",
                s.elapsed_s,
                s.leg,
                s.process,
                s.pid,
                s.rss_kb.unwrap()
            ));
        }
        write(&dir, "rss.csv", &rss_csv);
        write(
            &dir,
            "host.csv",
            &format!("{HOST_HEADER}\n0,0.1,0.1,0.1,1,1000000\n"),
        );
        let mut exits = BTreeMap::new();
        for role in ["send", "proxy", "recv"] {
            exits.insert(format!("srt-7-{role}"), 0);
        }
        write(&dir, "exits.json", &serde_json::to_string(&exits).unwrap());
        write(&dir, "streams/0/leg.txt", "srt-7\n");
        write(
            &dir,
            "streams/0/send-report.json",
            &serde_json::to_string(&cell_metrics(18_000)).unwrap(),
        );
        write(
            &dir,
            "streams/0/recv-report.json",
            &serde_json::to_string(&passing_recv_report(18_000)).unwrap(),
        );

        let r = run_step(&dir, thresholds()).unwrap();
        assert!(r.pass, "{:?}", r.failing);
        assert_eq!(r.per_stream[0].leg, "srt-7");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rss_csv_accepts_per_stream_legs_and_empty_rss() {
        // Unlike super::soak::parse_rss_csv, the stress-local parser has
        // no KNOWN_LEGS allowlist: "tcp-12" is a per-stream leg, not one
        // of soak.sh's two fixed legs.
        let text = "elapsed_s,leg,process,pid,rss_kb\n30,tcp-12,send,100,\n";
        let rows = parse_rss_csv(text).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].leg, "tcp-12");
        assert_eq!(rows[0].rss_kb, None);
    }

    #[test]
    fn host_csv_parses_load_and_memory() {
        let text = "elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n\
                    30,0.50,0.40,0.30,2,1000000\n60,,,,,\n";
        let rows = parse_host_csv(text).unwrap();
        assert_eq!(rows[0].load1, Some(0.5));
        assert_eq!(rows[0].mem_available_kb, Some(1_000_000));
        assert_eq!(rows[1].load1, None);
    }

    fn step_result(load: u32, axis: Axis, pass: bool, failing: &[&str]) -> StepResults {
        let mut d = decl(load, 1);
        d.axis = axis;
        if matches!(axis, Axis::Bitrate) {
            d.streams = 1;
            d.au_scale = load;
        }
        StepResults {
            decl: d,
            thresholds: thresholds(),
            pass,
            verdicts: Vec::new(),
            failing: failing.iter().map(|s| s.to_string()).collect(),
            per_stream: Vec::new(),
            aggregate_cpu_fraction: 0.1,
            cpu_fraction_per_stream: 0.1 / load as f64,
            aggregate_wire_mbps: 1.7 * load as f64,
            samples_used: 20,
        }
    }

    #[test]
    fn ceiling_is_last_pass_before_first_fail() {
        let steps = vec![
            step_result(1, Axis::Streams, true, &[]),
            step_result(2, Axis::Streams, true, &[]),
            step_result(4, Axis::Streams, false, &["cpu_headroom"]),
        ];
        assert_eq!(
            ceiling_of(&steps),
            (Some(2), Some(4), vec!["cpu_headroom".to_string()])
        );
    }

    #[test]
    fn ceiling_none_when_first_step_fails() {
        let steps = vec![step_result(1, Axis::Streams, false, &["delivery_complete"])];
        assert_eq!(
            ceiling_of(&steps),
            (None, Some(1), vec!["delivery_complete".to_string()])
        );
    }

    #[test]
    fn ceiling_is_top_of_ladder_when_nothing_fails() {
        let steps = vec![
            step_result(1, Axis::Bitrate, true, &[]),
            step_result(2, Axis::Bitrate, true, &[]),
        ];
        assert_eq!(ceiling_of(&steps), (Some(2), None, vec![]));
        let r = build_stress_results(
            vec![AxisResult {
                transport: "udp".into(),
                axis: Axis::Bitrate,
                steps,
                ceiling: Some(2),
                first_fail: None,
                first_fail_verdicts: vec![],
            }],
            None,
        );
        assert!(r.overall_pass);
        assert!(
            r.limitations
                .iter()
                .any(|l| l.contains("udp/bitrate") && l.contains("top of the ladder"))
        );
    }

    #[test]
    fn hold_sizing_must_match_the_sweep() {
        let steps = vec![
            step_result(8, Axis::Streams, true, &[]),
            step_result(16, Axis::Streams, false, &["cpu_headroom"]),
        ];
        let axis = AxisResult {
            transport: "srt".into(),
            axis: Axis::Streams,
            ceiling: Some(8),
            first_fail: Some(16),
            first_fail_verdicts: vec!["cpu_headroom".into()],
            steps,
        };
        let hold = HoldResults {
            decl: HoldDeclaration {
                n_hold: [("srt".to_string(), 7)].into(),
                ceilings_declared: [("srt".to_string(), 8)].into(),
                cpu_scale_factor: 1.0,
            },
            step: step_result(7, Axis::Hold, true, &[]),
            hold_verdicts: vec![],
            pass: true,
        };
        let r = build_stress_results(vec![axis.clone()], Some(hold.clone()));
        assert!(!r.overall_pass, "7 > floor(0.7 × 8) = 5");
        let v = r
            .hold
            .unwrap()
            .hold_verdicts
            .into_iter()
            .find(|v| v.name == "hold_sizing_declared")
            .unwrap();
        assert!(!v.pass);

        let mut ok = hold;
        ok.decl.n_hold.insert("srt".into(), 5);
        let r = build_stress_results(vec![axis], Some(ok));
        assert!(r.overall_pass);
    }

    #[test]
    fn run_stress_walks_the_sweep_tree_numerically() {
        let out = temp_step_dir("tree");
        for (load, pass) in [(1u32, true), (2, true), (16, false), (4, true), (8, true)] {
            let dir = out.join("sweep/srt/streams").join(load.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let mut s = step_result(
                load,
                Axis::Streams,
                pass,
                if pass { &[] } else { &["cpu_headroom"] },
            );
            s.decl.streams = load;
            std::fs::write(
                dir.join("step-results.json"),
                serde_json::to_string(&s).unwrap(),
            )
            .unwrap();
        }
        let r = run_stress(&out).unwrap();
        assert_eq!(r.sweep.len(), 1);
        assert_eq!(
            r.sweep[0].ceiling,
            Some(8),
            "16 must sort after 8, not between 1 and 2"
        );
        assert!(out.join("stress-results.json").exists());
        std::fs::remove_dir_all(&out).unwrap();
    }

    const LOG: &str = "\
send: heartbeat elapsed_s=60 video_aus=1800 keyframes=60 klv_records=600 audio_frames=0 wire_bytes=1 reconnects=0 gap_len=3
some other line
send: heartbeat elapsed_s=120 video_aus=3600 keyframes=120 klv_records=1200 audio_frames=0 wire_bytes=2 reconnects=1 gap_len=250
send: heartbeat elapsed_s=180 video_aus=5400 keyframes=180 klv_records=1800 audio_frames=0 wire_bytes=3 reconnects=1 gap_len=0
";

    #[test]
    fn heartbeats_parse_only_heartbeat_lines() {
        let hb = parse_send_heartbeats(LOG);
        assert_eq!(hb.len(), 3);
        assert_eq!(hb[1].reconnects, Some(1));
        assert_eq!(hb[1].gap_len, Some(250));
        let legacy = parse_send_heartbeats(
            "send: heartbeat elapsed_s=60 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1\n",
        );
        assert_eq!(legacy[0].reconnects, None);
        assert_eq!(legacy[0].gap_len, None);
    }

    #[test]
    fn restart_events_parse() {
        let ev = parse_restart_events(
            "7200 RESTART role=srt-0-recv old_pid=10 new_pid=20\n14400.5 RESTART role=srt-0-recv old_pid=20 new_pid=30\n",
        )
        .unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].role, "srt-0-recv");
        assert_eq!(ev[1].elapsed_s, 14400.5);
        let err = parse_restart_events("garbage\n").unwrap_err();
        assert!(err.contains("garbage"), "{err}");
    }

    fn hold_decl() -> StepDeclaration {
        let mut d = decl(2, 1);
        d.axis = Axis::Hold;
        d.hold_s = 86_400.0;
        d.outage_period_s = Some(900);
        d.outage_dur_s = Some(30);
        d.restart_period_s = Some(7200);
        d
    }

    fn hb_verdict<'a>(v: &'a [StepVerdict], name: &str) -> &'a StepVerdict {
        v.iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("no verdict {name}"))
    }

    #[test]
    fn restart_recovered_within_window_passes() {
        let log = "send: heartbeat elapsed_s=7140 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=7 gap_len=0\n\
                   send: heartbeat elapsed_s=7260 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=8 gap_len=10\n";
        let logs = [("srt-0".to_string(), log.to_string())].into();
        let restarts = vec![RestartEvent {
            elapsed_s: 7200.0,
            role: "srt-0-recv".into(),
        }];
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &logs,
            &restarts,
            256,
            0.9,
        );
        let r = hb_verdict(&v, "peer_restart_recovery");
        assert!(r.pass, "{}", r.detail);
        assert_eq!((r.observed, r.threshold), (1.0, 1.0));
    }

    #[test]
    fn restart_without_recovery_heartbeat_fails() {
        let log = "send: heartbeat elapsed_s=7140 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=7 gap_len=0\n";
        let logs = [("srt-0".to_string(), log.to_string())].into();
        let restarts = vec![RestartEvent {
            elapsed_s: 7200.0,
            role: "srt-0-recv".into(),
        }];
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &logs,
            &restarts,
            256,
            0.9,
        );
        let r = hb_verdict(&v, "peer_restart_recovery");
        assert!(!r.pass);
        assert!(r.detail.contains("7200"), "{}", r.detail);
    }

    #[test]
    fn recovery_heartbeat_after_the_window_does_not_count() {
        // reconnects rises only at 7200 + 180 s: outside the 120 s window.
        let log = "send: heartbeat elapsed_s=7140 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=7 gap_len=0\n\
                   send: heartbeat elapsed_s=7260 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=7 gap_len=0\n\
                   send: heartbeat elapsed_s=7380 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=8 gap_len=0\n";
        let logs = [("srt-0".to_string(), log.to_string())].into();
        let restarts = vec![RestartEvent {
            elapsed_s: 7200.0,
            role: "srt-0-recv".into(),
        }];
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &logs,
            &restarts,
            256,
            0.9,
        );
        assert!(!hb_verdict(&v, "peer_restart_recovery").pass);
    }

    #[test]
    fn declared_restarts_that_never_happened_fail() {
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &BTreeMap::new(),
            &[],
            256,
            0.9,
        );
        let r = hb_verdict(&v, "peer_restart_recovery");
        assert!(!r.pass);
        assert!(r.detail.contains("none happened"), "{}", r.detail);
    }

    #[test]
    fn undeclared_restarts_are_not_applicable() {
        let mut d = hold_decl();
        d.restart_period_s = None;
        let v = hold_verdicts(
            &d,
            &step_result(2, Axis::Hold, true, &[]),
            &BTreeMap::new(),
            &[],
            256,
            0.9,
        );
        let r = hb_verdict(&v, "peer_restart_recovery");
        assert!(r.pass, "{}", r.detail);
        assert!(r.detail.contains("not applicable"), "{}", r.detail);
    }

    #[test]
    fn queue_depth_p99_uses_nearest_rank() {
        // 98 heartbeats at gap 0 and two at 255: p99 (nearest rank,
        // n=100 → rank 99) lands on the second-highest value, 255 →
        // fail against 0.9 × 256 = 230.4. With only ONE at 255, rank 99
        // would land on 0 and pass.
        let mut log = String::new();
        for i in 0..100u64 {
            let g = if i >= 98 { 255 } else { 0 };
            log.push_str(&format!(
                "send: heartbeat elapsed_s={} video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=0 gap_len={g}\n",
                60 * (i + 1)
            ));
        }
        let logs = [("srt-0".to_string(), log)].into();
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &logs,
            &[RestartEvent {
                elapsed_s: 1.0,
                role: "x-recv".into(),
            }],
            256,
            0.9,
        );
        let q = hb_verdict(&v, "queue_depth_p99");
        assert!(!q.pass);
        assert_eq!(q.observed, 255.0);
        assert!((q.threshold - 230.4).abs() < 0.01);
    }

    #[test]
    fn queue_depth_without_managed_heartbeats_fails() {
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(2, Axis::Hold, true, &[]),
            &BTreeMap::new(),
            &[],
            256,
            0.9,
        );
        let q = hb_verdict(&v, "queue_depth_p99");
        assert!(!q.pass);
        assert!(
            q.detail.contains("no managed heartbeat found"),
            "{}",
            q.detail
        );
    }

    fn figures(leg: &str, reconnects: Option<u64>) -> StreamFigures {
        StreamFigures {
            index: 0,
            leg: leg.into(),
            cpu_seconds: BTreeMap::new(),
            rss_kb_p99: BTreeMap::new(),
            threads_max: BTreeMap::new(),
            fds_max: BTreeMap::new(),
            recv_video_aus: 0,
            send_video_aus: 0,
            wire_mbps: 0.0,
            reconnects,
            proxy_forwarded: None,
            proxy_dropped: None,
        }
    }

    #[test]
    fn reconnect_count_counts_srt_streams_only() {
        // 86_400 / 900 = 96 windows: each SRT stream needs ≥ 95. The
        // RIST stream's 0 is not judged.
        let mut step = step_result(3, Axis::Hold, true, &[]);
        step.per_stream = vec![
            figures("srt-0", Some(96)),
            figures("srt-1", Some(95)),
            figures("rist-0", Some(0)),
        ];
        let v = hold_verdicts(&hold_decl(), &step, &BTreeMap::new(), &[], 256, 0.9);
        let r = hb_verdict(&v, "reconnect_count");
        assert!(r.pass, "{}", r.detail);
        assert_eq!((r.observed, r.threshold), (95.0, 95.0));

        step.per_stream[1].reconnects = Some(94);
        let v = hold_verdicts(&hold_decl(), &step, &BTreeMap::new(), &[], 256, 0.9);
        let r = hb_verdict(&v, "reconnect_count");
        assert!(!r.pass);
        assert_eq!(r.observed, 94.0);
        assert!(r.detail.contains("srt-1: 94 < 95"), "{}", r.detail);

        step.per_stream[1].reconnects = None;
        let v = hold_verdicts(&hold_decl(), &step, &BTreeMap::new(), &[], 256, 0.9);
        let r = hb_verdict(&v, "reconnect_count");
        assert_eq!(r.observed, 0.0);
        assert!(r.detail.contains("srt-1: 0 < 95"), "{}", r.detail);
    }

    #[test]
    fn reconnect_count_one_busy_stream_does_not_mask_a_dead_one() {
        // The aggregate (190) would meet 2 × 95, but srt-1 never rebuilt.
        let mut step = step_result(2, Axis::Hold, true, &[]);
        step.per_stream = vec![figures("srt-0", Some(190)), figures("srt-1", Some(0))];
        let v = hold_verdicts(&hold_decl(), &step, &BTreeMap::new(), &[], 256, 0.9);
        let r = hb_verdict(&v, "reconnect_count");
        assert!(!r.pass, "{}", r.detail);
        assert_eq!((r.observed, r.threshold), (0.0, 95.0));
        assert!(r.detail.contains("srt-1: 0 < 95"), "{}", r.detail);
        assert!(!r.detail.contains("srt-0:"), "{}", r.detail);
    }

    #[test]
    fn reconnect_count_without_outages_is_not_applicable() {
        let mut d = hold_decl();
        d.outage_period_s = None;
        let mut step = step_result(1, Axis::Hold, true, &[]);
        step.per_stream = vec![figures("srt-0", None)];
        let v = hold_verdicts(&d, &step, &BTreeMap::new(), &[], 256, 0.9);
        let r = hb_verdict(&v, "reconnect_count");
        assert!(r.pass);
        assert!(r.detail.contains("not applicable"), "{}", r.detail);
    }

    /// A healthy 1-stream hold dir: `write_healthy_step` plus the
    /// hold-only artifacts. The step keeps its 600 s window (the step
    /// verdicts are calibrated to it); `config.json` gains the hold axis
    /// and a declared restart period, with no outage schedule. The one
    /// restart (300 s) sits inside the judgeable part of the window.
    fn write_healthy_hold(dir: &std::path::Path) {
        write_healthy_step(dir, 1);
        let mut d: StepDeclaration =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        d.axis = Axis::Hold;
        d.outage_period_s = None;
        d.restart_period_s = Some(7200);
        write(dir, "config.json", &serde_json::to_string(&d).unwrap());
        let hold_decl = HoldDeclaration {
            n_hold: [("srt".to_string(), 1)].into(),
            ceilings_declared: [("srt".to_string(), 2)].into(),
            cpu_scale_factor: 1.0,
        };
        write(
            dir,
            "hold-config.json",
            &serde_json::to_string(&hold_decl).unwrap(),
        );
        write(
            dir,
            "restart-events.log",
            "300 RESTART role=srt-0-recv old_pid=10 new_pid=20\n",
        );
        // The relaunched receiver's report covers only the 360 s after
        // the restart of the 660 s run: 18000 × 360/660 ≈ 9818 AUs.
        write(
            dir,
            "streams/0/recv-report.json",
            &serde_json::to_string(&passing_recv_report(9_818)).unwrap(),
        );
        write(
            dir,
            "logs/srt-0-send.log",
            "send: starting\n\
             send: heartbeat elapsed_s=240 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=7 gap_len=0\n\
             send: heartbeat elapsed_s=360 video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects=8 gap_len=10\n",
        );
        let mut send = cell_metrics(18_000);
        send.managed_send = Some(crate::report_types::ManagedSendStats {
            gap_buffer_capacity: 256,
            ..Default::default()
        });
        write(
            dir,
            "streams/0/send-report.json",
            &serde_json::to_string(&send).unwrap(),
        );
    }

    #[test]
    fn run_hold_reads_a_hold_dir_and_writes_results() {
        let dir = temp_step_dir("hold");
        write_healthy_hold(&dir);
        let r = run_hold(&dir, thresholds()).unwrap();
        let names: Vec<&str> = r.hold_verdicts.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "reconnect_count",
                "peer_restart_recovery",
                "queue_depth_p99"
            ]
        );
        assert!(r.pass, "{:?}", r.hold_verdicts);
        let rr = hb_verdict(&r.hold_verdicts, "peer_restart_recovery");
        assert_eq!((rr.observed, rr.threshold), (1.0, 1.0), "{}", rr.detail);
        let q = hb_verdict(&r.hold_verdicts, "queue_depth_p99");
        assert_eq!(q.observed, 10.0);
        assert!((q.threshold - 230.4).abs() < 0.01);
        let written: HoldResults =
            serde_json::from_str(&std::fs::read_to_string(dir.join("hold-results.json")).unwrap())
                .unwrap();
        assert!(written.pass);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_hold_without_restart_log_sees_zero_events() {
        let dir = temp_step_dir("hold-nolog");
        write_healthy_hold(&dir);
        std::fs::remove_file(dir.join("restart-events.log")).unwrap();
        let r = run_hold(&dir, thresholds()).unwrap();
        let v = hb_verdict(&r.hold_verdicts, "peer_restart_recovery");
        // hold_s 600 < restart_period_s 7200: no restart was due.
        assert!(v.pass, "{}", v.detail);
        assert_eq!(v.threshold, 0.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_hold_managed_stream_without_send_log_is_an_error() {
        let dir = temp_step_dir("hold-nosendlog");
        write_healthy_hold(&dir);
        std::fs::remove_file(dir.join("logs").join("srt-0-send.log")).unwrap();
        let err = run_hold(&dir, thresholds()).unwrap_err();
        let expected = dir
            .join("logs")
            .join("srt-0-send.log")
            .display()
            .to_string();
        assert!(err.contains(&expected), "{err}");
        assert!(!dir.join("hold-results.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_hold_malformed_restart_line_is_an_error() {
        let dir = temp_step_dir("hold-badrestart");
        write_healthy_hold(&dir);
        write(&dir, "restart-events.log", "7200 RESTART role=srt-0-recv\n");
        let err = run_hold(&dir, thresholds()).unwrap_err();
        assert!(err.contains("restart-events.log line 1"), "{err}");
        assert!(!dir.join("hold-results.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn hb_line(t: u64, reconnects: u64) -> String {
        format!(
            "send: heartbeat elapsed_s={t} video_aus=1 keyframes=1 klv_records=1 audio_frames=0 wire_bytes=1 reconnects={reconnects} gap_len=0\n"
        )
    }

    fn restart_verdict(d: &StepDeclaration, log: String, events: &[f64]) -> StepVerdict {
        let logs = [("srt-0".to_string(), log)].into();
        let restarts: Vec<RestartEvent> = events
            .iter()
            .map(|&t| RestartEvent {
                elapsed_s: t,
                role: "srt-0-recv".into(),
            })
            .collect();
        let v = hold_verdicts(
            d,
            &step_result(1, Axis::Hold, true, &[]),
            &logs,
            &restarts,
            256,
            0.9,
        );
        hb_verdict(&v, "peer_restart_recovery").clone()
    }

    #[test]
    fn restart_in_the_final_window_is_unjudgeable() {
        // hold_s 300: the event at 240 is past 300 − 120 = 180, so no
        // heartbeat can follow it; it is excluded, not failed.
        let mut d = hold_decl();
        d.hold_s = 300.0;
        d.restart_period_s = Some(60);
        let log = hb_line(30, 0) + &hb_line(90, 1) + &hb_line(210, 1);
        let r = restart_verdict(&d, log, &[60.0, 240.0]);
        assert!(r.pass, "{}", r.detail);
        assert_eq!((r.observed, r.threshold), (1.0, 1.0));
        assert!(
            r.detail.contains("unjudgeable (within the final 120 s)") && r.detail.contains("240"),
            "{}",
            r.detail
        );
    }

    #[test]
    fn only_unjudgeable_restarts_with_a_due_period_fail() {
        let mut d = hold_decl();
        d.hold_s = 300.0;
        d.restart_period_s = Some(240);
        let r = restart_verdict(&d, hb_line(210, 0), &[240.0]);
        assert!(!r.pass);
        assert!(r.detail.contains("none happened"), "{}", r.detail);
        assert!(r.detail.contains("unjudgeable"), "{}", r.detail);
    }

    #[test]
    fn recovery_window_edge_is_inclusive() {
        let at_edge = hb_line(7140, 7) + &hb_line(7320, 8);
        let r = restart_verdict(&hold_decl(), at_edge, &[7200.0]);
        assert!(r.pass, "{}", r.detail);
        let past_edge = hb_line(7140, 7) + &hb_line(7321, 8);
        let r = restart_verdict(&hold_decl(), past_edge, &[7200.0]);
        assert!(!r.pass, "{}", r.detail);
    }

    #[test]
    fn restart_on_a_leg_without_heartbeats_says_so() {
        let v = hold_verdicts(
            &hold_decl(),
            &step_result(1, Axis::Hold, true, &[]),
            &BTreeMap::new(),
            &[RestartEvent {
                elapsed_s: 7200.0,
                role: "srt-0-recv".into(),
            }],
            256,
            0.9,
        );
        let r = hb_verdict(&v, "peer_restart_recovery");
        assert!(!r.pass);
        assert!(
            r.detail.contains("no heartbeats for leg srt-0"),
            "{}",
            r.detail
        );
    }

    // --- restart-aware delivery, per-pid resources, hold sizing floor ---

    #[test]
    fn delivery_is_judged_on_a_restarted_legs_last_segment() {
        // The sender and the restart clock both run warm-up + hold =
        // 660 s from launch. srt-0's receiver was restarted at 300 s:
        // its report covers only the last 360 s, i.e. 360/660 of what
        // was sent (18000 × 360/660 ≈ 9818 AUs).
        let restarted = |recv: u64| {
            let mut inp = inputs(2, 30);
            inp.streams[0] = stream_artifacts(0, 18_000, recv);
            inp.restarts = vec![RestartEvent {
                elapsed_s: 300.0,
                role: "srt-0-recv".into(),
            }];
            build_step_results(inp).unwrap()
        };
        let r = restarted(9_818);
        let v = verdict(&r, "delivery_complete");
        assert!(v.pass, "{}", v.detail);
        assert!((v.observed - 1.0).abs() < 1e-3, "{}", v.observed);
        assert!(
            v.detail.contains("srt-0") && v.detail.contains("300") && v.detail.contains("0.5455"),
            "{}",
            v.detail
        );
        // A few hundred AUs short of the segment is still inside slack.
        let v = verdict(&restarted(9_000), "delivery_complete").clone();
        assert!(v.pass, "{}", v.detail);
        assert!((v.observed - 0.9167).abs() < 1e-3, "{}", v.observed);

        // The same receive count without the restart is ≈ 0.545 delivery.
        let mut inp = inputs(2, 30);
        inp.streams[0] = stream_artifacts(0, 18_000, 9_818);
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "delivery_complete");
        assert!(!v.pass);
        assert!((v.observed - 0.5454).abs() < 1e-3, "{}", v.observed);
    }

    #[test]
    fn delivery_above_the_upper_bound_fails() {
        // recv twice what was sent: the report did not reset across a
        // restart, or the accounting is wrong. Either way, not a pass.
        let mut inp = inputs(2, 30);
        inp.streams[1] = stream_artifacts(1, 18_000, 36_000);
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "delivery_complete");
        assert!(!v.pass, "{}", v.detail);
        assert!((v.observed - 2.0).abs() < 1e-9, "{}", v.observed);
        assert_eq!(v.threshold, DELIVERY_RATIO_MAX);
        assert!(
            v.detail.contains(
                "srt-1: recv exceeds expected — report did not reset or accounting mismatch"
            ),
            "{}",
            v.detail
        );
        // 1.2 itself is inside the bound.
        let mut inp = inputs(1, 30);
        inp.streams[0] = stream_artifacts(0, 18_000, 21_600);
        assert!(verdict(&build_step_results(inp).unwrap(), "delivery_complete").pass);
    }

    #[test]
    fn delivery_uses_the_legs_last_restart_only() {
        // Last restart at 400 s of a 660 s run: 18000 × 260/660 ≈ 7091.
        let mut inp = inputs(1, 30);
        inp.streams[0] = stream_artifacts(0, 18_000, 7_091);
        inp.restarts = vec![
            RestartEvent {
                elapsed_s: 200.0,
                role: "srt-0-recv".into(),
            },
            RestartEvent {
                elapsed_s: 400.0,
                role: "srt-0-recv".into(),
            },
            // Another leg's restart does not touch srt-0.
            RestartEvent {
                elapsed_s: 500.0,
                role: "srt-7-recv".into(),
            },
        ];
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "delivery_complete");
        assert!((v.observed - 1.0).abs() < 1e-3, "{}", v.detail);
        assert!(v.detail.contains("400"), "{}", v.detail);
    }

    /// srt-0/recv as two processes: pid 1 for t < 300 s (flat at 4
    /// threads / 5 fds), pid 2 from 300 s whose threads ramp 1→4 and
    /// fds 2→5 over its first 30 s, then flat. CPU ticks restart at 0
    /// in pid 2.
    fn restarted_recv_inputs() -> StepInputs {
        let mut inp = inputs(1, 30);
        inp.proc.retain(|s| s.process != "recv");
        inp.rss.retain(|s| s.process != "recv");
        for i in 0..22u64 {
            let t = i as f64 * 30.0;
            let (pid, ticks, th, fd) = match i {
                0..10 => (1, i * 30, 4, 5),
                10 => (2, 0, 1, 2),
                _ => (2, (i - 10) * 30, 4, 5),
            };
            let mut p = proc_row(t, "srt-0", "recv", ticks, 0, th, fd);
            p.pid = pid;
            inp.proc.push(p);
            let mut r = rss_row(t, "srt-0", "recv", 50_000);
            r.pid = pid;
            inp.rss.push(r);
        }
        inp
    }

    #[test]
    fn restarted_process_is_judged_per_pid() {
        let r = build_step_results(restarted_recv_inputs()).unwrap();
        for name in [
            "thread_count_flat_srt-0_recv",
            "fd_count_flat_srt-0_recv",
            "rss_slope_srt-0_recv",
        ] {
            let v = verdict(&r, name);
            assert!(v.pass, "{name}: {}", v.detail);
            assert!(
                v.detail.contains("pid 1") && v.detail.contains("pid 2"),
                "{name} must name both pids: {}",
                v.detail
            );
        }
        // pid 1: ticks 60..270 after warm-up = 2.1 s; pid 2: its own
        // warm-up ends at 360 s, ticks 60..330 = 2.7 s. Sum = 4.8 s.
        let cpu = r.per_stream[0].cpu_seconds["recv"];
        assert!((cpu - 4.8).abs() < 1e-9, "{cpu}");
        assert!(verdict(&r, "sample_coverage").pass);
        assert!(r.pass, "{:?}", r.failing);
    }

    #[test]
    fn per_pid_flat_verdict_still_catches_growth_inside_one_pid() {
        let mut inp = restarted_recv_inputs();
        // pid 2 leaks one fd per tick after its warm-up.
        for s in inp
            .proc
            .iter_mut()
            .filter(|s| s.process == "recv" && s.pid == 2 && s.elapsed_s >= 360.0)
        {
            s.fds = Some(5 + ((s.elapsed_s - 360.0) / 30.0) as u64);
        }
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "fd_count_flat_srt-0_recv");
        assert!(!v.pass, "{}", v.detail);
        assert!(v.detail.contains("pid 2"), "{}", v.detail);
    }

    #[test]
    fn segment_too_short_to_judge_is_skipped_and_named() {
        let mut inp = restarted_recv_inputs();
        // pid 3 appears at 600 s: still inside its own warm-up at the
        // end of the step, so it has no usable samples.
        for t in [600.0, 630.0] {
            let mut p = proc_row(t, "srt-0", "recv", 0, 0, 1, 2);
            p.pid = 3;
            inp.proc.push(p);
            let mut r = rss_row(t, "srt-0", "recv", 10);
            r.pid = 3;
            inp.rss.push(r);
        }
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "thread_count_flat_srt-0_recv");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("pid 3"), "{}", v.detail);
        let v = verdict(&r, "rss_slope_srt-0_recv");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("pid 3"), "{}", v.detail);
    }

    #[test]
    fn hold_sizing_floor_is_one() {
        assert_eq!(max_hold_for_ceiling(1), 1);
        assert_eq!(max_hold_for_ceiling(2), 1);
        assert_eq!(max_hold_for_ceiling(8), 5);
        let axis = AxisResult {
            transport: "tcp".into(),
            axis: Axis::Streams,
            steps: vec![
                step_result(1, Axis::Streams, true, &[]),
                step_result(2, Axis::Streams, false, &["cpu_headroom"]),
            ],
            ceiling: Some(1),
            first_fail: Some(2),
            first_fail_verdicts: vec!["cpu_headroom".into()],
        };
        let hold = HoldResults {
            decl: HoldDeclaration {
                n_hold: [("tcp".to_string(), 1)].into(),
                ceilings_declared: [("tcp".to_string(), 1)].into(),
                cpu_scale_factor: 1.0,
            },
            step: step_result(1, Axis::Hold, true, &[]),
            hold_verdicts: vec![],
            pass: true,
        };
        let r = build_stress_results(vec![axis], Some(hold));
        let v = r
            .hold
            .as_ref()
            .unwrap()
            .hold_verdicts
            .iter()
            .find(|v| v.name == "hold_sizing_declared")
            .unwrap();
        assert!(v.pass, "{}", v.detail);
        assert!(r.overall_pass);
    }

    #[test]
    fn run_hold_scales_a_restarted_legs_delivery() {
        // The healthy hold restarts srt-0's receiver at 300 s of a
        // 660 s run; the new receiver's report holds only the last
        // 360 s of AUs (`write_healthy_hold` writes 9818).
        let dir = temp_step_dir("hold-restart-delivery");
        write_healthy_hold(&dir);
        let r = run_hold(&dir, thresholds()).unwrap();
        let v = verdict(&r.step, "delivery_complete");
        assert!(v.pass, "{}", v.detail);
        assert!((v.observed - 1.0).abs() < 1e-3, "{}", v.observed);
        // Without the restart log the same report is 0.545 delivery.
        std::fs::remove_file(dir.join("restart-events.log")).unwrap();
        let r = run_hold(&dir, thresholds()).unwrap();
        assert!(!verdict(&r.step, "delivery_complete").pass);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// srt-0/recv over a 3600 s hold: pid 1 judged 60..450 s (a 390 s
    /// segment) rising `rise_kb` linearly, then pid 2 from 480 s, flat.
    fn segmented_rss_inputs(rise_kb: u64) -> StepInputs {
        let mut inp = inputs(1, 30);
        inp.decl.hold_s = 3600.0;
        inp.rss.retain(|s| s.process != "recv");
        for i in 0..123u64 {
            let t = i as f64 * 30.0;
            let (pid, kb) = if t < 480.0 {
                let judged = (t - 60.0).max(0.0);
                (1, 50_000 + rise_kb * judged as u64 / 390)
            } else {
                (2, 50_000)
            };
            let mut r = rss_row(t, "srt-0", "recv", kb);
            r.pid = pid;
            inp.rss.push(r);
        }
        inp
    }

    #[test]
    fn rss_allowance_scales_per_segment_span() {
        // +600 KB over pid 1's 390 s segment ≈ 5538 KB/h: inside
        // 1024 × 3600/390 ≈ 9452 KB/h for that segment, although the
        // 3600 s hold itself would allow only the unscaled 1024.
        let r = build_step_results(segmented_rss_inputs(600)).unwrap();
        let v = verdict(&r, "rss_slope_srt-0_recv");
        assert!(v.pass, "{}", v.detail);
        assert!(
            v.detail.contains("pid 1") && v.detail.contains("pid 2"),
            "{}",
            v.detail
        );

        // The same rate held over the full hold by one pid is judged
        // against the unscaled threshold and fails.
        let mut inp = inputs(1, 30);
        inp.decl.hold_s = 3600.0;
        inp.rss.retain(|s| s.process != "recv");
        for i in 0..123u64 {
            let t = i as f64 * 30.0;
            inp.rss
                .push(rss_row(t, "srt-0", "recv", 50_000 + (t as u64) * 600 / 390));
        }
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "rss_slope_srt-0_recv");
        assert!(!v.pass, "{}", v.detail);
        assert_eq!(v.threshold, 1024.0);
    }

    #[test]
    fn rss_segment_shorter_than_two_cadences_is_skipped() {
        // pid 3 judged at 3540 and 3570 s only (span 30 s < 2 × 30 s):
        // skipped and named, even though it carries a steep rise.
        let mut inp = segmented_rss_inputs(0);
        inp.rss
            .retain(|s| !(s.process == "recv" && s.elapsed_s >= 3480.0));
        for (t, kb) in [(3480.0, 1_000), (3540.0, 1_000), (3570.0, 9_000)] {
            let mut r = rss_row(t, "srt-0", "recv", kb);
            r.pid = 3;
            inp.rss.push(r);
        }
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "rss_slope_srt-0_recv");
        assert!(v.pass, "{}", v.detail);
        assert!(v.detail.contains("pid 3"), "{}", v.detail);

        // As the ONLY segment, a short span is insufficient.
        let mut inp = inputs(1, 30);
        inp.rss
            .retain(|s| !(s.process == "send" && s.elapsed_s > 90.0));
        let r = build_step_results(inp).unwrap();
        let v = verdict(&r, "rss_slope_srt-0_send");
        assert!(!v.pass);
        assert!(v.detail.contains("insufficient samples"), "{}", v.detail);
    }
}
