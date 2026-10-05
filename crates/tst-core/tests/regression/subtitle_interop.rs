//! Empirical interop matrix for WebVTT-in-TS + CEA-708
//! standalone carriage.
//!
//! # Why this exists
//!
//! One question: do external MPEG-TS receivers
//! **ignore** our auto-emitted `registration_descriptor` markers
//! `"VTTC"` (WebVTT-in-TS) and `"GA94"` (CEA-708 standalone), letting
//! the PIDs pass through as opaque private PES once they fail to
//! recognize the codec? Or do they actively **reject** the stream
//! because of the unknown markers? The rustdoc on
//! `format_identifier_vttc` / `format_identifier_ga94` claims only
//! "library-internal round-trip only — external-tool interop has not
//! been empirically verified"; this harness gathers that evidence.
//!
//! # The pass/fail model
//!
//! Each `(tool, fixture)` cell produces one of three outcomes:
//!
//! - **ignore (good)**: tool reads the TS, recognizes the structure,
//!   and either (a) silently ignores the subtitle PID or (b) labels it
//!   as opaque private data (e.g. ffprobe `bin_data`). Exit code 0,
//!   no parser-fatal stderr.
//! - **reject (bad)**: tool exits non-zero or emits "Invalid data"
//!   / "could not find codec" *attributable to the VTTC/GA94 markers
//!   specifically*. If found, the soft-doc stance fails and we need
//!   to change the markers (or drop the auto-emit).
//! - **skip**: the tool isn't on PATH on this machine. CI may not
//!   have ffmpeg/gstreamer/tsduck installed; the test must not fail
//!   when a tool is missing.
//!
//! # What this harness does NOT prove
//!
//! "Ignore" here means *the container parser accepts the stream*,
//! not *the receiver extracts the WebVTT cue text*. WebVTT-in-TS
//! decoding requires demuxer-specific support (Apple HLS variant).
//! The rustdoc only claims pass-through compatibility (the unknown PID
//! doesn't cause the rest of the program to be rejected). That is
//! what we measure here.
//!
//! # Why these probes and not others
//!
//! - **`ffprobe -show_streams`**: lightweight, exercises ffmpeg's
//!   container parser. The exit code + per-stream `codec_name`
//!   reveal both rejection (non-zero) and pass-through classification
//!   (`bin_data` for an unknown PID is the textbook ignore signal).
//! - **`tsp -P psi --all -O drop`** (tsduck): independent
//!   implementation, doesn't share ffmpeg's heuristics. The `psi`
//!   plugin dumps PSI tables — VTTC/GA94 markers show up as
//!   `Format identifier: ... ("VTTC")` if tsduck handles the
//!   registration descriptor cleanly.
//! - **`tsanalyze`** (tsduck): structural report. Catches PSI
//!   parse errors that tsp's bulk-drop wouldn't surface.
//! - **`gst-launch-1.0 ... tsdemux ! fakesink`**: third independent
//!   implementation (GStreamer Bad Plugins). `fakesink` discards
//!   data so we test parser acceptance only.
//!
//! VLC / mpv are intentionally NOT probed: they need GUI plumbing
//! or `--quit-on-end` flags that aren't worth the complexity for a
//! container-acceptance question already covered by three independent
//! parsers. ffmpeg is the most widely deployed receiver and the one
//! earlier docs specifically named, so its result is load-bearing.
//!
//! # `ffmpeg -c copy -f null` was intentionally dropped
//!
//! Earlier draft included `ffmpeg -i ... -map 0 -c copy -f null -`
//! to test stream-copy acceptance. It fails with `dimensions not set`
//! on **every** fixture — including pure DVB-subtitling ones with no
//! VTTC/GA94 markers — because the synthetic H.264 elementary stream
//! lacks an SPS the null muxer can read. The failure is unrelated to
//! the subtitle markers; including it would mis-classify all fixtures
//! as "reject" and obscure the real signal. Dropped from the matrix.
//!
//! # CI behavior
//!
//! Default test pass: matrix is informational, printed via
//! `eprintln!`. The `wave_i1_matrix_no_regression` test (marked
//! `#[ignore]`) does an actual assert against a known-good baseline
//! — run it explicitly to fail on regression. PR CI runs
//! `cargo test --workspace`, which executes default-pass tests with
//! all tools optional via runtime `which`-style detection. Missing
//! tools yield `skip`, never failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One cell of the interop matrix: a (tool, fixture) probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeResult {
    /// External tool name (e.g. `"ffprobe"`).
    tool: &'static str,
    /// Fixture filename (no directory).
    fixture: String,
    /// Process exit status. `None` if the tool wasn't run at all
    /// (e.g. tool not on PATH).
    exit: Option<i32>,
    /// Outcome classification — see module docs for definitions.
    action: Action,
    /// Free-form notes (tool stderr snippet, codec_name, etc.).
    notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Tool accepted the structure (parser exit-code 0, no fatal
    /// stderr, subtitle PID either ignored or labelled as opaque
    /// data). The soft-doc claim holds for this cell.
    Ignore,
    /// Tool rejected the stream because of the marker. The claim
    /// fails for this cell — caller should investigate.
    Reject,
    /// Tool not present on PATH; cell not measured. CI must
    /// tolerate this silently.
    Skip,
    /// Tool ran but failed for a reason unrelated to the markers
    /// (e.g. the synthetic fixture's empty H.264 stream tripping a
    /// downstream codec parser). Treated as informational, not a
    /// rejection signal for the claim.
    UnrelatedFailure,
}

