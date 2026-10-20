use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

use crate::{gen_profile_bytes, sha_of, temp_dir_named};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tst-interop"))
}

fn sweep(dir: &Path, run: &Path, extra: &[&str]) -> Output {
    bin()
        .arg("corpus")
        .arg("sweep")
        .arg(dir)
        .arg("--out")
        .arg(run)
        .args(extra)
        .output()
        .unwrap()
}

/// A directory of two distinct synthetic captures; returns it with their SHAs.
fn two_files(name: &str) -> (PathBuf, Vec<String>) {
    let dir = temp_dir_named(name);
    let a = gen_profile_bytes("baseline", 2.0);
    let b = gen_profile_bytes("baseline", 3.0);
    std::fs::write(dir.join("a.ts"), &a).unwrap();
    std::fs::write(dir.join("b.ts"), &b).unwrap();
    (dir, vec![sha_of(&a).0, sha_of(&b).0])
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn sweep_without_a_baseline_passes_and_writes_one_digest_per_file() {
    let (dir, shas) = two_files("sw-nobase");
    let run = temp_dir_named("sw-nobase-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    for sha in &shas {
        assert!(run.join("digests").join(format!("{sha}.json")).exists());
    }
    assert_eq!(std::fs::read_dir(run.join("digests")).unwrap().count(), 2);
    assert_eq!(json(&run.join("sweep.json"))["new_no_baseline"], 2);
}

#[test]
fn sweep_against_its_own_output_is_identical() {
    let (dir, _) = two_files("sw-self");
    let first = temp_dir_named("sw-self-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let second = temp_dir_named("sw-self-second");
    let base = first.join("digests");
    let out = sweep(&dir, &second, &["--baseline", base.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(json(&second.join("sweep.json"))["identical"], 2);
}

#[test]
fn a_changed_file_fails_unless_accepted_with_a_reason() {
    let (dir, shas) = two_files("sw-changed");
    let first = temp_dir_named("sw-changed-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    let stored = base.join(format!("{}.json", shas[0]));
    let mut d = json(&stored);
    let n = d["configs"]["default"]["klv_records"].as_u64().unwrap();
    d["configs"]["default"]["klv_records"] = Value::from(n + 1);
    std::fs::write(&stored, serde_json::to_string(&d).unwrap()).unwrap();

    let b = base.to_str().unwrap();
    let second = temp_dir_named("sw-changed-second");
    let out = sweep(&dir, &second, &["--baseline", b]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(json(&second.join("sweep.json"))["changed_unexplained"], 1);

    let third = temp_dir_named("sw-changed-third");
    let why = "CHANGELOG: Fixed — tst-core: a stated reason";
    let out = sweep(
        &dir,
        &third,
        &["--baseline", b, "--accept", &shas[0], "--because", why],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&third.join("sweep.json"));
    assert_eq!(s["changed_explained"], 1);
    let changed: Vec<&Value> = s["per_file"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["sha256"] == shas[0].as_str())
        .collect();
    assert_eq!(changed[0]["outcome"]["Changed"]["explained"], why);
}

#[test]
fn an_accept_without_a_because_is_a_usage_error() {
    let (dir, shas) = two_files("sw-nobecause");
    let run = temp_dir_named("sw-nobecause-run");
    let out = sweep(&dir, &run, &["--accept", &shas[0]]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn an_unused_accept_fails_the_run() {
    let (dir, shas) = two_files("sw-unused");
    let first = temp_dir_named("sw-unused-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    let second = temp_dir_named("sw-unused-second");
    let out = sweep(
        &dir,
        &second,
        &[
            "--baseline",
            base.to_str().unwrap(),
            "--accept",
            &shas[0],
            "--because",
            "no change here",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        json(&second.join("sweep.json"))["unused_accepts"][0],
        shas[0].as_str()
    );
}

#[test]
fn sweep_counts_a_pmtless_stream_as_a_parse_failure() {
    let dir = temp_dir_named("sw-junk");
    std::fs::write(dir.join("junk.ts"), [0x47, 0x1F, 0xFF, 0x10].repeat(47)).unwrap();
    let run = temp_dir_named("sw-junk-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(json(&run.join("sweep.json"))["parse_failures"], 1);
}

#[test]
fn sweep_output_names_no_path() {
    let dir = temp_dir_named("sw-nopath");
    let junk = [0x47, 0x1F, 0xFF, 0x10].repeat(47);
    std::fs::write(dir.join("junk.ts"), &junk).unwrap();
    let run = temp_dir_named("sw-nopath-run");
    let out = sweep(&dir, &run, &[]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains(&sha_of(&junk).0), "{text}");
    assert!(!text.contains("junk.ts"), "{text}");
    assert!(!text.contains(dir.to_str().unwrap()), "{text}");
}

fn assert_no_path(text: &str, dir: &Path) {
    assert!(!text.contains(dir.to_str().unwrap()), "{text}");
    for part in dir.components().filter_map(|c| c.as_os_str().to_str()) {
        if part.len() > 3 && part != "tmp" {
            assert!(!text.contains(part), "{part} leaked: {text}");
        }
    }
}

#[test]
fn a_missing_corpus_directory_exits_2_without_naming_it() {
    let dir = std::env::temp_dir().join(format!("tst-corpus-absent-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = temp_dir_named("sw-missing-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("corpus directory"), "{text}");
    assert_no_path(&text, &dir);
}

#[cfg(unix)]
#[test]
fn an_unreadable_capture_exits_2_without_naming_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir_named("sw-unreadable");
    let f = dir.join("locked.ts");
    std::fs::write(&f, gen_profile_bytes("baseline", 1.0)).unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&f).is_ok() {
        return; // running as a user the mode does not bind
    }
    let run = temp_dir_named("sw-unreadable-run");
    let out = sweep(&dir, &run, &[]);
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("file 1 of 1"), "{text}");
    assert!(!text.contains("locked.ts"), "{text}");
    assert_no_path(&text, &dir);
}

#[test]
fn a_baseline_that_is_the_runs_own_digests_directory_is_refused() {
    let (dir, _) = two_files("sw-ownbase");
    let run = temp_dir_named("sw-ownbase-run");
    let own = run.join("digests");
    let out = sweep(&dir, &run, &["--baseline", own.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("own digests directory"));
    assert!(!run.join("digests").exists());
}

#[test]
fn a_corrupt_inventory_is_an_error_not_a_rebuild() {
    let (dir, _) = two_files("sw-badinv");
    let run = temp_dir_named("sw-badinv-run");
    std::fs::write(run.join("inventory.json"), "{ not json").unwrap();
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("inventory.json"));
    assert_eq!(
        std::fs::read_to_string(run.join("inventory.json")).unwrap(),
        "{ not json"
    );
}

#[test]
fn an_invariant_failure_fails_the_sweep_until_annotated() {
    let dir = temp_dir_named("sw-invariant");
    let one = gen_profile_bytes("baseline", 2.0);
    let twice = [one.clone(), one].concat();
    std::fs::write(dir.join("restart.ts"), &twice).unwrap();
    let run = temp_dir_named("sw-invariant-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let v = json(&run.join("sweep.json"));
    assert_eq!(v["invariant_failures"], 1);
    assert_eq!(
        v["per_file"][0]["invariant_failures"][0]["rule"],
        "pts-order"
    );

    let ann = temp_dir_named("sw-invariant-ann");
    let sha = sha_of(&twice);
    std::fs::write(
        ann.join(format!("{sha}.toml")),
        "known_discontinuity = true\nreason = \"two takes joined\"\n",
    )
    .unwrap();
    let run = temp_dir_named("sw-invariant-run2");
    let out = sweep(&dir, &run, &["--annotations", ann.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(json(&run.join("sweep.json"))["invariant_failures"], 0);
}

#[test]
fn a_missing_annotations_directory_or_a_bad_annotation_stops_before_digesting() {
    let (dir, shas) = two_files("sw-badann");
    let missing = temp_dir_named("sw-badann-none").join("absent");
    let run = temp_dir_named("sw-badann-run1");
    let out = sweep(&dir, &run, &["--annotations", missing.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("--annotations"), "{text}");
    assert!(!text.contains(missing.to_str().unwrap()), "{text}");

    let ann = temp_dir_named("sw-badann-dir");
    std::fs::write(ann.join(format!("{}.toml", shas[1])), "oops = 1\n").unwrap();
    let run = temp_dir_named("sw-badann-run2");
    let out = sweep(&dir, &run, &["--annotations", ann.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains(&shas[1]) && text.contains("line 1"), "{text}");
    assert!(!run.join("digests").exists(), "no file was digested");
}
