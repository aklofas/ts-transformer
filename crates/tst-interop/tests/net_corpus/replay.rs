//! `tst-interop corpus replay` end to end: a sweep, then a replay of the
//! generated captures over loopback udp and tcp.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tst_interop::report::stress::parse_rss_csv;

use crate::{gen_profile_bytes, run_bounded, sha_of, temp_dir_named};

const LIMIT: Duration = Duration::from_secs(240);

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tst-interop"))
}

/// A directory of two distinct 2 s captures; returns it with their SHAs.
fn two_files(name: &str) -> (PathBuf, Vec<String>) {
    let dir = temp_dir_named(name);
    let a = gen_profile_bytes("baseline", 2.0);
    let b = gen_profile_bytes("klv-sync", 2.0);
    std::fs::write(dir.join("a.ts"), &a).unwrap();
    std::fs::write(dir.join("b.ts"), &b).unwrap();
    (dir, vec![sha_of(&a).0, sha_of(&b).0])
}

fn sweep(dir: &Path, run: &Path) {
    let out = run_bounded(
        bin()
            .args(["corpus", "sweep"])
            .arg(dir)
            .arg("--out")
            .arg(run),
        LIMIT,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn replay(dir: &Path, run: &Path, env: &[(&str, &str)]) -> Output {
    run_bounded(
        bin()
            .args(["corpus", "replay"])
            .arg(dir)
            .arg("--out")
            .arg(run)
            .args(["--paths", "udp,tcp", "--budget-seconds", "7200"])
            .envs(env.iter().copied()),
        LIMIT,
    )
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn replay_over_udp_and_tcp_passes_and_writes_rss_samples() {
    let (dir, shas) = two_files("rp-pass");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let verdicts = summary["verdicts"].as_array().unwrap();
    assert_eq!(verdicts.len(), 4, "{summary}");
    assert_eq!(summary["failed"], 0, "{summary}");
    assert_eq!(summary["pass"], true);
    for sha in &shas {
        for path in ["udp", "tcp"] {
            assert!(
                verdicts
                    .iter()
                    .any(|v| v["sha256"] == sha.as_str() && v["path"] == path && v["pass"] == true),
                "{sha} {path}: {summary}"
            );
        }
    }
    assert!(run.join("replay-plan.json").is_file());
    let rows = parse_rss_csv(&std::fs::read_to_string(run.join("replay-rss.csv")).unwrap())
        .expect("replay-rss.csv parses");
    for path in ["udp", "tcp"] {
        assert!(
            rows.iter()
                .any(|r| r.leg == path && r.process == "recv" && r.rss_kb.is_some()),
            "no sample for {path}"
        );
    }
}

#[test]
fn replay_refuses_without_a_sweep() {
    let (dir, _) = two_files("rp-nosweep");
    let out = replay(&dir, &dir.join("run"), &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("run the sweep first"), "{}", text(&out));
}

#[test]
fn a_receiver_that_dies_fails_its_remaining_files_but_the_other_path_finishes() {
    let (dir, _) = two_files("rp-die");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(
        &dir,
        &run,
        &[("TST_CORPUS_TEST_RECEIVE_DIE_AFTER", "tcp:1")],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let verdicts = summary["verdicts"].as_array().unwrap();
    let of =
        |path: &str| -> Vec<&Value> { verdicts.iter().filter(|v| v["path"] == path).collect() };
    let tcp = of("tcp");
    assert_eq!(tcp.len(), 2, "{summary}");
    assert_eq!(tcp[0]["pass"], true, "{summary}");
    assert_eq!(tcp[1]["pass"], false, "{summary}");
    assert_eq!(tcp[1]["failures"][0], "receiver exited", "{summary}");
    let udp = of("udp");
    assert_eq!(udp.len(), 2, "{summary}");
    assert!(udp.iter().all(|v| v["pass"] == true), "{summary}");
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["pass"], false);
}

#[test]
fn replay_output_names_no_path() {
    let (dir, shas) = two_files("rp-nopath");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[]);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    let tmp = std::env::temp_dir().display().to_string();
    assert!(!all.contains(&tmp), "output named a path: {all}");
    assert!(!all.contains("a.ts") && !all.contains("b.ts"), "{all}");
    for sha in &shas {
        assert!(all.contains(&format!("{sha} udp PASS")), "{all}");
        assert!(all.contains(&format!("{sha} tcp PASS")), "{all}");
    }
}
