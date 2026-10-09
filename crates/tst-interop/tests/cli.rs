//! CLI argument-parsing robustness tests, general (not proxy-specific —
//! see `tests/proxy.rs`'s `stats_json_missing_value_exits_with_usage_error`
//! for the original instance of this regression class, which this file
//! extends to `gen`/`send`/`recv`/`verify`/`report`).
//!
//! Every value-taking flag across these subcommands used to fetch its
//! value with a bare `args.get(i + 1)` (see `main.rs`'s `require_value`
//! doc comment). A flag given with NO value that's immediately followed
//! by ANOTHER recognized flag silently consumed that flag's own name as
//! if it were the first flag's value, then desynced every argument
//! after it — the user saw a misleading "unknown argument: <some later
//! token>" error instead of anything naming the flag that actually had
//! no value. A flag simply positioned as the very last, unfollowed
//! token on the command line does NOT reproduce this — every flag
//! tested here already has a post-loop "is required" check that catches
//! that degenerate case reasonably even under the old bug — so each
//! test below deliberately follows the value-less flag with another
//! real flag name, the one shape that actually distinguishes the fixed
//! behavior from the bug.
//!
//! Drives the real built CLI binary as a subprocess (the only way to
//! exercise `main.rs`'s argument loop directly — it calls
//! `std::process::exit`, so it can't be unit-tested in-process).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tst_interop::proxy::{ConfigEcho, PhaseCounters, ProxyStats};
use tst_interop::report::stress::{Axis, StepDeclaration};
use tst_interop::report_types::{CellMetrics, VerifyReport};

fn tst_interop_cmd() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_tst-interop"))
}

#[test]
fn report_merge_cells_dir_followed_by_another_flag_names_cells_dir() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "report",
            "merge",
            "--cells-dir",
            "--expectations",
            "expectations.toml",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "missing --cells-dir value must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("cells-dir"),
        "usage error should name the flag that's actually missing a value, got: {stderr}"
    );
    assert!(
        !stderr.contains("unknown argument"),
        "must not misreport this as an unrecognized argument, got: {stderr}"
    );
}

/// `report merge` must exit 1 and name the offending row when a `PASS`
/// cell matches an `expected_unsupported` expectation — the expectation
/// no longer reproduces and is now stale (see `report`'s module doc and
/// `Summary::stale_expectations`). Drives the real binary end to end
/// (cells dir + expectations + meta + a matching inventory) rather than
/// unit-testing `report::merge` directly, so this also proves the CLI's
/// own exit-code wiring, not just the library function's return value.
#[test]
fn report_merge_with_stale_expectation_exits_1_and_prints_error() {
    let dir = std::env::temp_dir().join(format!(
        "tst-interop-cli-merge-stale-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ));
    let cells_dir = dir.join("cells");
    std::fs::create_dir_all(&cells_dir).expect("create cells dir");

    std::fs::write(
        cells_dir.join("a.json"),
        r#"{"id":"decode/ffmpeg","profile":"baseline","peer":"ffmpeg","direction":"recv","tier":"remux","verdict":"PASS","failures":[],"metrics":null,"log":"decode-ffmpeg.log"}"#,
    )
    .expect("write cell a");

    let expectations_path = dir.join("expectations.toml");
    std::fs::write(
        &expectations_path,
        "[[expect]]\ncell = \"decode/ffmpeg\"\nprofile = \"baseline\"\nverdict = \"expected_unsupported\"\nreason = \"gap\"\nfailure_contains = \"gap\"\n",
    )
    .expect("write expectations");

    let meta_path = dir.join("meta.json");
    std::fs::write(&meta_path, r#"{"host": "test-host"}"#).expect("write meta");

    // Matches the one produced cell exactly, so this proves the exit-1
    // comes from the stale expectation, not an inventory mismatch.
    let inventory_path = dir.join("inventory.json");
    std::fs::write(
        &inventory_path,
        r#"{"shape":"subset","seconds_per_cell":10,"cells_glob":"*","profiles":["baseline"],"cells":[{"id":"decode/ffmpeg","profile":"baseline"}],"allowed_skips":[],"tools":{}}"#,
    )
    .expect("write inventory");

    let out_path = dir.join("results.json");

    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "report",
            "merge",
            "--cells-dir",
            cells_dir.to_str().unwrap(),
            "--expectations",
            expectations_path.to_str().unwrap(),
            "--meta",
            meta_path.to_str().unwrap(),
            "--inventory",
            inventory_path.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a stale expectation must exit 1, stderr: {stderr}"
    );
    assert!(
        stderr.contains("ERROR stale expectation"),
        "stderr must report the stale expectation, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn report_merge_without_inventory_exits_2_and_names_the_flag() {
    // --inventory is mandatory (spec §5.1) — a merge invocation that
    // omits it must be rejected before ever touching --cells-dir, not
    // silently treated as an unchecked run. Covers the same required-flag
    // contract exercised for --cells-dir/--expectations/--meta/--out
    // elsewhere in main.rs, which had no direct test of its own.
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "report",
            "merge",
            "--cells-dir",
            "does-not-matter",
            "--expectations",
            "does-not-matter.toml",
            "--meta",
            "does-not-matter.json",
            "--out",
            "does-not-matter.json",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing --inventory must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("--inventory"),
        "stderr must name the missing flag, got: {stderr}"
    );
}

#[test]
fn send_profile_followed_by_another_flag_names_profile() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args(["send", "--profile", "--url", "udp://127.0.0.1:1"])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "missing --profile value must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("profile"),
        "usage error should name the flag that's actually missing a value, got: {stderr}"
    );
    assert!(
        !stderr.contains("unknown argument"),
        "must not misreport this as an unrecognized argument, got: {stderr}"
    );
}