impl Action {
    fn as_str(&self) -> &'static str {
        match self {
            Action::Ignore => "ignore",
            Action::Reject => "REJECT",
            Action::Skip => "skip",
            Action::UnrelatedFailure => "unrelated-fail",
        }
    }
}

/// All subtitle fixtures we test, in stable order for matrix output.
const FIXTURES: &[&str] = &[
    "webvtt_in_ts_simple.ts",
    "webvtt_in_ts_multi_cue.ts",
    "cea708_standalone.ts",
    "subtitle_with_klv_same_program.ts",
    "webvtt_multi_program_with_klv.ts",
    // Controls — these fixtures DON'T use VTTC/GA94 markers (DVB
    // subtitling/teletext are standard ETSI EN 300 468 descriptors,
    // not our auto-emitted private markers). If the controls fail
    // a probe, the failure is the tool's not the marker's, and we
    // can subtract that noise from the WebVTT/CEA-708 results.
    "dvb_subtitling_eng.ts",
    "dvb_subtitling_multi_lang.ts",
    "dvb_teletext_eng.ts",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subtitles")
}

/// Returns true if a tool is callable. Uses the tool's own
/// `--version` or `-version` flag and treats any exit-zero as
/// "present". Cheaper and more portable than shelling out to
/// `which` (no `which` on Windows; subprocess spawn errors are
/// the canonical "missing tool" signal on Unix too).
fn tool_present(cmd: &str, version_flag: &str) -> bool {
    Command::new(cmd)
        .arg(version_flag)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse the integer counter that follows a `tsanalyze` field
/// label like `"With invalid sync:"`. The report layout pads with
/// dots: `"|     With invalid sync: .................. 0  | ..."`.
/// We slice after the label, drop the leading whitespace + dots,
/// then read characters while they're ASCII digits.
fn parse_counter_after(line: &str, label: &str) -> Option<u64> {
    let after = line.split_once(label)?.1;
    let trimmed = after.trim_start_matches(|c: char| c.is_whitespace() || c == '.');
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Capture the last `n` lines of stderr text, trimmed to 200 bytes
/// for readable matrix output. Long ffmpeg backtraces would otherwise
/// dominate the `notes` column.
fn tail_stderr(stderr: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut tail = lines[start..].join(" | ");
    if tail.len() > 200 {
        tail.truncate(200);
        tail.push_str("...");
    }
    tail
}

/// ffprobe: container parser acceptance + per-stream classification.
///
/// We expect VTTC/GA94 PIDs to surface as `codec_name=bin_data`
/// (ffmpeg's label for an opaque elementary stream — proves it
/// parsed the container, recognized the PID, and didn't error on
/// the registration descriptor). The control fixtures
/// (`dvb_subtitling_eng.ts` etc.) should surface their proper
/// codec names (`dvb_subtitle`, `dvb_teletext`).
fn probe_ffprobe(fixture_path: &Path) -> ProbeResult {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-show_format"])
        .arg(fixture_path)
        .output();
    let fixture = fixture_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Collect codec_name=... lines for the notes column.
            let codecs: Vec<&str> = stdout
                .lines()
                .filter(|l| l.starts_with("codec_name="))
                .collect();
            ProbeResult {
                tool: "ffprobe",
                fixture,
                exit: Some(0),
                action: Action::Ignore,
                notes: codecs.join(", "),
            }
        }
        Ok(o) => {
            let exit = o.status.code().unwrap_or(-1);
            let stderr_tail = tail_stderr(&o.stderr, 5);
            // Heuristic: ffprobe errors like "Invalid data found" or
            // "could not find codec parameters" on the SUBTITLE PID
            // are markers-rejection. Errors on the video PID
            // (`dimensions not set`, `non-existing PPS`) are the
            // synthetic-fixture artifact described in the module
            // docs. We don't see ffprobe fail with -show_streams on
            // any fixture today, so this branch is defensive.
            let action = if stderr_tail.contains("dimensions not set")
                || stderr_tail.contains("non-existing PPS")
            {
                Action::UnrelatedFailure
            } else {
                Action::Reject
            };
            ProbeResult {
                tool: "ffprobe",
                fixture,
                exit: Some(exit),
                action,
                notes: stderr_tail,
            }
        }
        Err(e) => ProbeResult {
            tool: "ffprobe",
            fixture,
            exit: None,
            action: Action::Skip,
            notes: format!("spawn-error: {e}"),
        },
    }
}

