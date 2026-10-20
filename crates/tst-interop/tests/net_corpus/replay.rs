//! `tst-interop corpus replay` end to end: a sweep, then a replay of the
//! generated captures over loopback udp and tcp.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tst_interop::fixtures::{AuSizeMode, KlvSet};
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

/// Two distinct 2 s captures at a real encoder's AU sizes (about 1.7 Mb/s
/// of video), enough bytes to fill a loopback connection's buffers.
fn two_realistic_files(name: &str) -> PathBuf {
    let dir = temp_dir_named(name);
    for (file, profile) in [("a.ts", "baseline"), ("b.ts", "klv-sync")] {
        let p = tst_interop::profiles::by_name(profile).unwrap();
        tst_interop::r#gen::run(
            p,
            2.0,
            &dir.join(file),
            KlvSet::Compact,
            0,
            AuSizeMode::Realistic { scale: 1 },
        )
        .unwrap();
    }
    dir
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
    replay_over(dir, run, "udp,tcp", env)
}

fn replay_over(dir: &Path, run: &Path, paths: &str, env: &[(&str, &str)]) -> Output {
    run_bounded(
        bin()
            .args(["corpus", "replay"])
            .arg(dir)
            .arg("--out")
            .arg(run)
            .args(["--paths", paths, "--budget-seconds", "7200"])
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
    // VmRSS comes from /proc: elsewhere the file holds only its header.
    #[cfg(not(target_os = "linux"))]
    assert!(rows.is_empty(), "{rows:?}");
    #[cfg(target_os = "linux")]
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

#[test]
fn a_receiver_that_stops_reading_trips_the_send_deadline() {
    let dir = two_realistic_files("rp-deadline");
    let run = dir.join("run");
    sweep(&dir, &run);
    // tcp alone: the one-second stall knob would make a loaded udp path's
    // verdicts timing-dependent, and they prove nothing here.
    let out = replay_over(
        &dir,
        &run,
        "tcp",
        &[
            ("TST_CORPUS_TEST_RECEIVE_STOP_READING", "tcp"),
            ("TST_CORPUS_TEST_STALL_SECONDS", "1"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let verdicts = summary["verdicts"].as_array().unwrap();
    let tcp: Vec<&Value> = verdicts.iter().filter(|v| v["path"] == "tcp").collect();
    assert_eq!(tcp.len(), 2, "{summary}");
    let first = tcp[0]["failures"].as_array().unwrap();
    assert!(
        first[0].as_str().unwrap().starts_with("sender deadline"),
        "{summary}"
    );
    assert_eq!(first[1], "receiver stalled", "{summary}");
    assert_eq!(tcp[1]["failures"][0], "receiver exited", "{summary}");
    assert_eq!(verdicts.len(), 2, "{summary}");
}

#[test]
fn a_short_delivery_after_framing_damage_names_the_senders_resyncs() {
    let dir = temp_dir_named("rp-resync");
    // Realistic access units, so a PCR interval holds more than one
    // seven-packet bundle and the sender checks the framing it sends.
    let src = dir.join("src.tmp");
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    tst_interop::r#gen::run(
        p,
        2.0,
        &src,
        KlvSet::Compact,
        0,
        AuSizeMode::Realistic { scale: 1 },
    )
    .unwrap();
    let mut bytes = std::fs::read(&src).unwrap();
    std::fs::remove_file(&src).unwrap();
    // One byte gone from the middle of a packet halfway in: every packet
    // after it is off its 188-byte grid until the sender resyncs.
    let at = bytes.len() / 188 / 2 * 188 + 100;
    bytes.remove(at);
    std::fs::write(dir.join("damaged.ts"), &bytes).unwrap();
    let run = dir.join("run");
    // The sweep may fail its invariants on this capture; the replay only
    // needs its digests.
    run_bounded(
        bin()
            .args(["corpus", "sweep"])
            .arg(&dir)
            .arg("--out")
            .arg(&run),
        LIMIT,
    );
    let out = replay_over(&dir, &run, "tcp", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let v = &summary["verdicts"][0];
    assert_eq!(v["pass"], false, "{summary}");
    let failures: Vec<&str> = v["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(
        failures.iter().any(|f| f.starts_with("sender resynced ")),
        "{summary}"
    );
}

#[test]
fn every_path_records_whether_its_rss_was_gated() {
    let (dir, _) = two_files("rp-rssgate");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    for path in ["udp", "tcp"] {
        // A run of seconds is far too short for the soak's slope rule.
        assert_eq!(summary["rss_gated_by_path"][path], false, "{summary}");
        let gate = summary["rss_gate_by_path"][path].as_str().unwrap();
        assert!(gate.starts_with("not gated: "), "{gate}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&format!("rss slope {path} not gated")),
            "{}",
            text(&out)
        );
    }
}

/// A udp socket given far less than the replay asks for loses datagrams in
/// each keyframe burst, and the failed verdict names the buffer the kernel
/// granted beside the one requested. Linux only: the tiny buffer is a
/// stand-in for a kernel that caps the request, and its loss is certain
/// only where the bookkeeping is known.
#[cfg(target_os = "linux")]
#[test]
fn a_udp_receive_buffer_below_the_request_is_named_on_a_short_delivery() {
    let dir = two_realistic_files("rp-rcvbuf");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay_over(&dir, &run, "udp", &[("TST_CORPUS_TEST_RCVBUF", "2048")]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let verdicts = summary["verdicts"].as_array().unwrap();
    assert_eq!(verdicts.len(), 2, "{summary}");
    for v in verdicts {
        assert_eq!(v["pass"], false, "{summary}");
        let failures: Vec<&str> = v["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert!(
            failures
                .iter()
                .any(|f| f.starts_with("delivery incomplete: ")),
            "{summary}"
        );
        let named = failures
            .iter()
            .find_map(|f| f.strip_prefix("receive buffer "))
            .unwrap_or_else(|| panic!("no buffer attribution: {summary}"));
        let (granted, requested) = named
            .split_once(" < requested ")
            .expect("the attribution's shape");
        let (granted, requested): (u64, u64) =
            (granted.parse().unwrap(), requested.parse().unwrap());
        assert!(granted < requested, "{named}");
    }
}
