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

/// A sampler row keyed by `(leg, process)` at an elapsed time.
trait Sampled {
    fn elapsed_s(&self) -> f64;
    fn leg(&self) -> &str;
    fn process(&self) -> &str;
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
}

/// Post-warm-up samples grouped by `(leg, process)`, in input order.
fn by_process<S: Sampled>(samples: &[S], warmup_s: f64) -> BTreeMap<(String, String), Vec<&S>> {
    let mut groups: BTreeMap<(String, String), Vec<&S>> = BTreeMap::new();
    for s in samples.iter().filter(|s| s.elapsed_s() >= warmup_s) {
        groups
            .entry((s.leg().to_string(), s.process().to_string()))
            .or_default()
            .push(s);
    }
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
        thresholds.delivery_slack,
    ));
    let (cpu_verdict, cpu_fraction) =
        verdict_cpu_headroom(&decl, &proc, &proc_groups, thresholds.cpu_headroom_max);
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
    verdicts.push(verdict_sample_coverage(&decl, &rss_groups, &streams));

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
        samples_used: proc.iter().filter(|s| s.elapsed_s >= decl.warmup_s).count(),
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

/// Received ÷ sent video AUs, worst stream. A stream that sent nothing
/// scores 0: no traffic is not delivery.
fn verdict_delivery_complete(streams: &[StreamArtifacts], slack: f64) -> StepVerdict {
    let mut min_ratio = f64::INFINITY;
    let mut notes = Vec::new();
    for s in streams {
        let ratio = if s.send.video_aus == 0 {
            notes.push(format!("{}: sender reported 0 video AUs", s.leg));
            0.0
        } else {
            let ratio = s.recv.metrics.video_aus as f64 / s.send.video_aus as f64;
            if ratio < slack {
                notes.push(format!(
                    "{}: {}/{} AUs = {ratio:.4}",
                    s.leg, s.recv.metrics.video_aus, s.send.video_aus
                ));
            }
            ratio
        };
        min_ratio = min_ratio.min(ratio);
    }
    StepVerdict {
        name: "delivery_complete".into(),
        pass: min_ratio >= slack,
        observed: min_ratio,
        threshold: slack,
        detail: if notes.is_empty() {
            format!("worst stream delivered {min_ratio:.4} of sent AUs")
        } else {
            notes.join("; ")
        },
    }
}