/// `--reconnect-mode` takes exactly two values. Anything else must be a
/// usage error naming the flag — never a silent fall-back to `blocking`,
/// which would let a drill believe it ran in a mode it never did. Exits
/// during argument parsing, before any transport is built, so the URL is
/// never dialed.
#[test]
fn send_reconnect_mode_with_an_unknown_value_exits_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "send",
            "--profile",
            "baseline",
            "--url",
            "udp://127.0.0.1:1",
            "--seconds",
            "1",
            "--managed",
            "--reconnect-mode",
            "eventually",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown --reconnect-mode value must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("--reconnect-mode") && stderr.contains("eventually"),
        "usage error should name the flag and the rejected value, got: {stderr}"
    );
}

/// Only the managed path builds a `ReconnectPolicy`, so without
/// `--managed` the mode would be parsed and then read by nothing. That
/// is rejected rather than dropped — again before any transport is
/// built.
#[test]
fn send_reconnect_mode_without_managed_exits_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "send",
            "--profile",
            "baseline",
            "--url",
            "udp://127.0.0.1:1",
            "--seconds",
            "1",
            "--reconnect-mode",
            "background",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "--reconnect-mode without --managed must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("--reconnect-mode") && stderr.contains("--managed"),
        "usage error should say what the flag depends on, got: {stderr}"
    );
}

/// Positive control for the two rejections above, and the CLI half of
/// `managed_send`: `--managed --reconnect-mode background` is accepted,
/// runs, and the JSON it prints carries the managed transport's own
/// account of the run. Over UDP to a socket this test holds open — there
/// is no outage here and nothing to reconnect to, so every counter is
/// zero; what is pinned is that the flag parses and the block is
/// written. The reconnect itself is
/// `tests/proxy.rs::srt_background_reconnect_keeps_producing_through_an_outage`'s
/// job.
#[test]
fn send_managed_background_is_accepted_and_reports_managed_send() {
    let sink = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp sink");
    let url = format!(
        "udp://127.0.0.1:{}",
        sink.local_addr().expect("sink local_addr").port()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "send",
            "--profile",
            "baseline",
            "--url",
            &url,
            "--seconds",
            "0.2",
            "--json",
            "-",
            "--managed",
            "--reconnect-mode",
            "background",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(0),
        "a managed background send must succeed, stderr: {stderr}"
    );
    let metrics: tst_interop::report_types::CellMetrics = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be the metrics JSON ({e}), got: {stdout}"));
    let managed = metrics
        .managed_send
        .expect("a managed send's JSON must carry managed_send");
    assert_eq!(
        (managed.reconnect_attempts, managed.gap_messages_dropped),
        (0, 0),
        "nothing broke, so nothing was retried or evicted: {managed:?}"
    );
    assert_eq!(
        managed.gap_buffer_capacity, 256,
        "the harness runs the policy's default gap buffer"
    );
    assert_eq!(
        (
            managed.reconnect_mode.as_deref(),
            managed.overflow_policy.as_deref()
        ),
        (Some("background"), Some("drop_oldest")),
        "the report must name the mode and the overflow policy the transport ran"
    );
}

