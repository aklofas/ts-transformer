//! The `rss.csv` decoder and the RSS-trend rules the soak, stress and
//! corpus-replay judges share.

use serde::{Deserialize, Serialize};

use super::csv::{RSS_HEADER, csv_rows};

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

/// Fraction of the cadence-implied post-warmup sample count a
/// process must actually have.
pub(crate) const MIN_SAMPLE_COVERAGE: f64 = 0.9;
/// Largest tolerated gap between consecutive samples, in cadences.
pub(crate) const MAX_GAP_CADENCES: f64 = 3.0;

/// One `elapsed_s,leg,process,pid,rss_kb` row from `soak.sh`'s RSS
/// sampler. `rss_kb: None` means `/proc/<pid>/status` couldn't be
/// read at this tick — the sampler's own crash signal (see
/// [`SoakResults::process_exits`](super::soak::SoakResults::process_exits)); a healthy run never produces one
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

/// Decode an `rss.csv`: an `elapsed_s,leg,process,pid,rss_kb` header
/// line followed by one row per sample tick per process. `rss_kb` may be
/// empty (process gone at this tick). A row whose leg `leg_ok` refuses,
/// or whose `elapsed_s` is not a finite non-negative number, is an error.
/// Loudly errors (naming the offending line) on anything else — a
/// malformed or truncated artifact must never silently read as "no data,
/// every check vacuously passes."
pub fn parse_rows(text: &str, leg_ok: impl Fn(&str) -> bool) -> Result<Vec<RssSample>, String> {
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
        if !leg_ok(&leg) {
            return Err(format!("rss.csv line {line_no}: unknown leg {leg:?}"));
        }
        let process = fields[2].to_string();
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_negative_or_non_finite_elapsed_is_rejected_by_both_callers() {
        let bad = "elapsed_s,leg,process,pid,rss_kb\n-1,srt-1,recv,10,100\n";
        assert!(crate::report::stress::parse_rss_csv(bad).is_err());
        assert!(crate::report::stress::parse_rss_csv(&bad.replace("-1", "NaN")).is_err());
    }
}
