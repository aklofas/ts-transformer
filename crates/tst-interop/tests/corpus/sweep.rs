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
    let digests = std::fs::read_dir(run.join("digests"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".json")
        })
        .count();
    assert_eq!(digests, 2);
    let mut listed = shas.clone();
    listed.sort();
    assert_eq!(
        std::fs::read_to_string(run.join("digests").join("manifest.txt")).unwrap(),
        format!("{}\n", listed.join("\n"))
    );
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
    let s = json(&second.join("sweep.json"));
    assert_eq!(s["identical"], 2);
    use sha2::{Digest, Sha256};
    let manifest = std::fs::read(base.join("manifest.txt")).unwrap();
    let want: String = Sha256::digest(&manifest)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(s["baseline_manifest_sha256"], want.as_str());
    assert_eq!(
        json(&first.join("sweep.json"))["baseline_manifest_sha256"],
        Value::Null
    );
}

/// A baseline written before a digest field existed compares identical
/// on that field: the field is taken from the current digest.
#[test]
fn a_baseline_digest_without_the_new_fields_is_identical() {
    let (dir, shas) = two_files("sw-oldfields");
    let first = temp_dir_named("sw-oldfields-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    for sha in &shas {
        let p = first.join("digests").join(format!("{sha}.json"));
        let mut v = json(&p);
        for cfg in v["configs"].as_object_mut().unwrap().values_mut() {
            for pid in cfg["pids"].as_object_mut().unwrap().values_mut() {
                let o = pid.as_object_mut().unwrap();
                o.remove("klv_truncated");
                o.remove("klv_field_errors");
            }
        }
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    }
    let second = temp_dir_named("sw-oldfields-second");
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
        "known_discontinuity = true\nmax_pts_backward_breaks = 1\ndiscontinuity_pids = [4113]\nreason = \"two takes joined\"\n",
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

#[test]
fn a_baseline_directory_that_does_not_exist_exits_2_without_naming_it() {
    let (dir, _) = two_files("sw-nobasedir");
    let absent = temp_dir_named("sw-nobasedir-base").join("absent");
    let run = temp_dir_named("sw-nobasedir-run");
    let out = sweep(&dir, &run, &["--baseline", absent.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("--baseline"), "{text}");
    assert_no_path(&text, &absent);
    assert!(!run.join("digests").exists(), "no file was digested");
}

#[test]
fn a_baseline_that_matches_no_capture_fails_the_sweep() {
    let (other, _) = two_files("sw-wrongbase-other");
    let first = temp_dir_named("sw-wrongbase-first");
    assert_eq!(sweep(&other, &first, &[]).status.code(), Some(0));
    let dir = temp_dir_named("sw-wrongbase");
    std::fs::write(dir.join("c.ts"), gen_profile_bytes("baseline", 4.0)).unwrap();
    let run = temp_dir_named("sw-wrongbase-run");
    let base = first.join("digests");
    let out = sweep(&dir, &run, &["--baseline", base.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stdout.contains("baseline matched 0 files"), "{stdout}");
    let s = json(&run.join("sweep.json"));
    assert_eq!(s["baseline_matched"], 0);
    assert_eq!(s["new_no_baseline"], 1);
    assert_eq!(s["new_unexplained"], 1);
    assert_eq!(s["baseline_only"].as_array().unwrap().len(), 2);
    assert_eq!(s["pass"], false);
}

#[test]
fn a_corpus_directory_without_captures_exits_2_without_naming_it() {
    let dir = temp_dir_named("sw-nocaptures");
    std::fs::write(dir.join("readme.txt"), b"no captures here").unwrap();
    let run = temp_dir_named("sw-nocaptures-run");
    let out = sweep(&dir, &run, &[]);
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("no captures found"), "{text}");
    assert_no_path(&text, &dir);
    assert!(!run.join("sweep.json").exists());
}

#[test]
fn a_pes_header_that_overruns_its_packet_never_crashes_the_sweep() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    let probe = tst_test_helpers::rawts::parsed(&bytes);
    let video = probe
        .streams
        .iter()
        .find(|s| s.stream_type == 0x1B)
        .unwrap()
        .pid;
    // A PES start on the video PID, inside the first 4 MiB, whose
    // PES_header_data_length claims 0xFF bytes.
    let header_len_at = (0..bytes.len())
        .step_by(188)
        .skip(10)
        .find_map(|o| {
            let p = &bytes[o..o + 188];
            let payload = match (p[3] >> 4) & 3 {
                1 => 4,
                3 => 5 + usize::from(p[4]),
                _ => return None,
            };
            let ours = (u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])) == video;
            (ours && p[1] & 0x40 != 0 && payload + 9 <= 188).then_some(o + payload + 8)
        })
        .unwrap();
    bytes[header_len_at] = 0xFF;
    assert!(tst_test_helpers::rawts::try_parsed(&bytes).is_err());
    let dir = temp_dir_named("sw-peshead");
    std::fs::write(dir.join("damaged.ts"), &bytes).unwrap();
    let run = temp_dir_named("sw-peshead-run");
    let out = sweep(&dir, &run, &[]);
    // The default pass reports the header on the video PID and goes on;
    // strict mode rejects it once. Judged, not swallowed, and no NO-GO.
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&run.join("sweep.json"));
    let d = json(&run.join("digests").join(format!("{}.json", sha_of(&bytes))));
    assert_eq!(
        d["configs"]["default"]["pids"][video.to_string()]["nonconformant"]["MalformedPes"],
        1,
        "{d}"
    );
    assert_eq!(
        d["configs"]["strict_full"]["feed_error_kinds"]["MalformedPes"], 1,
        "{d}"
    );
    assert_eq!(s["files"], 1);
    assert_eq!(s["per_file"][0]["sha256"], sha_of(&bytes).0.as_str());
    assert_eq!(s["parse_failures"], 0, "{s}");
}

