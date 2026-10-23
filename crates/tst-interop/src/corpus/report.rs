//! The report stage: renders a run's `sweep.json` and `replay.json` as the
//! evidence-page block and as one archive row. Counts and hashes only; no
//! path and no capture file name appears in either.

use std::fmt::Write as _;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::PATHS;
use super::RunDir;
use super::replay::ReplaySummary;
use super::sweep::{SweepSummary, digests_fingerprint};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveRow {
    /// Always `"corpus"`.
    pub kind: String,
    pub tree: Option<String>,
    pub overall_pass: bool,
    pub files: u64,
    pub identical: u64,
    pub changed_explained: u64,
    pub invariant_failures: u64,
    pub parse_failures: u64,
    pub replay_files: u64,
    pub replay_paths: u64,
    pub replay_failed: u64,
    pub note: String,
}

/// The archive row of a run. A run without a replay is not a pass.
pub fn archive_row(sweep: &SweepSummary, replay: Option<&ReplaySummary>) -> ArchiveRow {
    let (replay_files, replay_paths, replay_failed) = replay
        .map(|r| (r.files, r.paths.len() as u64, r.failed))
        .unwrap_or((0, 0, 0));
    let note = format!(
        "Sweep {} files: {} identical, {} changed and explained, {} invariant failures, \
         {} parse failures; replay {replay_files} files over {replay_paths} paths, \
         {replay_failed} failed.",
        sweep.files,
        sweep.identical,
        sweep.changed_explained,
        sweep.invariant_failures,
        sweep.parse_failures,
    );
    ArchiveRow {
        kind: "corpus".into(),
        tree: sweep.tree.clone(),
        overall_pass: sweep.pass && replay.is_some_and(|r| r.pass),
        files: sweep.files,
        identical: sweep.identical,
        changed_explained: sweep.changed_explained,
        invariant_failures: sweep.invariant_failures,
        parse_failures: sweep.parse_failures,
        replay_files,
        replay_paths,
        replay_failed,
        note,
    }
}

/// The evidence-page block: a sweep table and a per-path replay table.
pub fn markdown(sweep: &SweepSummary, replay: Option<&ReplaySummary>) -> String {
    let mut s = String::new();
    if let Some(t) = &sweep.tree {
        let _ = writeln!(s, "Tree {t}.\n");
    }
    s.push_str("| Sweep | Count |\n| --- | ---: |\n");
    for (label, n) in [
        ("Files", sweep.files),
        ("Identical", sweep.identical),
        ("Changed and explained", sweep.changed_explained),
        ("Changed and unexplained", sweep.changed_unexplained),
        ("New without a baseline", sweep.new_no_baseline),
        ("Parse failures", sweep.parse_failures),
        ("Invariant failures", sweep.invariant_failures),
    ] {
        let _ = writeln!(s, "| {label} | {n} |");
    }
    let _ = writeln!(s, "\nSweep verdict: {}.", verdict(sweep.pass));
    let Some(r) = replay else {
        s.push_str("\nNo replay was recorded for this run.\n");
        return s;
    };
    s.push_str("\n| Replay path | Files | Failed | RSS max (kB) | RSS slope (kB per hour) |\n");
    s.push_str("| --- | ---: | ---: | ---: | ---: |\n");
    for p in PATHS.iter().filter(|p| r.paths.iter().any(|q| q == *p)) {
        let failed = r
            .verdicts
            .iter()
            .filter(|v| v.path == *p && !v.pass)
            .count();
        let files = r.verdicts.iter().filter(|v| v.path == *p).count();
        let max = r
            .rss_max_kb_by_path
            .get(*p)
            .map_or("-".into(), |v| v.to_string());
        let slope = r
            .rss_slope_kb_per_hour_by_path
            .get(*p)
            .map_or("-".into(), |v| format!("{v:.1}"));
        let _ = writeln!(s, "| {p} | {files} | {failed} | {max} | {slope} |");
    }
    s.push_str("\nRSS is recorded, not gated; the soak gates memory.\n");
    for (path, note) in &r.rss_note_by_path {
        let _ = writeln!(s, "- {path}: {note}");
    }
    let _ = writeln!(
        s,
        "\nReplayed {} files over {} paths, {} failed. Predicted peak {} bytes.",
        r.files,
        r.paths.len(),
        r.failed,
        r.plan.predicted_peak_bytes
    );
    let _ = writeln!(s, "Trimmed to fit the budget: {}.", r.plan.trimmed.len());
    for t in &r.plan.trimmed {
        let _ = writeln!(s, "- {t}");
    }
    let _ = writeln!(s, "Skipped: {}.", r.plan.skipped.len());
    for (sha, why) in &r.plan.skipped {
        let _ = writeln!(s, "- {sha}: {why}");
    }
    let _ = writeln!(s, "Replay verdict: {}.", verdict(r.pass));
    s
}

fn verdict(pass: bool) -> &'static str {
    if pass { "pass" } else { "fail" }
}

fn read_json<T: DeserializeOwned>(root: &Path, name: &str) -> Result<T, String> {
    let text = std::fs::read_to_string(root.join(name))
        .map_err(|e| format!("read {name}: {}", e.kind()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {name}: {e}"))
}

/// Reads `sweep.json` (required) and `replay.json` (optional), writes the
/// requested files and returns the row.
pub fn run(
    run: &RunDir,
    markdown_out: Option<&Path>,
    archive_out: Option<&Path>,
) -> Result<ArchiveRow, String> {
    let sweep: SweepSummary = read_json(&run.root, "sweep.json")?;
    let replay: Option<ReplaySummary> = if run.root.join("replay.json").exists() {
        Some(read_json(&run.root, "replay.json")?)
    } else {
        None
    };
    // A replay of another sweep is no replay of this one.
    let (replay, stale) = match replay {
        Some(r)
            if r.sweep_tree != sweep.tree.clone().unwrap_or_default()
                || r.sweep_digests_sha256 != digests_fingerprint(run)? =>
        {
            (None, true)
        }
        r => (r, false),
    };
    let mut row = archive_row(&sweep, replay.as_ref());
    if stale {
        row.note
            .push_str(" The replay belongs to an earlier sweep.");
    }
    if let Some(p) = markdown_out {
        let mut md = markdown(&sweep, replay.as_ref());
        if stale {
            md.push_str("The replay recorded in this run belongs to an earlier sweep.\n");
        }
        std::fs::write(p, md).map_err(|e| format!("write markdown: {}", e.kind()))?;
    }
    if let Some(p) = archive_out {
        let text = serde_json::to_string_pretty(&row).map_err(|e| format!("row json: {e}"))?;
        std::fs::write(p, text + "\n").map_err(|e| format!("write archive row: {}", e.kind()))?;
    }
    Ok(row)
}