/// Total CPU of every sampled process over the post-warm-up window, as
/// a fraction of the host's cores. Returns the verdict and the
/// fraction.
fn verdict_cpu_headroom(
    decl: &StepDeclaration,
    proc: &[ProcSample],
    groups: &BTreeMap<(String, String), Vec<&ProcSample>>,
    max: f64,
) -> (StepVerdict, f64) {
    let warm_t = proc
        .iter()
        .map(|s| s.elapsed_s)
        .filter(|&t| t >= decl.warmup_s);
    let (lo, hi) = warm_t.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), t| {
        (lo.min(t), hi.max(t))
    });
    let window_s = hi - lo;
    let mut total_cpu_s = 0.0;
    let mut usable = 0usize;
    let mut unusable = Vec::new();
    for ((leg, process), samples) in groups {
        match process_cpu_seconds(samples, decl.clk_tck) {
            Some(cpu_s) => {
                total_cpu_s += cpu_s;
                usable += 1;
            }
            None => unusable.push(format!("{leg}/{process}")),
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

/// RSS growth per process in KB/hour. A short step cannot resolve a
/// small slope from noise, so its allowance scales up by 3600/hold.
fn verdict_rss_slopes(
    decl: &StepDeclaration,
    groups: &BTreeMap<(String, String), Vec<&RssSample>>,
    threshold: f64,
) -> Vec<StepVerdict> {
    let allowed = if decl.hold_s < 3600.0 {
        threshold * 3600.0 / decl.hold_s
    } else {
        threshold
    };
    groups
        .iter()
        .map(|((leg, process), samples)| {
            let points: Vec<(f64, f64)> = samples
                .iter()
                .filter_map(|s| s.rss_kb.map(|kb| (s.elapsed_s / 3600.0, kb as f64)))
                .collect();
            let name = format!("rss_slope_{leg}_{process}");
            if points.len() < 2 {
                return StepVerdict {
                    name,
                    pass: false,
                    observed: 0.0,
                    threshold: allowed,
                    detail: format!("insufficient samples ({})", points.len()),
                };
            }
            let slope = linear_regression_slope(&points);
            StepVerdict {
                name,
                pass: slope <= allowed,
                observed: slope,
                threshold: allowed,
                detail: format!(
                    "{slope:.1} KB/h over {} samples (allowed {allowed:.1})",
                    points.len()
                ),
            }
        })
        .collect()
}

/// `max − min` of one per-process counter (fds, threads) must stay
/// within `delta_max`: a steady-state process does not accumulate them.
fn verdict_flat(
    groups: &BTreeMap<(String, String), Vec<&ProcSample>>,
    prefix: &str,
    field: impl Fn(&ProcSample) -> Option<u64>,
    delta_max: u64,
) -> Vec<StepVerdict> {
    groups
        .iter()
        .map(|((leg, process), samples)| {
            let values: Vec<u64> = samples.iter().filter_map(|s| field(s)).collect();
            let name = format!("{prefix}_{leg}_{process}");
            match (values.iter().min(), values.iter().max()) {
                (Some(&lo), Some(&hi)) => StepVerdict {
                    name,
                    pass: hi - lo <= delta_max,
                    observed: (hi - lo) as f64,
                    threshold: delta_max as f64,
                    detail: format!("min {lo}, max {hi} over {} samples", values.len()),
                },
                _ => StepVerdict {
                    name,
                    pass: false,
                    observed: 0.0,
                    threshold: delta_max as f64,
                    detail: "no post-warm-up samples".into(),
                },
            }
        })
        .collect()
}

/// Post-warm-up RSS ticks observed per `(leg, process)` ÷ the ticks the
/// hold should have produced. Every declared stream's roles are
/// expected even if the sampler never wrote a row for them.
fn verdict_sample_coverage(
    decl: &StepDeclaration,
    groups: &BTreeMap<(String, String), Vec<&RssSample>>,
    streams: &[StreamArtifacts],
) -> StepVerdict {
    let expected = decl.hold_s / decl.sample_cadence_s;
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for s in streams {
        for role in roles(s) {
            counts.insert((s.leg.clone(), role.to_string()), 0);
        }
    }
    for (key, samples) in groups {
        counts.insert(
            key.clone(),
            samples.iter().filter(|s| s.rss_kb.is_some()).count(),
        );
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

fn stream_figures(
    decl: &StepDeclaration,
    proc_groups: &BTreeMap<(String, String), Vec<&ProcSample>>,
    rss_groups: &BTreeMap<(String, String), Vec<&RssSample>>,
    streams: &[StreamArtifacts],
) -> Vec<StreamFigures> {
    streams
        .iter()
        .map(|s| {
            let leg = s.leg.clone();
            let mut cpu_seconds = BTreeMap::new();
            let mut threads_max = BTreeMap::new();
            let mut fds_max = BTreeMap::new();
            for ((_, process), samples) in proc_groups.iter().filter(|((l, _), _)| *l == leg) {
                if let Some(cpu_s) = process_cpu_seconds(samples, decl.clk_tck) {
                    cpu_seconds.insert(process.clone(), cpu_s);
                }
                if let Some(t) = samples.iter().filter_map(|x| x.threads).max() {
                    threads_max.insert(process.clone(), t);
                }
                if let Some(f) = samples.iter().filter_map(|x| x.fds).max() {
                    fds_max.insert(process.clone(), f);
                }
            }
            let mut rss_kb_p99 = BTreeMap::new();
            for ((_, process), samples) in rss_groups.iter().filter(|((l, _), _)| *l == leg) {
                let mut kb: Vec<u64> = samples.iter().filter_map(|x| x.rss_kb).collect();
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

    #[test]
    fn rss_slope_threshold_is_scaled_for_short_steps() {
        // +100 KB per 30 s tick = 12 MB/h: fails a 1024 KB/h threshold
        // unscaled, and the 10-minute step scales the allowance ×6 to
        // 6144 KB/h — still a fail. Then a +10 KB/tick ramp (1.2 MB/h)
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
            (v.threshold - 6144.0).abs() < 1.0,
            "allowed = 1024 × 3600/600 = {}",
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
        assert!(err.contains("streams/0/send-report.json"), "{err}");
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
        std::fs::remove_file(dir.join("streams/1/recv-report.json")).unwrap();
        assert!(
            run_step(&dir, thresholds())
                .unwrap_err()
                .contains("streams/1/recv-report.json")
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
}