#[test]
fn a_test_knob_in_the_environment_is_named_on_stderr() {
    let (dir, _) = two_files("sw-knob");
    let run = temp_dir_named("sw-knob-run");
    let out = bin()
        .arg("corpus")
        .arg("sweep")
        .arg(&dir)
        .arg("--out")
        .arg(&run)
        .env("TST_CORPUS_TEST_STALL_SECONDS", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("test knob TST_CORPUS_TEST_STALL_SECONDS is set"),
        "{err}"
    );
    assert!(!err.contains("=1"), "{err}");
}

#[test]
fn a_zero_filled_dropout_fails_feed_error_until_marked_never_a_parse_failure() {
    let mut bytes = gen_profile_bytes("baseline", 4.0);
    // 8 KiB of zeros spliced in on a packet boundary halfway in: more than
    // the demuxer's sync-search window, so its feed rejects.
    let at = bytes.len() / 188 / 2 * 188;
    bytes.splice(at..at, std::iter::repeat_n(0u8, 8192));
    let dir = temp_dir_named("sw-dropout");
    std::fs::write(dir.join("dropout.ts"), &bytes).unwrap();
    let run = temp_dir_named("sw-dropout-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let s = json(&run.join("sweep.json"));
    assert_eq!(s["parse_failures"], 0, "{s}");
    let failures = s["per_file"][0]["invariant_failures"].as_array().unwrap();
    let rules: Vec<&str> = failures
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert_eq!(rules, ["feed-error"], "{s}");
    // The reviewed count the annotation below pins as its bound.
    assert_eq!(failures[0]["detail"], "1 feed errors", "{s}");

    let ann = temp_dir_named("sw-dropout-ann");
    std::fs::write(
        ann.join(format!("{}.toml", sha_of(&bytes))),
        "known_feed_errors = true\nmax_feed_errors = 1\nreason = \"recorder dropout, reviewed\"\n",
    )
    .unwrap();
    let run = temp_dir_named("sw-dropout-run2");
    let out = sweep(&dir, &run, &["--annotations", ann.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&run.join("sweep.json"));
    assert_eq!(s["invariant_failures"], 0, "{s}");
    let f = &s["per_file"][0]["invariant_failures"][0];
    assert_eq!(f["excused"], true, "{s}");
    assert!(
        f["detail"]
            .as_str()
            .unwrap()
            .starts_with("excused: recorder dropout, reviewed;"),
        "{s}"
    );

    // A bound below what the capture shows no longer excuses it.
    std::fs::write(
        ann.join(format!("{}.toml", sha_of(&bytes))),
        "known_feed_errors = true\nmax_feed_errors = 0\nreason = \"recorder dropout, reviewed\"\n",
    )
    .unwrap();
    let run = temp_dir_named("sw-dropout-run3");
    let out = sweep(&dir, &run, &["--annotations", ann.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let s = json(&run.join("sweep.json"));
    assert_eq!(
        s["per_file"][0]["invariant_failures"][0]["detail"],
        "1 feed errors exceeds the reviewed bound 0",
        "{s}"
    );
}

#[test]
fn a_new_sweep_removes_the_replay_results_of_the_old_one() {
    let (dir, _) = two_files("sw-stale");
    let run = dir.join("run");
    assert!(sweep(&dir, &run, &[]).status.code().is_some());
    let stale = ["replay.json", "replay-rss.csv", "replay-plan.json"];
    for name in stale {
        std::fs::write(run.join(name), "old").unwrap();
    }
    assert!(sweep(&dir, &run, &[]).status.code().is_some());
    for name in stale {
        assert!(!run.join(name).exists(), "{name} survived a new sweep");
    }
    assert!(run.join("sweep.json").is_file());
}

#[test]
fn digests_fingerprint_ignores_prefix_digests() {
    use tst_interop::corpus::{RunDir, sweep::digests_fingerprint};
    let root = temp_dir_named("sw-fingerprint");
    let digests = root.join("digests");
    std::fs::create_dir_all(&digests).unwrap();
    std::fs::write(digests.join("aa.json"), b"{\"a\":1}").unwrap();
    std::fs::write(digests.join("bb.json"), b"{\"b\":2}").unwrap();
    let run = RunDir { root };
    let before = digests_fingerprint(&run).unwrap();

    std::fs::write(digests.join("aa.prefix600.json"), b"{\"p\":1}").unwrap();
    assert_eq!(digests_fingerprint(&run).unwrap(), before);

    std::fs::write(digests.join("aa.json"), b"{\"a\":2}").unwrap();
    assert_ne!(digests_fingerprint(&run).unwrap(), before);
}

#[test]
fn a_sweep_removes_the_previous_replays_prefix_digests() {
    let (dir, _) = two_files("sw-stale-prefix");
    let run = temp_dir_named("sw-stale-prefix-run");
    assert_eq!(sweep(&dir, &run, &[]).status.code(), Some(0));
    let stale = run.join("digests").join("aa.prefix600.json");
    std::fs::write(&stale, b"{}").unwrap();
    assert_eq!(sweep(&dir, &run, &[]).status.code(), Some(0));
    assert!(!stale.exists());
}

#[test]
fn a_blank_because_is_a_usage_error() {
    let (dir, shas) = two_files("sw-blankwhy");
    let run = temp_dir_named("sw-blankwhy-run");
    let out = sweep(&dir, &run, &["--accept", &shas[0], "--because", "  "]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--because needs a non-empty reason"),
        "{out:?}"
    );
}

#[test]
fn field_errors_are_summed_into_the_sweep_and_do_not_fail_it() {
    let dir = temp_dir_named("sw-fielderr");
    let payload = crate::digest::st0601_with_short_item2();
    std::fs::write(
        dir.join("a.ts"),
        crate::invariants::muxed_klv_file(&payload, 5),
    )
    .unwrap();
    let run = temp_dir_named("sw-fielderr-run");
    let out = sweep(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&run.join("sweep.json"));
    assert_eq!(s["klv_field_errors"], 5, "{s}");
    assert_eq!(s["files_with_klv_field_errors"], 1, "{s}");
}

/// A sweep summary written before the field-error counts and the baseline
/// acceptance fields existed still reads, as zero and empty.
#[test]
fn a_sweep_summary_without_the_field_error_counts_reads_as_zero() {
    let (dir, _) = two_files("sw-oldsummary");
    let run = temp_dir_named("sw-oldsummary-run");
    assert_eq!(sweep(&dir, &run, &[]).status.code(), Some(0));
    let p = run.join("sweep.json");
    let mut v = json(&p);
    let o = v.as_object_mut().unwrap();
    assert_eq!(o.remove("klv_field_errors"), Some(Value::from(0)));
    assert_eq!(o.remove("accepted"), Some(serde_json::json!([])));
    for key in [
        "new_explained",
        "new_unexplained",
        "baseline_only",
        "baseline_only_explained",
    ] {
        assert!(o.remove(key).is_some(), "{key}");
    }
    assert_eq!(
        o.remove("files_with_klv_field_errors"),
        Some(Value::from(0))
    );
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    let s: tst_interop::corpus::sweep::SweepSummary =
        serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    assert_eq!((s.klv_field_errors, s.files_with_klv_field_errors), (0, 0));
    assert!(s.accepted.is_empty() && s.baseline_only.is_empty());
    assert_eq!((s.new_explained, s.new_unexplained), (0, 0));
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn a_baseline_missing_most_digests_is_refused() {
    let (dir, shas) = two_files("sw-partial");
    let first = temp_dir_named("sw-partial-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    std::fs::remove_file(first.join("digests").join(format!("{}.json", shas[1]))).unwrap();
    let second = temp_dir_named("sw-partial-second");
    let out = sweep(
        &dir,
        &second,
        &["--baseline", first.join("digests").to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("the baseline is incomplete: 1 listed digest(s) are missing"),
        "{out:?}"
    );
    assert!(!second.join("digests").exists(), "no file was digested");
}

#[test]
fn a_capture_the_baseline_does_not_list_fails_unless_accepted() {
    let (dir, _) = two_files("sw-newcap");
    let first = temp_dir_named("sw-newcap-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let c = gen_profile_bytes("baseline", 4.0);
    std::fs::write(dir.join("c.ts"), &c).unwrap();
    let sha = sha_of(&c).0;
    let base = first.join("digests");
    let second = temp_dir_named("sw-newcap-second");
    let out = sweep(&dir, &second, &["--baseline", base.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let s = json(&second.join("sweep.json"));
    assert_eq!(s["new_unexplained"], 1, "{s}");
    assert_eq!(s["identical"], 2, "{s}");
    // The line names the capture by SHA and says how to accept it.
    assert!(
        stdout(&out).contains(&format!(
            "{sha} new-no-baseline: the baseline does not list it; accept it with --accept {sha} --because <reason>"
        )),
        "{out:?}"
    );
    assert!(!stdout(&out).contains("c.ts"), "{out:?}");
    let third = temp_dir_named("sw-newcap-third");
    let out = sweep(
        &dir,
        &third,
        &[
            "--baseline",
            base.to_str().unwrap(),
            "--accept",
            &sha,
            "--because",
            "new capture",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&third.join("sweep.json"));
    assert_eq!(s["new_explained"], 1, "{s}");
    assert_eq!(s["new_unexplained"], 0, "{s}");
    assert_eq!(
        s["accepted"],
        serde_json::json!([{"sha": sha, "kind": "new", "because": "new capture"}]),
        "{s}"
    );
    assert!(stdout(&out).contains("new (accepted): 1"), "{out:?}");
}

#[test]
fn a_baseline_capture_missing_from_the_corpus_fails_unless_accepted() {
    let (dir, shas) = two_files("sw-gone");
    let first = temp_dir_named("sw-gone-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    std::fs::remove_file(dir.join("b.ts")).unwrap();
    let base = first.join("digests");
    let second = temp_dir_named("sw-gone-second");
    let out = sweep(&dir, &second, &["--baseline", base.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let s = json(&second.join("sweep.json"));
    assert_eq!(s["baseline_only"], serde_json::json!([shas[1]]), "{s}");
    assert_eq!(s["baseline_only_explained"], 0, "{s}");
    assert!(
        stdout(&out).contains(&format!(
            "{} baseline-only: the corpus no longer holds it; accept it with --accept {} --because <reason>",
            shas[1], shas[1]
        )),
        "{out:?}"
    );
    assert!(stdout(&out).contains("baseline only: 1"), "{out:?}");
    let third = temp_dir_named("sw-gone-third");
    let out = sweep(
        &dir,
        &third,
        &[
            "--baseline",
            base.to_str().unwrap(),
            "--accept",
            &shas[1],
            "--because",
            "capture withdrawn",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let s = json(&third.join("sweep.json"));
    assert_eq!(s["baseline_only_explained"], 1, "{s}");
    assert_eq!(
        s["accepted"],
        serde_json::json!([{"sha": shas[1], "kind": "removed", "because": "capture withdrawn"}]),
        "{s}"
    );
    assert_eq!(s["unused_accepts"], serde_json::json!([]), "{s}");
}

fn manifest(digests: &Path, expect: &str) -> Output {
    bin()
        .arg("corpus")
        .arg("manifest")
        .arg(digests)
        .args(["--expect-files", expect])
        .output()
        .unwrap()
}

#[test]
fn a_baseline_without_a_manifest_is_refused_until_the_manifest_command_repairs_it() {
    let (dir, _) = two_files("sw-nomanifest");
    let first = temp_dir_named("sw-nomanifest-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    std::fs::remove_file(base.join("manifest.txt")).unwrap();
    let second = temp_dir_named("sw-nomanifest-second");
    let out = sweep(&dir, &second, &["--baseline", base.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        err.contains(
            "--baseline has no manifest.txt; a baseline from before manifests needs `corpus manifest <dir> --expect-files N` once"
        ),
        "{err}"
    );
    assert_no_path(&err, &base);
    assert!(!second.join("digests").exists(), "no file was digested");
    // A replay's prefix digest is not a whole-capture digest.
    std::fs::write(base.join("aa.prefix600.json"), b"{}").unwrap();
    let out = manifest(&base, "3");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("2 whole-capture digest(s), expected 3"),
        "{out:?}"
    );
    assert!(!base.join("manifest.txt").exists());
    let out = manifest(&base, "2");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let third = temp_dir_named("sw-nomanifest-third");
    let out = sweep(&dir, &third, &["--baseline", base.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(json(&third.join("sweep.json"))["identical"], 2);
}

#[test]
fn the_manifest_command_refuses_a_digest_whose_name_is_not_its_sha() {
    let (dir, shas) = two_files("sw-badname");
    let first = temp_dir_named("sw-badname-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    std::fs::remove_file(base.join("manifest.txt")).unwrap();
    let wrong = "0".repeat(64);
    std::fs::rename(
        base.join(format!("{}.json", shas[0])),
        base.join(format!("{wrong}.json")),
    )
    .unwrap();
    let out = manifest(&base, "2");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("digest {wrong}.json holds capture {}", shas[0])),
        "{out:?}"
    );
    assert!(!base.join("manifest.txt").exists());
}

/// A swept baseline whose digests directory is then edited by `edit`; the
/// next sweep against it must refuse before digesting anything.
fn refused_baseline(name: &str, edit: impl Fn(&Path, &[String])) -> (String, Vec<String>) {
    let (dir, shas) = two_files(name);
    let first = temp_dir_named(&format!("{name}-first"));
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    edit(&base, &shas);
    let second = temp_dir_named(&format!("{name}-second"));
    let out = sweep(&dir, &second, &["--baseline", base.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_no_path(&err, &base);
    assert!(!second.join("digests").exists(), "no file was digested");
    (err, shas)
}

#[test]
fn a_corrupt_or_mismatched_baseline_digest_is_refused_before_digesting() {
    let (err, _) = refused_baseline("sw-corruptbase", |base, shas| {
        std::fs::write(base.join(format!("{}.json", shas[0])), "{ not json").unwrap();
    });
    assert!(err.contains("parse baseline"), "{err}");
    let (err, shas) = refused_baseline("sw-swappedbase", |base, shas| {
        let other = std::fs::read(base.join(format!("{}.json", shas[1]))).unwrap();
        std::fs::write(base.join(format!("{}.json", shas[0])), other).unwrap();
    });
    let first = &shas[0];
    assert!(
        err.contains(&format!(
            "baseline digest {first}.json is not the whole-capture digest of {first}"
        )),
        "{err}"
    );
}

#[test]
fn an_unsorted_or_repeated_manifest_line_is_refused() {
    let rewrite = |f: fn(&mut Vec<String>)| {
        move |base: &Path, _: &[String]| {
            let p = base.join("manifest.txt");
            let mut lines: Vec<String> = std::fs::read_to_string(&p)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            f(&mut lines);
            std::fs::write(&p, format!("{}\n", lines.join("\n"))).unwrap();
        }
    };
    let (err, _) = refused_baseline("sw-unsorted", rewrite(|l| l.reverse()));
    assert!(
        err.contains("manifest.txt line 2 is out of order or repeats the line before it"),
        "{err}"
    );
    let (err, _) = refused_baseline("sw-repeated", rewrite(|l| l.insert(1, l[0].clone())));
    assert!(
        err.contains("manifest.txt line 2 is out of order or repeats the line before it"),
        "{err}"
    );
}

#[test]
fn accepting_one_of_two_new_captures_leaves_the_other_failing_by_name() {
    let (dir, _) = two_files("sw-twonew");
    let first = temp_dir_named("sw-twonew-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let c = gen_profile_bytes("baseline", 4.0);
    let d = gen_profile_bytes("baseline", 5.0);
    std::fs::write(dir.join("c.ts"), &c).unwrap();
    std::fs::write(dir.join("d.ts"), &d).unwrap();
    let (c, d) = (sha_of(&c).0, sha_of(&d).0);
    let base = first.join("digests");
    let second = temp_dir_named("sw-twonew-second");
    let out = sweep(
        &dir,
        &second,
        &[
            "--baseline",
            base.to_str().unwrap(),
            "--accept",
            &c,
            "--because",
            "new capture",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let s = json(&second.join("sweep.json"));
    assert_eq!(
        (&s["new_explained"], &s["new_unexplained"]),
        (&1.into(), &1.into()),
        "{s}"
    );
    let text = stdout(&out);
    assert!(
        text.contains(&format!("{c} new-no-baseline (accepted)")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{d} new-no-baseline: the baseline does not list it; accept it with --accept {d} --because <reason>"
        )),
        "{text}"
    );
}

#[test]
fn the_manifest_command_names_a_prefix_digest_under_a_whole_capture_name() {
    let (dir, shas) = two_files("sw-prefixname");
    let first = temp_dir_named("sw-prefixname-first");
    assert_eq!(sweep(&dir, &first, &[]).status.code(), Some(0));
    let base = first.join("digests");
    std::fs::remove_file(base.join("manifest.txt")).unwrap();
    let p = base.join(format!("{}.json", shas[0]));
    let mut v = json(&p);
    v["prefix_seconds"] = Value::from(600);
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    let out = manifest(&base, "2");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!(
            "digest {}.json is a replay prefix digest stored under a whole-capture name",
            shas[0]
        )),
        "{out:?}"
    );
}
