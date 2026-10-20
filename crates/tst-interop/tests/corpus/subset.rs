use serde_json::json;
use tst_interop::corpus::inventory::{FileEntry, Inventory};
use tst_interop::corpus::subset::{ReplayPlan, read, select, write};
use tst_interop::corpus::{PREFIX_SECONDS, RunDir};

use crate::temp_dir_named;

fn entry(
    sha: &str,
    shape: &str,
    secs: f64,
    bytes: u64,
    codecs: &[&str],
    klv: &[&str],
) -> FileEntry {
    serde_json::from_value(json!({
        "sha256": sha,
        "paths": [],
        "bytes": bytes,
        "duration_s": secs,
        "shape": shape,
        "pcr_pid": 256,
        "programs": 1,
        "streams": [],
        "codecs": codecs,
        "klv_sets": klv,
        "largest_au_bytes": 0,
        "probe_error": null,
    }))
    .unwrap()
}

fn inv(files: Vec<FileEntry>) -> Inventory {
    Inventory {
        schema_version: 1,
        files,
    }
}

fn whys(p: &ReplayPlan) -> Vec<(String, String)> {
    p.picks
        .iter()
        .map(|k| (k.sha256.0.clone(), k.why.clone()))
        .collect()
}

#[test]
fn one_pick_per_shape_takes_the_longest() {
    let i = inv(vec![
        entry("01", "a", 100.0, 10, &["h264"], &[]),
        entry("02", "a", 300.0, 20, &["h264"], &[]),
        entry("03", "b", 50.0, 10, &["h264"], &[]),
        entry("04", "b", 50.0, 10, &["h264"], &[]),
    ]);
    let p = select(&i, &[], 7200);
    let w = whys(&p);
    // h264 appears in 4 files, so nothing is rare; "longest" is already picked.
    assert!(w.contains(&("02".into(), "shape:a".into())));
    // tie on duration goes to the smallest sha
    assert!(w.contains(&("03".into(), "shape:b".into())));
    assert!(!w.iter().any(|(s, _)| s == "01" || s == "04"));
}

#[test]
fn a_codec_seen_in_two_files_is_rare_and_both_are_picked() {
    let i = inv(vec![
        entry("01", "a", 10.0, 1, &["h264"], &[]),
        entry("02", "a", 20.0, 1, &["h264"], &[]),
        entry("03", "a", 30.0, 1, &["h264"], &[]),
        entry("04", "a", 5.0, 1, &["h264", "h265"], &[]),
        entry("05", "a", 6.0, 1, &["h264", "h265"], &[]),
    ]);
    let p = select(&i, &[], 7200);
    let w = whys(&p);
    assert!(w.contains(&("04".into(), "rare:codec:h265".into())));
    assert!(w.contains(&("05".into(), "rare:codec:h265".into())));
}

#[test]
fn the_budget_trims_shape_picks_before_rare_picks() {
    let i = inv(vec![
        entry("01", "a", 600.0, 1, &["h264", "av1"], &[]),
        entry("02", "a", 600.0, 1, &["h264", "av1"], &[]),
        entry("03", "b", 600.0, 9, &["h264"], &[]),
    ]);
    let p = select(&i, &["srt".to_string()], 1200);
    assert_eq!(p.paths, vec!["srt".to_string()]);
    let shas: Vec<_> = p.picks.iter().map(|k| k.sha256.0.as_str()).collect();
    assert_eq!(shas, vec!["01", "02"]);
    assert_eq!(p.trimmed.len(), 1);
    assert_eq!(p.trimmed[0].0, "03");
    assert_eq!(p.total_seconds, 1200.0);
}

#[test]
fn long_files_get_a_600_second_prefix() {
    let i = inv(vec![
        entry("01", "a", 4000.0, 1, &["h264"], &[]),
        entry("02", "b", 90.0, 1, &["h264"], &[]),
        entry("03", "c", 600.0, 1, &["h264"], &[]),
    ]);
    let p = select(&i, &[], 7200);
    let get = |s: &str| p.picks.iter().find(|k| k.sha256.0 == s).unwrap();
    assert_eq!(get("01").prefix_seconds, Some(PREFIX_SECONDS));
    assert_eq!(get("01").seconds, 600.0);
    assert_eq!(get("02").prefix_seconds, None);
    assert_eq!(get("02").seconds, 90.0);
    assert_eq!(get("03").prefix_seconds, None);
    assert_eq!(get("03").seconds, 600.0);
}

#[test]
fn selection_is_deterministic_across_input_order() {
    let files = vec![
        entry("01", "a", 10.0, 5, &["h264"], &["st0601"]),
        entry("02", "b", 20.0, 6, &["h265"], &[]),
        entry("03", "c", 30.0, 7, &["h264"], &["unknown"]),
        entry("04", "other", 40.0, 8, &["h264"], &[]),
        entry("05", "a", 50.0, 9, &["h264"], &[]),
        entry("06", "a", 50.0, 9, &["h264"], &[]),
    ];
    let mut rev = files.clone();
    rev.reverse();
    let a = select(&inv(files), &[], 7200);
    let b = select(&inv(rev), &[], 7200);
    assert_eq!(a, b);
    // two equal-duration files in one shape: the smallest sha wins
    assert!(whys(&a).contains(&("05".into(), "shape:a".into())));
}

#[test]
fn trimming_drops_group_three_first_then_shape_picks_from_the_end() {
    let i = inv(vec![
        entry("01", "a", 50.0, 1, &["h264"], &[]),
        entry("02", "b", 100.0, 1, &["h264"], &[]),
        entry("03", "c", 100.0, 1, &["h264"], &[]),
        entry("04", "other", 100.0, 1, &["h264"], &[]),
        entry("06", "a", 150.0, 1000, &["h264"], &[]),
        entry("07", "a", 300.0, 1, &["h264"], &[]),
    ]);
    let full = select(&i, &[], 7200);
    let order: Vec<_> = full.picks.iter().map(|k| k.why.as_str()).collect();
    assert_eq!(
        order,
        vec!["shape:a", "shape:b", "shape:c", "shape:other", "largest"]
    );
    // 750 s planned; 400 s needs three pops: largest, shape:other, shape:c
    let p = select(&i, &[], 400);
    let shas: Vec<_> = p.picks.iter().map(|k| k.sha256.0.as_str()).collect();
    assert_eq!(shas, vec!["07", "02"]);
    let dropped: Vec<_> = p.trimmed.iter().map(|k| k.0.as_str()).collect();
    assert_eq!(dropped, vec!["03", "04", "06"]);
    assert_eq!(p.total_seconds, 400.0);
}

#[test]
fn a_probe_error_is_never_picked_and_the_plan_round_trips() {
    let mut bad = entry("00", "a", 999.0, 99, &["av1"], &[]);
    bad.probe_error = Some("no PMT".into());
    let i = inv(vec![bad, entry("01", "a", 10.0, 1, &["h264"], &[])]);
    let p = select(&i, &[], 7200);
    assert!(p.picks.iter().all(|k| k.sha256.0 != "00"));
    let run = RunDir {
        root: temp_dir_named("subset-roundtrip"),
    };
    write(&p, &run).unwrap();
    let back = read(&run).unwrap();
    assert_eq!(back.picks, p.picks);
    assert_eq!(back.total_seconds, p.total_seconds);
}
