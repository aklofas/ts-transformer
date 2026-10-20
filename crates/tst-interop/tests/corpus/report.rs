use std::collections::BTreeMap;
use std::process::Command;

use tst_interop::corpus::replay::ReplaySummary;
use tst_interop::corpus::report::{self, ArchiveRow};
use tst_interop::corpus::subset::{Pick, ReplayPlan};
use tst_interop::corpus::sweep::SweepSummary;
use tst_interop::corpus::{PATHS, RunDir, Sha256Hex};

use crate::temp_dir_named;

fn sha(c: char) -> Sha256Hex {
    Sha256Hex(c.to_string().repeat(64))
}

fn sweep(pass: bool) -> SweepSummary {
    SweepSummary {
        schema_version: 1,
        tree: Some("0123abcd".into()),
        configs: vec!["default".into()],
        files: 12,
        identical: 10,
        changed_explained: 2,
        changed_unexplained: 0,
        new_no_baseline: 0,
        parse_failures: 0,
        invariant_failures: 0,
        unused_accepts: vec![],
        per_file: vec![],
        pass,
    }
}

fn replay(pass: bool) -> ReplaySummary {
    let mut max = BTreeMap::new();
    let mut slope = BTreeMap::new();
    for p in PATHS {
        max.insert(p.to_string(), 120_000);
        slope.insert(p.to_string(), 1.5);
    }
    ReplaySummary {
        schema_version: 1,
        plan: ReplayPlan {
            schema_version: 1,
            budget_seconds: 7200,
            paths: PATHS.iter().map(|p| p.to_string()).collect(),
            picks: vec![Pick {
                sha256: sha('a'),
                why: "largest".into(),
                prefix_seconds: Some(600),
                seconds: 600.0,
            }],
            trimmed: vec![sha('b')],
            skipped: vec![(sha('c'), "over the pick size cap".into())],
            total_seconds: 600.0,
            predicted_peak_bytes: 4096,
        },
        verdicts: vec![],
        files: 3,
        paths: PATHS.iter().map(|p| p.to_string()).collect(),
        failed: if pass { 0 } else { 1 },
        rss_max_kb_by_path: max,
        rss_slope_kb_per_hour_by_path: slope,
        pass,
    }
}

#[test]
fn passing_sweep_and_replay_give_a_passing_row_with_the_counts() {
    let row = report::archive_row(&sweep(true), Some(&replay(true)));
    assert!(row.overall_pass);
    assert_eq!(row.kind, "corpus");
    assert_eq!(row.tree.as_deref(), Some("0123abcd"));
    assert_eq!(
        (row.files, row.identical, row.changed_explained),
        (12, 10, 2)
    );
    assert_eq!(
        (row.replay_files, row.replay_paths, row.replay_failed),
        (3, 5, 0)
    );
}

#[test]
fn a_missing_replay_is_not_a_pass() {
    let row = report::archive_row(&sweep(true), None);
    assert!(!row.overall_pass);
    assert_eq!(
        (row.replay_files, row.replay_paths, row.replay_failed),
        (0, 0, 0)
    );
}

#[test]
fn a_failing_half_fails_the_row() {
    assert!(!report::archive_row(&sweep(false), Some(&replay(true))).overall_pass);
    assert!(!report::archive_row(&sweep(true), Some(&replay(false))).overall_pass);
}

#[test]
fn the_note_is_the_fixed_sentence() {
    let row = report::archive_row(&sweep(true), Some(&replay(true)));
    assert_eq!(
        row.note,
        "Sweep 12 files: 10 identical, 2 changed and explained, 0 invariant failures, \
         0 parse failures; replay 3 files over 5 paths, 0 failed."
    );
}

#[test]
fn the_markdown_names_no_path_and_no_capture_file() {
    let md = report::markdown(&sweep(true), Some(&replay(true)));
    assert!(!md.contains('/'), "{md}");
    assert!(!md.contains(".ts"), "{md}");
    assert!(md.contains(&"b".repeat(64)));
    assert!(md.contains(&"c".repeat(64)));
    for p in PATHS {
        assert!(md.contains(p), "{p}");
    }
    let no_replay = report::markdown(&sweep(true), None);
    assert!(no_replay.contains("No replay"), "{no_replay}");
}

#[test]
fn the_markdown_is_deterministic() {
    let a = report::markdown(&sweep(true), Some(&replay(true)));
    assert_eq!(a, report::markdown(&sweep(true), Some(&replay(true))));
}

fn write_run(name: &str, with_replay: bool) -> std::path::PathBuf {
    let dir = temp_dir_named(name);
    std::fs::write(
        dir.join("sweep.json"),
        serde_json::to_string_pretty(&sweep(true)).unwrap(),
    )
    .unwrap();
    if with_replay {
        std::fs::write(
            dir.join("replay.json"),
            serde_json::to_string_pretty(&replay(true)).unwrap(),
        )
        .unwrap();
    }
    dir
}

#[test]
fn the_cli_writes_an_archive_row_that_round_trips_and_prints_the_note() {
    let run = write_run("rep-cli", true);
    let row_file = run.join("row.json");
    let md_file = run.join("block.md");
    let out = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args(["corpus", "report"])
        .arg(&run)
        .arg("--archive-row")
        .arg(&row_file)
        .arg("--markdown")
        .arg(&md_file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let row: ArchiveRow =
        serde_json::from_str(&std::fs::read_to_string(&row_file).unwrap()).unwrap();
    assert_eq!(row, report::archive_row(&sweep(true), Some(&replay(true))));
    assert!(String::from_utf8_lossy(&out.stdout).contains(&row.note));
    assert!(!std::fs::read_to_string(&md_file).unwrap().is_empty());
}

#[test]
fn a_run_without_a_replay_reports_and_exits_zero_but_fails_the_row() {
    let run = write_run("rep-noreplay", false);
    let r = report::run(&RunDir { root: run }, None, None).unwrap();
    assert!(!r.overall_pass);
}

#[test]
fn a_run_without_a_sweep_is_an_error_that_names_no_path() {
    let dir = temp_dir_named("rep-nosweep");
    let err = report::run(&RunDir { root: dir.clone() }, None, None).unwrap_err();
    assert!(!err.contains(dir.to_str().unwrap()), "{err}");
    let out = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args(["corpus", "report"])
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