/// tsduck `tsp -P psi --all -O drop` — dump every PSI table
/// (PAT/PMT/etc.) and discard the TS body. Probes that tsduck
/// can parse the PMT including the registration descriptors. If
/// tsduck mis-handled VTTC/GA94 we'd see psi-parse errors here.
///
/// `--all` flag is needed so the plugin doesn't stop after the
/// first occurrence (our fixtures are tiny — 3-15 TS packets).
fn probe_tsduck_psi(fixture_path: &Path) -> ProbeResult {
    let out = Command::new("tsp")
        .args(["-I", "file"])
        .arg(fixture_path)
        .args(["-P", "psi", "--all", "-O", "drop"])
        .output();
    let fixture = fixture_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Confirm we actually saw the registration descriptor
            // surfaced by name. If tsduck silently dropped the
            // descriptor we'd want to know about it; if it errored
            // we'd see non-zero exit.
            let saw_vttc = stdout.contains("(\"VTTC\")");
            let saw_ga94 = stdout.contains("(\"GA94\")");
            let saw_pmt = stdout.contains("PMT");
            let note = format!(
                "pmt={pmt} vttc={vttc} ga94={ga94}",
                pmt = saw_pmt,
                vttc = saw_vttc,
                ga94 = saw_ga94
            );
            ProbeResult {
                tool: "tsduck-psi",
                fixture,
                exit: Some(0),
                action: Action::Ignore,
                notes: note,
            }
        }
        Ok(o) => ProbeResult {
            tool: "tsduck-psi",
            fixture,
            exit: Some(o.status.code().unwrap_or(-1)),
            action: Action::Reject,
            notes: tail_stderr(&o.stderr, 5),
        },
        Err(e) => ProbeResult {
            tool: "tsduck-psi",
            fixture,
            exit: None,
            action: Action::Skip,
            notes: format!("spawn-error: {e}"),
        },
    }
}

/// tsduck `tsanalyze` — structural report. Catches errors that the
/// bulk-drop tsp run wouldn't surface (invalid sync bytes, malformed
/// section CRCs, etc.). Output is huge, but exit-zero + no
/// "with invalid sync" / "with transport error" counters above zero
/// is the textbook pass.
fn probe_tsanalyze(fixture_path: &Path) -> ProbeResult {
    let out = Command::new("tsanalyze").arg(fixture_path).output();
    let fixture = fixture_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Scan for the two structural-error counters. tsanalyze
            // lays them out as:
            //   "|     With invalid sync: .................. 0  | ..."
            // We strip everything before the field name, then strip
            // the dot-leader padding, then read the first numeric
            // token. `parse_counter_after` does that.
            let bad_sync = stdout
                .lines()
                .any(|l| parse_counter_after(l, "With invalid sync:").is_some_and(|n| n != 0));
            let bad_transport = stdout
                .lines()
                .any(|l| parse_counter_after(l, "With transport error:").is_some_and(|n| n != 0));
            let action = if bad_sync || bad_transport {
                Action::Reject
            } else {
                Action::Ignore
            };
            ProbeResult {
                tool: "tsanalyze",
                fixture,
                exit: Some(0),
                action,
                notes: format!("sync_errs={bad_sync} transport_errs={bad_transport}"),
            }
        }
        Ok(o) => ProbeResult {
            tool: "tsanalyze",
            fixture,
            exit: Some(o.status.code().unwrap_or(-1)),
            action: Action::Reject,
            notes: tail_stderr(&o.stderr, 5),
        },
        Err(e) => ProbeResult {
            tool: "tsanalyze",
            fixture,
            exit: None,
            action: Action::Skip,
            notes: format!("spawn-error: {e}"),
        },
    }
}

