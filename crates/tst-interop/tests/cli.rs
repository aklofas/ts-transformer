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

use std::process::Command;

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
