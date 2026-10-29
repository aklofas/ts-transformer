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
        baseline_matched: Some(12),
        klv_field_errors: 0,
        files_with_klv_field_errors: 0,
        per_file: vec![],
        pass,
    }
}

/// The fingerprint of the digest file `write_run` lays down: its name, its
/// length as u64 little-endian, then its bytes.
fn fingerprint_of_digest_files() -> Sha256Hex {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("{}.json", sha('a')).as_bytes());
    h.update((DIGEST_FILE.len() as u64).to_le_bytes());
    h.update(DIGEST_FILE);
    Sha256Hex(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

const DIGEST_FILE: &[u8] = b"{\"digest\":1}";

fn replay(pass: bool) -> ReplaySummary {
    let mut max = BTreeMap::new();
    let mut slope = BTreeMap::new();
    for p in PATHS {
        max.insert(p.to_string(), 120_000);
        slope.insert(p.to_string(), 1.5);
    }
    ReplaySummary {
        schema_version: 1,
        sweep_tree: "0123abcd".into(),
        sweep_digests_sha256: fingerprint_of_digest_files(),
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
        rss_judged_by_path: BTreeMap::new(),
        rss_note_by_path: BTreeMap::new(),
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
    std::fs::create_dir_all(dir.join("digests")).unwrap();
    std::fs::write(
        dir.join("digests").join(format!("{}.json", sha('a'))),
        DIGEST_FILE,
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

fn verdict(path: &str, pass: bool) -> tst_interop::corpus::judge::FileVerdict {
    tst_interop::corpus::judge::FileVerdict {
        sha256: sha('a'),
        path: path.into(),
        pass,
        failures: if pass { vec![] } else { vec!["short".into()] },
    }
}

#[test]
fn the_markdown_table_carries_the_per_path_counts_rss_and_the_dash_fallback() {
    let mut r = replay(false);
    r.verdicts = vec![
        verdict("srt", true),
        verdict("srt", false),
        verdict("udp", true),
    ];
    r.rss_max_kb_by_path.remove("rist");
    r.rss_slope_kb_per_hour_by_path.remove("rist");
    r.rss_max_kb_by_path.insert("srt".into(), 123_456);
    r.rss_note_by_path.insert(
        "srt".into(),
        "9000.0 KiB/h over 181 post-warm-up sample(s), above the soak's threshold 200 \
         (recorded, not gated)"
            .into(),
    );
    let md = report::markdown(&sweep(true), Some(&r));
    assert!(md.contains("| srt | 2 | 1 | 123456 | 1.5 |"), "{md}");
    assert!(md.contains("| udp | 1 | 0 | 120000 | 1.5 |"), "{md}");
    assert!(md.contains("| rist | 0 | 0 | - | - |"), "{md}");
    assert!(
        md.contains("RSS is recorded, not gated; the soak gates memory."),
        "{md}"
    );
    assert!(md.contains("- srt: 9000.0 KiB/h over 181"), "{md}");
    assert!(md.contains("Predicted peak 4096 bytes."), "{md}");
}

#[test]
fn an_rss_slope_above_the_soak_threshold_leaves_a_passing_replay_passing() {
    let mut r = replay(true);
    r.rss_slope_kb_per_hour_by_path
        .insert("rist".into(), 6250.0);
    r.rss_note_by_path.insert(
        "rist".into(),
        "6249.8 KiB/h over 181 post-warm-up sample(s), above the soak's threshold 200 \
         (recorded, not gated)"
            .into(),
    );
    let row = report::archive_row(&sweep(true), Some(&r));
    assert!(row.overall_pass, "{row:?}");
    assert_eq!(row.replay_failed, 0);
}

#[test]
fn a_corrupt_sweep_or_replay_json_exits_two_and_names_no_path() {
    for bad in ["sweep.json", "replay.json"] {
        let run = write_run("rep-corrupt", true);
        std::fs::write(run.join(bad), "{ not json").unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
            .args(["corpus", "report"])
            .arg(&run)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(bad), "{err}");
        assert!(!err.contains(run.to_str().unwrap()), "{err}");
    }
}

fn report_of(run: &std::path::Path) -> ArchiveRow {
    report::run(
        &RunDir {
            root: run.to_path_buf(),
        },
        None,
        None,
    )
    .unwrap()
}

#[test]
fn a_replay_of_another_tree_or_other_digests_is_no_replay() {
    let run = write_run("rep-stale", true);
    assert!(report_of(&run).overall_pass);

    let mut r = replay(true);
    r.sweep_tree = "ffff".into();
    std::fs::write(run.join("replay.json"), serde_json::to_string(&r).unwrap()).unwrap();
    let row = report_of(&run);
    assert!(!row.overall_pass);
    assert!(row.note.contains("earlier sweep"), "{}", row.note);
    assert_eq!(row.replay_files, 0);

    std::fs::write(
        run.join("replay.json"),
        serde_json::to_string(&replay(true)).unwrap(),
    )
    .unwrap();
    assert!(report_of(&run).overall_pass);
    std::fs::write(
        run.join("digests").join(format!("{}.json", sha('b'))),
        b"{}",
    )
    .unwrap();
    let row = report_of(&run);
    assert!(!row.overall_pass);
    assert!(row.note.contains("earlier sweep"), "{}", row.note);
}

#[test]
fn a_replay_without_the_sweep_fields_is_no_replay() {
    let run = write_run("rep-oldfields", true);
    let mut v = serde_json::to_value(replay(true)).unwrap();
    let o = v.as_object_mut().unwrap();
    o.remove("sweep_tree");
    o.remove("sweep_digests_sha256");
    std::fs::write(run.join("replay.json"), v.to_string()).unwrap();
    let row = report_of(&run);
    assert!(!row.overall_pass);
    assert!(row.note.contains("earlier sweep"), "{}", row.note);
}

/// ST 0601 field errors are reported beside the sweep table, never gated:
/// the verdict and the archive row stay those of the sweep.
#[test]
fn the_markdown_reports_st0601_field_errors_as_recorded_not_gated() {
    let mut s = sweep(true);
    s.klv_field_errors = 7;
    s.files_with_klv_field_errors = 2;
    let md = report::markdown(&s, None);
    assert!(
        md.contains("ST 0601 field errors: 7 across 2 capture(s) (recorded, not gated)"),
        "{md}"
    );
    assert!(md.contains("Sweep verdict: pass."), "{md}");
    assert_eq!(
        report::archive_row(&s, None),
        report::archive_row(&sweep(true), None)
    );
}