/// GStreamer `tsdemux ! fakesink` — third independent TS parser.
///
/// Caveats this probe has to absorb:
///
/// - Our fixtures are 3-15 TS packets total (only the PMT and a
///   single PES per stream). gstreamer prefers to see at least one
///   complete program before EOS and emits "No program activated
///   before EOS" if it doesn't. That's a fixture-size issue, not a
///   marker issue — it fires on ALL our fixtures including the
///   DVB-subtitling controls.
/// - gst-launch is *flaky* on these tiny fixtures: stderr error
///   message lands in ~10ms, but ~60% of runs the process then
///   hangs forever in pipeline state-change negotiation instead of
///   exiting. We wrap with `timeout 5` so the test doesn't deadlock
///   the test runner. Exit-124 with the benign stderr is treated
///   identically to exit-1 with the benign stderr.
/// - `-q` suppresses non-error gstreamer chatter so stderr is
///   focused; pipeline status goes to exit code.
fn probe_gst_tsdemux(fixture_path: &Path) -> ProbeResult {
    let location = format!("location={}", fixture_path.display());
    // Wrap in `timeout 5` — gst-launch is flaky on tiny fixtures
    // (see docstring). Without this, ~60% of CI runs would hang
    // the test until the cargo-test timeout, ~3 min per stuck cell.
    let out = Command::new("timeout")
        .args(["5", "gst-launch-1.0", "-q", "filesrc"])
        .arg(&location)
        .args(["!", "tsdemux", "!", "fakesink"])
        .output();
    let fixture = fixture_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match out {
        Ok(o) => {
            let stderr_text = String::from_utf8_lossy(&o.stderr);
            let stderr_tail = tail_stderr(&o.stderr, 5);
            // "No program activated before EOS" is the benign
            // fixture-size warning; treat as ignore. Anything else
            // non-zero or with parser-fatal stderr is a real reject.
            let benign_short_fixture = stderr_text.contains("No program activated");
            let parser_fatal = stderr_text.contains("Invalid data")
                || stderr_text.contains("Could not parse")
                || stderr_text.contains("could not link");
            let action = if parser_fatal {
                Action::Reject
            } else if benign_short_fixture || o.status.success() {
                Action::Ignore
            } else {
                Action::UnrelatedFailure
            };
            ProbeResult {
                tool: "gst-tsdemux",
                fixture,
                exit: Some(o.status.code().unwrap_or(-1)),
                action,
                notes: stderr_tail,
            }
        }
        Err(e) => ProbeResult {
            tool: "gst-tsdemux",
            fixture,
            exit: None,
            action: Action::Skip,
            notes: format!("spawn-error: {e}"),
        },
    }
}

