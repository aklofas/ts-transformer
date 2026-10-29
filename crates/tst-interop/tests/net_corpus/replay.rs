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

/// The distinct pids `replay-rss.csv` sampled for `path`'s receiver.
#[cfg(target_os = "linux")]
fn distinct_pids(run: &Path, path: &str) -> usize {
    let rows = parse_rss_csv(&std::fs::read_to_string(run.join("replay-rss.csv")).unwrap())
        .expect("replay-rss.csv parses");
    rows.iter()
        .filter(|r| r.leg == path && r.process == "recv")
        .map(|r| r.pid)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

#[test]
fn a_dead_path_is_restarted_for_the_next_pick() {
    let (dir, _) = two_files("rp-die");
    let run = dir.join("run");
    sweep(&dir, &run);
    // The tcp child exits after its first verdict.
    let out = replay(
        &dir,
        &run,
        &[("TST_CORPUS_TEST_RECEIVE_DIE_AFTER", "tcp:1")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    let verdicts = summary["verdicts"].as_array().unwrap();
    let of =
        |path: &str| -> Vec<&Value> { verdicts.iter().filter(|v| v["path"] == path).collect() };
    let tcp = of("tcp");
    assert_eq!(tcp.len(), 2, "{summary}");
    assert!(tcp.iter().all(|v| v["pass"] == true), "{summary}");
    // The restart is named by the pick it preceded: the second.
    let second = tcp[1]["sha256"].as_str().unwrap();
    assert_eq!(
        summary["restarted_before_by_path"]["tcp"],
        serde_json::json!([second]),
        "{summary}"
    );
    assert_eq!(
        summary["restarted_before_by_path"]["udp"],
        serde_json::json!([]),
        "{summary}"
    );
    // The RSS note says the series spans both children.
    let note = summary["rss_note_by_path"]["tcp"].as_str().unwrap();
    assert!(
        note.starts_with("the series mixes 2 receiver processes (1 restart(s)); "),
        "{note}"
    );
    // The stderr log marks where the restarted child's lines begin, by SHA.
    let stderr = std::fs::read_to_string(run.join("logs/receive-tcp.stderr")).unwrap();
    let marker = format!("--- receiver restarted for pick {second} ---");
    assert_eq!(stderr.matches(&marker).count(), 1, "{stderr}");
    let tmp = std::env::temp_dir().display().to_string();
    assert!(!stderr.contains(&tmp), "{stderr}");
    let udp = of("udp");
    assert_eq!(udp.len(), 2, "{summary}");
    assert!(udp.iter().all(|v| v["pass"] == true), "{summary}");
    // The sampler follows the restarted child.
    #[cfg(target_os = "linux")]
    assert_eq!(distinct_pids(&run, "tcp"), 2);
    // One log per path: both children's verdicts are in it.
    let log = std::fs::read_to_string(run.join("logs/receive-tcp.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
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

/// A receiver that stops reading on every pick trips each pick's send
/// deadline: the killed child is restarted once, for the second pick, and
/// that child stalls the same way.
#[test]
fn a_child_that_dies_on_every_pick_fails_each_pick_once() {
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
    assert_eq!(verdicts.len(), 2, "{summary}");
    for v in verdicts {
        assert_eq!(v["path"], "tcp", "{summary}");
        assert_eq!(v["pass"], false, "{summary}");
        let failures = v["failures"].as_array().unwrap();
        assert!(
            failures[0].as_str().unwrap().starts_with("sender deadline"),
            "{summary}"
        );
        assert_eq!(failures[1], "receiver stalled", "{summary}");
    }
    // One restart, before the second pick.
    assert_eq!(
        summary["restarted_before_by_path"]["tcp"],
        serde_json::json!([verdicts[1]["sha256"]]),
        "{summary}"
    );
}

/// Adds `ticks` to the PCR base of every PCR-bearing packet from the
/// middle of the capture on: one forward jump on the PCR timeline.
fn jump_pcrs_from_the_middle(bytes: &mut [u8], ticks: u64) {
    let half = bytes.len() / 188 / 2;
    for pkt in bytes.chunks_exact_mut(188).skip(half) {
        if pkt[3] & 0x20 == 0 || pkt[4] < 7 || pkt[5] & 0x10 == 0 {
            continue;
        }
        let base = (u64::from(pkt[6]) << 25)
            | (u64::from(pkt[7]) << 17)
            | (u64::from(pkt[8]) << 9)
            | (u64::from(pkt[9]) << 1)
            | u64::from(pkt[10] >> 7);
        let base = (base + ticks) & ((1 << 33) - 1);
        pkt[6] = (base >> 25) as u8;
        pkt[7] = (base >> 17) as u8;
        pkt[8] = (base >> 9) as u8;
        pkt[9] = (base >> 1) as u8;
        pkt[10] = (pkt[10] & 0x7F) | ((base & 1) as u8) << 7;
    }
}

/// A capture whose PCR jumps a minute forward is paced across the gap at
/// once, and the replay says so for that pick, by SHA and without a path.
#[test]
fn a_pcr_gap_the_pacer_did_not_pace_is_named_by_sha() {
    let dir = temp_dir_named("rp-pcr-gap");
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    jump_pcrs_from_the_middle(&mut bytes, 60 * 90_000);
    std::fs::write(dir.join("gap.ts"), &bytes).unwrap();
    let run = dir.join("run");
    // The sweep may judge the jump; the replay only needs its digests.
    run_bounded(
        bin()
            .args(["corpus", "sweep"])
            .arg(&dir)
            .arg("--out")
            .arg(&run),
        LIMIT,
    );
    let out = replay_over(&dir, &run, "tcp", &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    let sha = sha_of(&bytes).0;
    assert!(
        err.contains(&format!("pick {sha}: 1 PCR gap(s) not paced")),
        "{}",
        text(&out)
    );
    let tmp = std::env::temp_dir().display().to_string();
    assert!(!text(&out).contains(&tmp), "{}", text(&out));
    assert!(!text(&out).contains("gap.ts"), "{}", text(&out));
}

/// Sweeps then replays over tcp a 2 s capture at realistic access-unit
/// sizes (so a PCR interval holds more than one seven-packet bundle and the
/// sender checks the framing it sends), after `damage` has edited its bytes.
/// Returns the replay's output and its summary.
fn replay_damaged(name: &str, damage: impl FnOnce(&mut Vec<u8>)) -> (Output, Value) {
    let dir = temp_dir_named(name);
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
    damage(&mut bytes);
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
    let summary = json(&run.join("replay.json"));
    (out, summary)
}

/// Removes the 100th byte of every third packet across `count` packets
/// from the middle of the capture. The sender re-acquires sync only on
/// three packets in a row on the 188-byte grid, and every removal moves
/// the grid, so it resyncs at each one and what reaches the receiver no
/// longer demuxes the way the file does.
fn remove_a_byte_from_every_third_packet(bytes: &mut Vec<u8>, count: usize) {
    let first = bytes.len() / 188 / 2;
    // Back to front, so each offset is still on the original grid.
    for k in (first..first + count).step_by(3).rev() {
        bytes.remove(k * 188 + 100);
    }
}

/// The receiver's digest no longer matches the sweep's, and the failed
/// verdict says the sender resynced, so the loss is not read as the
/// transport's.
#[test]
fn a_failed_delivery_after_framing_damage_names_the_senders_resyncs() {
    // Sixty packets keep what the sender skips under its default watchdog
    // limit, so the send itself does not fail.
    let (out, summary) = replay_damaged("rp-resync", |b| {
        remove_a_byte_from_every_third_packet(b, 60)
    });
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
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
    assert!(
        !failures.iter().any(|f| f.starts_with("sender error")),
        "{summary}"
    );
}

/// One byte gone from the middle of one packet costs the sender that
/// packet alone: it re-acquires sync on the packets after it, so the
/// receiver demuxes what the sweep did and the replay passes.
#[test]
fn a_single_damaged_packet_does_not_fail_the_replay() {
    let (out, summary) = replay_damaged("rp-resync-one", |b| {
        let at = b.len() / 188 / 2 * 188 + 100;
        b.remove(at);
    });
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let v = &summary["verdicts"][0];
    assert_eq!(v["pass"], true, "{summary}");
    assert_eq!(v["failures"].as_array().unwrap().len(), 0, "{summary}");
}

#[test]
fn every_path_records_whether_its_rss_could_be_judged() {
    let (dir, _) = two_files("rp-rssnote");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let summary = json(&run.join("replay.json"));
    for path in ["udp", "tcp"] {
        // A run of seconds is far too short for the soak's slope rule.
        assert_eq!(summary["rss_judged_by_path"][path], false, "{summary}");
        let note = summary["rss_note_by_path"][path].as_str().unwrap();
        assert!(note.starts_with("too few samples: "), "{note}");
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains(&format!("rss slope {path}: too few samples")),
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

fn report_row(run: &Path) -> Value {
    let row = run.join("row.json");
    let out = bin()
        .args(["corpus", "report"])
        .arg(run)
        .arg("--archive-row")
        .arg(&row)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    json(&row)
}

#[test]
fn a_replay_does_not_survive_a_new_sweep_into_a_passing_report() {
    let (dir, _) = two_files("rp-stale");
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(report_row(&run)["overall_pass"], true);

    sweep(&dir, &run);
    assert!(!run.join("replay.json").exists());
    // A replay that fails its preflight writes nothing.
    let out = replay_over(&dir, &run, "nonesuch", &[]);
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(report_row(&run)["overall_pass"], false);
}

#[test]
fn a_prefixed_replay_survives_into_a_passing_report() {
    let dir = temp_dir_named("rp-prefix");
    let a = gen_profile_bytes("baseline", 4.0);
    let b = gen_profile_bytes("klv-sync", 4.0);
    std::fs::write(dir.join("a.ts"), &a).unwrap();
    std::fs::write(dir.join("b.ts"), &b).unwrap();
    let run = dir.join("run");
    sweep(&dir, &run);
    let out = replay(&dir, &run, &[("TST_CORPUS_TEST_PREFIX_SECONDS", "2")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    let prefixed = [sha_of(&a).0, sha_of(&b).0].iter().any(|sha| {
        run.join("digests")
            .join(format!("{sha}.prefix2.json"))
            .exists()
    });
    assert!(prefixed, "no pick was cut to a prefix");
    let row = report_row(&run);
    assert!(row["replay_files"].as_u64().unwrap() > 0, "{row}");
    assert_eq!(row["overall_pass"], true, "{row}");
    assert!(!row.to_string().contains("earlier sweep"), "{row}");
}