/// The other mode, reached the way `soak.sh` reaches it by default: a
/// `--managed` send with no `--reconnect-mode` at all reports `blocking`,
/// so a reader never has to know what the flag's default was at the
/// revision that wrote the file.
#[test]
fn send_managed_without_a_mode_reports_blocking() {
    let sink = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp sink");
    let url = format!(
        "udp://127.0.0.1:{}",
        sink.local_addr().expect("sink local_addr").port()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "send",
            "--profile",
            "baseline",
            "--url",
            &url,
            "--seconds",
            "0.2",
            "--json",
            "-",
            "--managed",
        ])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(0),
        "a managed send must succeed, stderr: {stderr}"
    );
    let metrics: tst_interop::report_types::CellMetrics = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be the metrics JSON ({e}), got: {stdout}"));
    let managed = metrics
        .managed_send
        .expect("a managed send's JSON must carry managed_send");
    assert_eq!(managed.reconnect_mode.as_deref(), Some("blocking"));
}

/// The launch gate: `soak.sh` runs `report soak --validate-only` on the
/// config it has just written, before any worker starts. A declared
/// reconnect mode that names neither mode must stop the run there, with
/// exit 2 and the field named — not 72 hours later as a verdict that can
/// never match.
#[test]
fn report_soak_validate_only_rejects_an_unknown_declared_reconnect_mode() {
    let dir = std::env::temp_dir().join(format!(
        "tst-interop-cli-validate-reconnect-mode-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let validate = |name: &str, reconnect_mode: &str| {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                r#"{{"expected_duration_s": 3600, "rss_cadence_s": 30, "warmup_fraction": 0.1667,
                    "sampler_end_slack_s": 35, "expected_worker_exits": {{}},
                    "legs": {{"srt": {{"profile": "baseline", "reconnect_mode": {reconnect_mode}}},
                             "rist": {{"profile": "baseline", "reconnect_mode": null}}}}}}"#
            ),
        )
        .expect("write config");
        Command::new(env!("CARGO_BIN_EXE_tst-interop"))
            .args([
                "report",
                "soak",
                "--config",
                path.to_str().unwrap(),
                "--validate-only",
            ])
            .output()
            .expect("spawn tst-interop binary")
    };

    let good = validate("good.json", r#""background""#);
    assert_eq!(
        good.status.code(),
        Some(0),
        "a known mode must validate, stderr: {}",
        String::from_utf8_lossy(&good.stderr)
    );

    // Wrong name, wrong case, and wrong type. The last is refused by the
    // JSON parser, whose message gives a position rather than a field
    // path, so only the first two are held to naming the field.
    for (name, value, names_the_field) in [
        ("typo.json", r#""backgroud""#, true),
        ("case.json", r#""Background""#, true),
        ("type.json", "1", false),
    ] {
        let bad = validate(name, value);
        let stderr = String::from_utf8_lossy(&bad.stderr);
        assert_eq!(
            bad.status.code(),
            Some(2),
            "reconnect_mode {value} must fail the launch gate, stderr: {stderr}"
        );
        assert!(
            !names_the_field || stderr.contains("legs.srt.reconnect_mode"),
            "the error must name the field, got: {stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_file_followed_by_another_flag_names_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args(["verify", "--file", "--expect", "baseline"])
        .output()
        .expect("spawn tst-interop binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "missing --file value must exit 2, stderr: {stderr}"
    );
    assert!(
        stderr.contains("file"),
        "usage error should name the flag that's actually missing a value, got: {stderr}"
    );
    assert!(
        !stderr.contains("unknown argument"),
        "must not misreport this as an unrecognized argument, got: {stderr}"
    );
}

#[test]
fn au_scale_without_realistic_is_a_usage_error() {
    let out = tst_interop_cmd()
        .args([
            "gen",
            "--profile",
            "baseline",
            "--seconds",
            "1",
            "--out",
            "-",
            "--au-scale",
            "2",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--au-scale requires --au-sizes realistic"),
        "{err}"
    );
}

#[test]
fn au_scale_above_the_pes_cap_is_refused() {
    let out = tst_interop_cmd()
        .args([
            "gen",
            "--profile",
            "baseline",
            "--seconds",
            "1",
            "--out",
            "-",
            "--au-sizes",
            "realistic",
            "--au-scale",
            "79",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("must be an integer in 1..=78"));
}

#[test]
fn au_scale_grows_the_generated_stream() {
    let dir = std::env::temp_dir().join(format!("tst-interop-au-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let one = dir.join("one.ts");
    let four = dir.join("four.ts");
    for (scale, path) in [("1", &one), ("4", &four)] {
        let st = tst_interop_cmd()
            .args([
                "gen",
                "--profile",
                "baseline",
                "--seconds",
                "2",
                "--au-sizes",
                "realistic",
                "--au-scale",
                scale,
                "--out",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(st.success());
    }
    let (a, b) = (
        std::fs::metadata(&one).unwrap().len(),
        std::fs::metadata(&four).unwrap().len(),
    );
    assert!(
        b > 3 * a,
        "scale 4 must be roughly 4x the bytes of scale 1: {a} vs {b}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// --- `report step`/`report hold`/`report stress` ---------------------
//
// `report::stress::run_step` reads a whole directory tree (sampler
// CSVs + per-stream JSON reports), not a single file, so exercising
// the CLI end to end needs a fixture writer. `write_healthy_step_dir`
// below mirrors the healthy fixture `report/stress.rs`'s own
// `write_healthy_step` test helper writes (same 22-tick-at-30s shape,
// same stream counts) — that helper is `#[cfg(test)]`-private to its
// module, unreachable from this integration test binary, so this is a
// field-for-field twin kept in sync by hand rather than a shared import.

/// A fresh, process-and-time-unique temp dir for one test, matching the
/// uniqueness scheme the other tests in this file already inline (pid +
/// nanos) rather than introducing a new shared counter.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tst-interop-cli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A `CellMetrics` with nothing to report beyond the AU count — every
/// field is public plain data, so this constructs one directly rather
/// than going through `report/stress.rs`'s `pub(crate)` test builder.
fn healthy_cell_metrics(video_aus: u64) -> CellMetrics {
    CellMetrics {
        video_aus,
        keyframes: video_aus / 30,
        klv_records: 0,
        klv_set_sha256: Some(String::new()),
        audio_frames: 0,
        programs_seen: 1,
        pts_monotonic: true,
        misp_sei_seen: false,
        bytes: 0,
        stream_sha256: String::new(),
        discontinuities: 0,
        nonconformant: 0,
        corruption: None,
        corruption_attribution: None,
        klv_rich: None,
        since_reconnect: None,
        managed_send: None,
        judged_profile: None,
        skipped_oracles: None,
    }
}

fn healthy_recv_report(video_aus: u64) -> VerifyReport {
    VerifyReport {
        pass: true,
        failures: Vec::new(),
        metrics: healthy_cell_metrics(video_aus),
        reconnects: None,
        profile: None,
        skipped_oracles: Vec::new(),
        publish_mount: None,
    }
}

fn healthy_proxy_stats(forwarded: u64) -> ProxyStats {
    ProxyStats {
        forwarded,
        dropped: 0,
        duped: 0,
        reordered: 0,
        seed: 1,
        config: ConfigEcho {
            loss_pct: 0.0,
            dup_pct: 0.0,
            reorder_pct: 0.0,
            reorder_hold: 0,
            jitter_ms_max: 0,
            base_delay_ms: 0,
            outage_period_s: None,
            outage_dur_s: 0,
            schedule: None,
        },
        phases: vec![PhaseCounters {
            index: 0,
            forwarded,
            dropped: 0,
            duped: 0,
            outage_dropped: 0,
        }],
    }
}

/// Writes a minimal, healthy stress-step directory with `streams`
/// concurrent `srt-<i>` legs: `config.json`, `rss.csv`/`proc.csv`
/// (22 ticks at the declared 30 s cadence — 20 of them land at or past
/// the declared 60 s warm-up, matching `hold_s / sample_cadence_s` = 20
/// so `sample_coverage` reads exactly 1.0) and `host.csv` (one row per
/// tick), `exits.json`, and per-stream
/// `streams/<i>/{send-report,recv-report,proxy-stats}.json` — enough
/// for `report::stress::run_step`/`run_hold` to judge the directory and
/// pass every verdict at the default thresholds.
fn write_healthy_step_dir(dir: &Path, streams: u32) {
    let decl = StepDeclaration {
        transport: "srt".to_string(),
        axis: Axis::Streams,
        streams,
        au_scale: 1,
        warmup_s: 60.0,
        hold_s: 600.0,
        sample_cadence_s: 30.0,
        vcpus: 8,
        clk_tck: 100,
        nominal_mbps_per_stream: 1.7,
        managed: true,
        outage_period_s: None,
        outage_dur_s: None,
        restart_period_s: None,
        corrupt_rate_per_10k: None,
    };
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string(&decl).unwrap(),
    )
    .expect("write config.json");

    let mut proc_csv =
        String::from("elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n");
    let mut rss_csv = String::from("elapsed_s,leg,process,pid,rss_kb\n");
    let mut worker_exits = BTreeMap::new();
    for i in 0..streams {
        let leg = format!("srt-{i}");
        for tick in 0..22u64 {
            let t = tick as f64 * 30.0;
            let utime = tick * 30;
            for process in ["send", "proxy", "recv"] {
                proc_csv.push_str(&format!("{t},{leg},{process},1,{utime},0,4,5\n"));
                rss_csv.push_str(&format!("{t},{leg},{process},1,50000\n"));
            }
        }
        for role in ["send", "proxy", "recv"] {
            worker_exits.insert(format!("{leg}-{role}"), 0);
        }
    }
    std::fs::write(dir.join("proc.csv"), proc_csv).expect("write proc.csv");
    std::fs::write(dir.join("rss.csv"), rss_csv).expect("write rss.csv");

    let mut host_csv =
        String::from("elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n");
    for tick in 0..22u64 {
        let t = tick as f64 * 30.0;
        host_csv.push_str(&format!("{t},0.1,0.1,0.1,1,1000000\n"));
    }
    std::fs::write(dir.join("host.csv"), host_csv).expect("write host.csv");

    std::fs::write(
        dir.join("exits.json"),
        serde_json::to_string(&worker_exits).unwrap(),
    )
    .expect("write exits.json");

    for i in 0..streams {
        let sdir = dir.join("streams").join(i.to_string());
        std::fs::create_dir_all(&sdir).expect("create stream dir");
        std::fs::write(
            sdir.join("send-report.json"),
            serde_json::to_string(&healthy_cell_metrics(18_000)).unwrap(),
        )
        .expect("write send-report.json");
        std::fs::write(
            sdir.join("recv-report.json"),
            serde_json::to_string(&healthy_recv_report(18_000)).unwrap(),
        )
        .expect("write recv-report.json");
        std::fs::write(
            sdir.join("proxy-stats.json"),
            serde_json::to_string(&healthy_proxy_stats(18_000)).unwrap(),
        )
        .expect("write proxy-stats.json");
    }
}

#[test]
fn report_step_judges_a_directory_and_exits_by_verdict() {
    let dir = temp_dir("report-step");
    write_healthy_step_dir(&dir, 1);

    let out = tst_interop_cmd()
        .args([
            "report",
            "step",
            "--dir",
            dir.to_str().unwrap(),
            "--rss-slope-threshold-kb-per-hour",
            "1024",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("step-results.json").exists());

    // An impossible cpu ceiling flips it to exit 1.
    let out = tst_interop_cmd()
        .args([
            "report",
            "step",
            "--dir",
            dir.to_str().unwrap(),
            "--rss-slope-threshold-kb-per-hour",
            "1024",
            "--cpu-headroom-max",
            "0.0001",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cpu_headroom"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn report_step_requires_the_rss_threshold() {
    let dir = temp_dir("report-step-norss");
    write_healthy_step_dir(&dir, 1);

    let out = tst_interop_cmd()
        .args(["report", "step", "--dir", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("--rss-slope-threshold-kb-per-hour is required")
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn report_stress_folds_a_sweep_tree() {
    let outdir = temp_dir("report-stress");
    let step_dir = outdir.join("sweep").join("srt").join("streams").join("1");
    std::fs::create_dir_all(&step_dir).expect("create sweep/srt/streams/1");
    write_healthy_step_dir(&step_dir, 1);

    let out = tst_interop_cmd()
        .args([
            "report",
            "step",
            "--dir",
            step_dir.to_str().unwrap(),
            "--rss-slope-threshold-kb-per-hour",
            "1024",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = tst_interop_cmd()
        .args(["report", "stress", "--outdir", outdir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(outdir.join("stress-results.json").exists());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("report stress: overall_pass=true"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let empty = temp_dir("report-stress-empty");
    let out = tst_interop_cmd()
        .args(["report", "stress", "--outdir", empty.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&outdir).unwrap();
    std::fs::remove_dir_all(&empty).unwrap();
}