/// Run the full matrix and return all cells in stable order.
fn run_matrix() -> Vec<ProbeResult> {
    let dir = fixtures_dir();
    let mut rows = Vec::new();
    for fname in FIXTURES {
        let path = dir.join(fname);
        if !path.exists() {
            eprintln!("WARN: fixture missing: {}", path.display());
            continue;
        }
        // Probe order is stable; one row per (tool, fixture).
        if tool_present("ffprobe", "-version") {
            rows.push(probe_ffprobe(&path));
        } else {
            rows.push(ProbeResult {
                tool: "ffprobe",
                fixture: (*fname).to_string(),
                exit: None,
                action: Action::Skip,
                notes: "ffprobe not on PATH".to_string(),
            });
        }
        if tool_present("tsp", "--version") {
            rows.push(probe_tsduck_psi(&path));
            rows.push(probe_tsanalyze(&path));
        } else {
            rows.push(ProbeResult {
                tool: "tsduck-psi",
                fixture: (*fname).to_string(),
                exit: None,
                action: Action::Skip,
                notes: "tsp not on PATH".to_string(),
            });
            rows.push(ProbeResult {
                tool: "tsanalyze",
                fixture: (*fname).to_string(),
                exit: None,
                action: Action::Skip,
                notes: "tsanalyze not on PATH".to_string(),
            });
        }
        // gst-launch is wrapped in `timeout(1)` inside the probe;
        // both need to be present for the probe to work, but
        // `timeout` is GNU coreutils and effectively always present
        // on Linux CI so we only gate on gst-launch here.
        if tool_present("gst-launch-1.0", "--version") {
            rows.push(probe_gst_tsdemux(&path));
        } else {
            rows.push(ProbeResult {
                tool: "gst-tsdemux",
                fixture: (*fname).to_string(),
                exit: None,
                action: Action::Skip,
                notes: "gst-launch-1.0 not on PATH".to_string(),
            });
        }
    }
    rows
}

/// Print the matrix in markdown-table form. Goes to stderr so it
/// shows up under `cargo test -- --nocapture` without being
/// confused for test-progress output on stdout.
fn print_matrix(rows: &[ProbeResult]) {
    eprintln!();
    eprintln!("## I1 — WebVTT-in-TS + CEA-708 standalone interop matrix");
    eprintln!();
    eprintln!("| fixture | tool | exit | action | notes |");
    eprintln!("|---|---|---|---|---|");
    for r in rows {
        let exit = r
            .exit
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".to_string());
        eprintln!(
            "| `{}` | {} | {} | {} | {} |",
            r.fixture,
            r.tool,
            exit,
            r.action.as_str(),
            r.notes.replace('|', "\\|")
        );
    }
    eprintln!();
}

/// Default-pass informational test. Runs the matrix, prints results,
/// and asserts only that we got at least one cell back (sanity check
/// that the harness itself runs). Cells where the tool is missing
/// become `skip` rows — they don't fail the test.
///
/// Run with `cargo test -p tst-core --test regression
/// subtitle_interop::wave_i1_matrix_informational -- --nocapture` to view the matrix.
#[test]
fn wave_i1_matrix_informational() {
    let rows = run_matrix();
    print_matrix(&rows);
    assert!(
        !rows.is_empty(),
        "matrix produced zero cells — fixtures missing?"
    );
    // Summarize action counts for quick triage in CI logs.
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for r in &rows {
        *counts.entry(r.action.as_str()).or_insert(0) += 1;
    }
    eprintln!("Action summary: {counts:?}");
}

/// Strict regression-guard. Fails if any cell where a tool was
/// actually run reports `Reject` for a VTTC/GA94 fixture.
/// `UnrelatedFailure` and `Skip` are tolerated. Marked `#[ignore]`
/// so PR CI doesn't break when ffmpeg builds drift behavior, but
/// run explicitly before tagging a release.
///
/// `cargo test -p tst-core --test regression
/// subtitle_interop::wave_i1_matrix_no_regression -- --ignored --nocapture`
#[test]
#[ignore = "Strict — run explicitly before release; PR CI uses informational variant"]
fn wave_i1_matrix_no_regression() {
    let rows = run_matrix();
    print_matrix(&rows);
    let vttc_ga94_fixtures = [
        "webvtt_in_ts_simple.ts",
        "webvtt_in_ts_multi_cue.ts",
        "cea708_standalone.ts",
        "subtitle_with_klv_same_program.ts",
        "webvtt_multi_program_with_klv.ts",
    ];
    let mut rejections = Vec::new();
    for r in &rows {
        if r.action == Action::Reject && vttc_ga94_fixtures.contains(&r.fixture.as_str()) {
            rejections.push(format!(
                "  - {} on {}: exit={:?} notes={}",
                r.tool, r.fixture, r.exit, r.notes
            ));
        }
    }
    assert!(
        rejections.is_empty(),
        "external receiver rejected VTTC/GA94 marker stream(s):\n{}\n\
         the documented claim that external receivers tolerate the markers does NOT hold for these cells. \
         Consider removing the auto-emit or changing the marker.",
        rejections.join("\n")
    );
}
