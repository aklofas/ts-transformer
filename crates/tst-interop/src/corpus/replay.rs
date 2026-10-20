//! The replay stage: every capture of the replay subset sent, at its own
//! PCR pace, over every transport path to a `corpus receive` child, which
//! judges what arrived against the sweep's digest.
//!
//! One child per path runs for the whole replay, driven over its stdin and
//! stdout (the protocol in [`super::receive`]). Picks go in plan order and
//! in lockstep: each pick is read once, up to its prefix cut, and every
//! path replays it on its own thread before the next pick starts, so at
//! most one pick's bytes are held. A child that exits early fails its
//! remaining picks as [`ReceiverOutcome::Exited`]; the other paths go on.
//!
//! `srt`, `rist`, `udp` and `tcp` send through a [`Sender`] over
//! [`make_send`]. `rtsp-publish` pipes the same paced bytes into
//! `gst-launch-1.0 fdsrc ! tsparse ! rtspclientsink`, so the publisher
//! sends exactly the cut the digest was made of and no capture path ever
//! leaves the tool. Its output goes to `logs/`.
//!
//! Every wait is bounded. A child gets `READY_WAIT` to answer `file` and
//! `VERDICT_WAIT` to answer `sent`; the paced send itself gets the pick's
//! PCR seconds plus twice the stall time (`send_deadline`). A send that
//! outlives it (a receiver alive but no longer reading, so the sender's
//! writes never complete) is cancelled, the child is killed, the pick is
//! judged `Stalled` and the path's later picks `Exited`.
//!
//! An RSS sampler reads each child's `VmRSS` every `rss_sample_seconds`
//! into `replay-rss.csv`, in the `rss.csv` shape the soak and stress
//! reports read (leg = the path, process = `recv`). A tick that reads no
//! `VmRSS` (a child that has exited, or a read that raced it) records no
//! row; it is never taken for an exit. `VmRSS` comes from `/proc`, so the
//! sampling is Linux-only: elsewhere the CSV holds only its header and the
//! summary's RSS maps are empty.
//!
//! RSS is gated per path by the soak's rule ([`judge_rss`]): a path whose
//! post-warm-up slope exceeds the soak's threshold fails the replay. A path
//! with too few samples for the rule (any short run) is recorded as not
//! gated, with the reason.
//!
//! With `udp` among the paths, the replay refuses to start when
//! `net.core.rmem_max` would clamp the receive buffer the largest pick's
//! access units need ([`super::receive::udp_rcvbuf_for`]).
//!
//! Output, files and errors name captures by SHA-256 only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tst_core::transport::{Transport, TransportCancel};
use tst_pipeline::{Sender, SenderConfig, ShellErrorKind};

use super::digest;
use super::inventory::{self, FileEntry};
use super::judge::{FileVerdict, ReceiverOutcome, outcome_failure};
use super::pace::{PcrPacer, send_paced};
use super::pcr::prefix_end_read;
use super::receive::{STALL_ENV, net_core, udp_rcvbuf_for};
use super::subset::{self, Pick, ReplayPlan};
use super::{PATHS, REPLAY_PICK_MAX_BYTES, RunDir, STALL_SECONDS, Sha256Hex};
use crate::report::csv::RSS_HEADER;
use crate::report::soak::{
    MAX_GAP_CADENCES, MAX_WARMUP_S, MIN_SAMPLE_COVERAGE, RSS_SLOPE_THRESHOLD_KB_PER_HOUR,
    WARMUP_FRACTION, linear_regression_slope, rss_coverage, rss_warmup_s,
};
use crate::transport::make_send;

pub struct ReplayArgs {
    /// The swept corpus directory; every capture the run's inventory names
    /// must lie inside it.
    pub dir: PathBuf,
    pub run: RunDir,
    /// Transport paths to replay over; [`PATHS`] for all of them.
    pub paths: Vec<String>,
    /// Wall-clock seconds the subset may cost; see [`subset::select`].
    pub budget_seconds: u32,
    pub rss_sample_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplaySummary {
    pub schema_version: u32,
    pub plan: ReplayPlan,
    /// Pick by pick in plan order, each pick's verdicts in path order.
    pub verdicts: Vec<FileVerdict>,
    /// Picks replayed.
    pub files: u64,
    pub paths: Vec<String>,
    /// Verdicts that did not pass, plus paths whose RSS gate failed.
    pub failed: u64,
    /// Paths with at least one RSS sample only.
    pub rss_max_kb_by_path: BTreeMap<String, u64>,
    /// Least-squares slope over each path's samples but the first (taken
    /// as the child starts, before it holds anything), the soak report's
    /// arithmetic; 0 with fewer than two such samples.
    pub rss_slope_kb_per_hour_by_path: BTreeMap<String, f64>,
    /// Whether each path's RSS was judged by the soak's slope rule.
    #[serde(default)]
    pub rss_gated_by_path: BTreeMap<String, bool>,
    /// Per path: `PASS …` or `FAIL …` with the judged slope, or
    /// `not gated: …` with the reason (see [`judge_rss`]).
    #[serde(default)]
    pub rss_gate_by_path: BTreeMap<String, String>,
    /// No verdict failed, no path's RSS gate failed, and every path
    /// produced a verdict.
    pub pass: bool,
}

/// What [`judge_rss`] made of each path's RSS samples.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RssJudgement {
    pub gated_by_path: BTreeMap<String, bool>,
    pub gate_by_path: BTreeMap<String, String>,
    /// Gated paths whose slope exceeded the threshold.
    pub failed: Vec<String>,
}

/// Judges each path's RSS the soak's way. The run is as long as its last
/// sample; the soak's warm-up for a run that long is left out; the
/// post-warm-up points must meet the soak's coverage rule at `cadence_s`
/// and span at least the soak's longest warm-up (30 minutes of samples),
/// or the path is recorded as not gated; a gated path fails when its
/// least-squares slope exceeds the soak's threshold. `samples` are
/// `(path, elapsed seconds, kB)`.
pub fn judge_rss(samples: &[(String, f64, u64)], paths: &[String], cadence_s: f64) -> RssJudgement {
    let run_s = samples.iter().map(|(_, t, _)| *t).fold(0.0, f64::max);
    let warmup_s = rss_warmup_s(run_s, WARMUP_FRACTION);
    let expected = ((run_s - warmup_s) / cadence_s).floor().max(0.0) as u64;
    let min_samples = ((expected as f64 * MIN_SAMPLE_COVERAGE).ceil() as u64)
        .max((MAX_WARMUP_S / cadence_s).ceil() as u64);
    let mut out = RssJudgement::default();
    for path in paths {
        let mut points: Vec<(f64, f64)> = samples
            .iter()
            .filter(|(p, t, _)| p == path && *t >= warmup_s)
            .map(|(_, t, kb)| (*t, *kb as f64))
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (covered, largest_gap, _) =
            rss_coverage(&points, min_samples, MAX_GAP_CADENCES * cadence_s);
        let (gated, text) = if covered {
            let slope = linear_regression_slope(&points) * 3600.0;
            let pass = slope <= RSS_SLOPE_THRESHOLD_KB_PER_HOUR;
            if !pass {
                out.failed.push(path.clone());
            }
            (
                true,
                format!(
                    "{} {slope:.1} KiB/h over {} post-warm-up sample(s) (threshold \
                     {RSS_SLOPE_THRESHOLD_KB_PER_HOUR:.0})",
                    if pass { "PASS" } else { "FAIL" },
                    points.len()
                ),
            )
        } else {
            (
                false,
                format!(
                    "not gated: {} post-warm-up sample(s), largest gap {largest_gap:.0} s; \
                     the slope rule needs {min_samples} at a {cadence_s:.0} s cadence",
                    points.len()
                ),
            )
        };
        out.gated_by_path.insert(path.clone(), gated);
        out.gate_by_path.insert(path.clone(), text);
    }
    out
}

/// How long a child may take to answer `file`: it only binds a socket.
const READY_WAIT: Duration = Duration::from_secs(30);
/// How long a child may take to answer `sent`: well past its own stall rule
/// and wind-down, so only a hung child reaches it.
const VERDICT_WAIT: Duration = Duration::from_secs(STALL_SECONDS * 3 + 30);
/// How long a child may take to answer `quit` and exit.
const QUIT_WAIT: Duration = Duration::from_secs(15);
/// The pause between two retries of a send the transport refused for now.
const BACKOFF: Duration = Duration::from_millis(2);
const TICK: Duration = Duration::from_millis(100);

/// The stall time the receivers use: [`STALL_SECONDS`], or the test knob
/// [`STALL_ENV`] the children read too.
fn stall() -> Duration {
    Duration::from_secs(
        std::env::var(STALL_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(STALL_SECONDS),
    )
}

/// How long the paced send of `pacer` may take: its PCR seconds, plus the
/// stall time for a slow start and again for the end.
fn send_deadline(pacer: &PcrPacer) -> Duration {
    Duration::from_secs_f64(pacer.total_seconds()) + 2 * stall()
}

type CancelSlot = Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>;

fn failed(sha: &Sha256Hex, path: &str, why: String) -> FileVerdict {
    FileVerdict {
        sha256: sha.clone(),
        path: path.to_string(),
        pass: false,
        failures: vec![why],
    }
}

fn not_received(sha: &Sha256Hex, path: &str, outcome: ReceiverOutcome) -> FileVerdict {
    failed(
        sha,
        path,
        outcome_failure(&outcome).expect("not a completed outcome"),
    )
}

/// Why a child gave no line.
enum Gone {
    /// It closed its stdout: it exited.
    Exited,
    /// It is alive and silent past the wait.
    Silent,
}

/// One path's `corpus receive` child.
struct PathRun {
    path: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    alive: bool,
}

impl PathRun {
    fn spawn(path: &str, run: &RunDir) -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot find the tool's own executable: {}", e.kind()))?;
        let logs = run.root.join("logs");
        std::fs::create_dir_all(&logs)
            .map_err(|e| format!("cannot create logs in the run directory: {}", e.kind()))?;
        let stderr = File::create(logs.join(format!("receive-{path}.stderr")))
            .map_err(|e| format!("cannot create a receiver log: {}", e.kind()))?;
        let mut child = Command::new(exe)
            .args(["corpus", "receive", "--path", path, "--digests"])
            .arg(run.root.join("digests"))
            .arg("--out")
            .arg(logs.join(format!("receive-{path}.jsonl")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("cannot start the {path} receiver: {}", e.kind()))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            path: path.to_string(),
            stdin: child.stdin.take(),
            child,
            lines,
            alive: true,
        })
    }

    /// Writes one protocol line; `false` when the child can no longer read.
    fn say(&mut self, line: &str) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return false;
        };
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_ok()
    }

    fn next_line(&self, wait: Duration) -> Result<String, Gone> {
        match self.lines.recv_timeout(wait) {
            Ok(l) => Ok(l),
            Err(RecvTimeoutError::Disconnected) => Err(Gone::Exited),
            Err(RecvTimeoutError::Timeout) => Err(Gone::Silent),
        }
    }

    fn kill(&mut self) {
        self.alive = false;
        self.stdin = None;
        let _ = self.child.kill();
    }

    /// `quit`, then waits for the child to leave; kills it if it does not.
    fn finish(&mut self) {
        if self.alive && self.say("quit") {
            let until = Instant::now() + QUIT_WAIT;
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match self.next_line(left) {
                    Ok(l) if l == "bye" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        self.stdin = None;
        let until = Instant::now() + QUIT_WAIT;
        while Instant::now() < until {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(TICK);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The verdict a `verdict <sha> PASS|FAIL <json>` line carries.
    fn parse_verdict(line: &str, sha: &Sha256Hex) -> Option<FileVerdict> {
        let rest = line.strip_prefix(&format!("verdict {sha} "))?;
        let (_, json) = rest.split_once(' ')?;
        serde_json::from_str(json).ok()
    }

    /// One pick over this path: `file`, `ready`, the paced send, `sent`,
    /// `verdict`.
    fn replay(
        &mut self,
        pick: &Pick,
        pacer: &PcrPacer,
        logs: &Path,
        largest_au_bytes: u64,
    ) -> FileVerdict {
        let sha = &pick.sha256;
        let path = self.path.clone();
        if !self.alive {
            return not_received(sha, &path, ReceiverOutcome::Exited);
        }
        let prefix = pick
            .prefix_seconds
            .map_or_else(|| "none".to_string(), |p| p.to_string());
        let largest_chunk = pacer
            .chunks()
            .map(|c| c.bytes.len() as u64)
            .max()
            .unwrap_or(0);
        // The pre-flight saw only the access units; the pacer's chunk is
        // known now, and a buffer the kernel would clamp fails this pick.
        if path == "udp"
            && let (Some(max), Some(n)) = (
                net_core("rmem_max"),
                udp_rcvbuf_for(largest_au_bytes, largest_chunk, net_core("rmem_default")),
            )
            && n > max
        {
            return failed(
                sha,
                &path,
                format!(
                    "the udp path needs a {n}-byte receive buffer but net.core.rmem_max is {max}"
                ),
            );
        }
        if !self.say(&format!(
            "file {sha} {prefix} {largest_au_bytes} {largest_chunk}"
        )) {
            self.alive = false;
            return not_received(sha, &path, ReceiverOutcome::Exited);
        }
        let ready = match self.answer(sha, READY_WAIT) {
            Ok(l) => l,
            Err(v) => return v,
        };
        // A file the child could not prepare is judged in place of `ready`.
        if let Some(v) = Self::parse_verdict(&ready, sha) {
            return v;
        }
        let Some(url) = ready.strip_prefix(&format!("ready {sha} ")) else {
            self.kill();
            return failed(sha, &path, "receiver protocol error".into());
        };
        let deadline = send_deadline(pacer);
        let log = logs.join(format!("publish-{sha}.log"));
        let cancel: CancelSlot = Mutex::new(None);
        let sent = std::thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            let (path, cancel, log) = (&path, &cancel, &log);
            s.spawn(move || {
                let _ = tx.send(send(path, url, pacer, log, cancel));
            });
            match rx.recv_timeout(deadline) {
                Ok(r) => Some(r),
                Err(_) => {
                    // Closing both ends makes a blocked write fail, so the
                    // send thread returns and the scope can join it.
                    if let Some(c) = cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        c.cancel();
                    }
                    self.kill();
                    None
                }
            }
        });
        let Some((sent, resyncs)) = sent else {
            let mut v = not_received(sha, &path, ReceiverOutcome::Stalled);
            v.failures.insert(
                0,
                format!(
                    "sender deadline: the send outlived {} s",
                    deadline.as_secs()
                ),
            );
            return v;
        };
        // A path that ends on the sender's close may already have judged
        // the file, and a child told to leave after it may be gone: the
        // verdict is read either way.
        let _ = self.say(&format!("sent {sha}"));
        let line = match self.answer(sha, VERDICT_WAIT) {
            Ok(l) => l,
            Err(v) => return v,
        };
        let Some(mut v) = Self::parse_verdict(&line, sha) else {
            self.kill();
            return failed(sha, &path, "receiver protocol error".into());
        };
        if let Err(e) = sent {
            v.pass = false;
            v.failures.insert(0, format!("sender error: {e}"));
        }
        // A sender that lost framing dropped what it held: name it, so a
        // short delivery is not read as transport loss.
        if !v.pass && resyncs.events > 0 {
            v.failures.push(format!(
                "sender resynced {} times, skipped {} bytes",
                resyncs.events, resyncs.bytes_skipped
            ));
        }
        v
    }

    /// The child's next line, or the verdict for a child that gave none.
    fn answer(&mut self, sha: &Sha256Hex, wait: Duration) -> Result<String, FileVerdict> {
        match self.next_line(wait) {
            Ok(l) => Ok(l),
            Err(Gone::Exited) => {
                self.alive = false;
                Err(not_received(sha, &self.path, ReceiverOutcome::Exited))
            }
            Err(Gone::Silent) => {
                self.kill();
                Err(not_received(sha, &self.path, ReceiverOutcome::Stalled))
            }
        }
    }
}

/// The sender's framing recoveries during one send.
#[derive(Debug, Clone, Copy, Default)]
struct Resyncs {
    events: u64,
    bytes_skipped: u64,
}

/// Sends the paced capture to the URL a child announced. The sender's
/// cancel handle goes into `cancel` as soon as it exists.
fn send(
    path: &str,
    url: &str,
    pacer: &PcrPacer,
    publish_log: &Path,
    cancel: &CancelSlot,
) -> (Result<(), String>, Resyncs) {
    if path == "rtsp-publish" {
        return (publish(url, pacer, publish_log), Resyncs::default());
    }
    let mut sender = match make_send(url) {
        Ok(t) => Sender::new(t, SenderConfig::default()),
        Err(e) => return (Err(e), Resyncs::default()),
    };
    *cancel.lock().unwrap_or_else(|e| e.into_inner()) = sender.cancel_handle();
    let start = Instant::now();
    let r = send_paced(
        pacer,
        &mut || start.elapsed(),
        &mut std::thread::sleep,
        |chunk| push(&mut sender, chunk),
    );
    let stats = sender.stats();
    sender.close();
    (
        r,
        Resyncs {
            events: stats.resync_events,
            bytes_skipped: stats.bytes_skipped_for_sync,
        },
    )
}

/// One chunk through the sender, all of it on the wire before the next
/// chunk is due. A send the transport refuses for now is retried for up to
/// [`STALL_SECONDS`]: `send_ts` again when the chunk was not taken in
/// (`input_consumed == Some(false)`), else `flush`, which drains what the
/// sender kept.
fn push<T: Transport>(sender: &mut Sender<T>, chunk: &[u8]) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(STALL_SECONDS);
    let mut taken = false;
    loop {
        let r = if taken {
            sender.flush()
        } else {
            match sender.send_ts(chunk) {
                Ok(()) => {
                    taken = true;
                    sender.flush()
                }
                Err(e) => {
                    taken = e.input_consumed != Some(false);
                    Err(e)
                }
            }
        };
        match r {
            Ok(()) => return Ok(()),
            Err(e) if e.kind == ShellErrorKind::Backpressure && Instant::now() < until => {
                std::thread::sleep(BACKOFF);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `rtsp-publish`: GStreamer RECORDs the paced bytes from its stdin into
/// the child's mount, under `timeout` so a wedged publisher cannot outlive
/// the replay.
fn publish(url: &str, pacer: &PcrPacer, log: &Path) -> Result<(), String> {
    let rtsp = url.replacen("rtsp-publish://", "rtsp://", 1);
    let limit = pacer.total_seconds().ceil() as u64 + 60;
    let out =
        File::create(log).map_err(|e| format!("cannot create a publisher log: {}", e.kind()))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("cannot create a publisher log: {}", e.kind()))?;
    let mut child = Command::new("timeout")
        .args(["--kill-after=5", &limit.to_string(), "gst-launch-1.0"])
        .args(["fdsrc", "fd=0", "!", "tsparse", "set-timestamps=true", "!"])
        .args([
            "rtspclientsink",
            &format!("location={rtsp}"),
            "protocols=tcp",
        ])
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("cannot start the publisher: {}", e.kind()))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let start = Instant::now();
    let r = send_paced(
        pacer,
        &mut || start.elapsed(),
        &mut std::thread::sleep,
        |chunk| {
            stdin
                .write_all(chunk)
                .map_err(|e| format!("the publisher stopped reading: {}", e.kind()))
        },
    );
    drop(stdin);
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for the publisher: {}", e.kind()))?;
    r?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the publisher exited with {status}"))
    }
}

/// Exits non-zero or cannot start: either way the tool is missing.
fn runs_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The publisher's tools, checked once before anything is replayed.
fn check_publisher() -> Result<(), String> {
    let needs: [(&str, &[&str], &str); 4] = [
        ("timeout", &["--version"], "timeout (package coreutils)"),
        (
            "gst-launch-1.0",
            &["--version"],
            "gst-launch-1.0 (package gstreamer1.0-tools)",
        ),
        (
            "gst-inspect-1.0",
            &["rtspclientsink"],
            "rtspclientsink (package gstreamer1.0-rtsp)",
        ),
        (
            "gst-inspect-1.0",
            &["tsparse"],
            "tsparse (package gstreamer1.0-plugins-bad)",
        ),
    ];
    for (program, args, what) in needs {
        if !runs_ok(program, args) {
            return Err(format!(
                "the rtsp-publish path needs {what}; install it or leave rtsp-publish out of --paths"
            ));
        }
    }
    Ok(())
}

/// Refuses a udp replay whose receive buffer the kernel would clamp
/// silently (`setsockopt` caps `SO_RCVBUF` at `net.core.rmem_max`), as the
/// stress harness does, judged from the inventory's access units; a pick
/// whose pacer chunk needs more fails its udp verdict when it is replayed.
/// Unreadable sysctls (not Linux) pass.
fn check_rmem_max(
    plan: &ReplayPlan,
    entries: &BTreeMap<&Sha256Hex, &FileEntry>,
) -> Result<(), String> {
    let Some(max) = net_core("rmem_max") else {
        return Ok(());
    };
    let default = net_core("rmem_default");
    let needed = plan
        .picks
        .iter()
        .filter_map(|p| entries.get(&p.sha256))
        .filter_map(|e| udp_rcvbuf_for(e.largest_au_bytes, 0, default))
        .max();
    match needed {
        Some(n) if n > max => Err(format!(
            "the udp path needs a {n}-byte receive buffer but net.core.rmem_max is {max}; run \
             `sudo sysctl -w net.core.rmem_max={n}` first, or leave udp out of --paths"
        )),
        _ => Ok(()),
    }
}

/// A child's resident set in kB; `None` when its status has no `VmRSS`.
fn vm_rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// (path, elapsed seconds, kB) per sample.
type Samples = Vec<(String, f64, u64)>;

/// Samples every child now and then every `every` until `stop`, appending
/// each sample to `csv` as it is taken.
fn sample_rss(
    mut csv: File,
    procs: Vec<(String, u32)>,
    every: Duration,
    stop: Arc<AtomicBool>,
) -> Samples {
    let epoch = Instant::now();
    let mut samples = Vec::new();
    loop {
        let t = epoch.elapsed().as_secs_f64();
        for (path, pid) in &procs {
            if let Some(kb) = vm_rss_kb(*pid) {
                let _ = writeln!(csv, "{t:.3},{path},recv,{pid},{kb}");
                samples.push((path.clone(), t, kb));
            }
        }
        let _ = csv.flush();
        let next = epoch + Duration::from_secs_f64(t) + every;
        while Instant::now() < next {
            if stop.load(Ordering::Relaxed) {
                return samples;
            }
            std::thread::sleep(TICK.min(every));
        }
    }
}

/// Reads a pick's bytes, the whole capture or up to its prefix cut, into
/// one allocation of exactly that size. A whole capture must still have the
/// sha the plan names (a prefixed one was checked by its prefix digest).
fn read_pick(file: &Path, sha: &Sha256Hex, prefix_seconds: Option<u32>) -> Result<Vec<u8>, String> {
    let read_err = |e: std::io::Error| format!("cannot read the capture: {}", e.kind());
    let open = || File::open(file).map_err(|e| format!("cannot open the capture: {}", e.kind()));
    let mut f = open()?;
    let end = match prefix_seconds {
        Some(s) => prefix_end_read(&mut f, s).map_err(read_err)?,
        None => f.metadata().map_err(read_err)?.len(),
    };
    // The plan's cap is on a prediction; the real cut can be longer.
    if end > REPLAY_PICK_MAX_BYTES {
        return Err(format!(
            "skipped: the pick is {end} bytes, over the pick size cap of {REPLAY_PICK_MAX_BYTES}"
        ));
    }
    let end = usize::try_from(end).map_err(|_| "the capture does not fit in memory".to_string())?;
    let mut bytes = vec![0u8; end];
    open()?.read_exact(&mut bytes).map_err(read_err)?;
    if prefix_seconds.is_none() {
        let got: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != sha.0 {
            return Err("content changed since the inventory".into());
        }
    }
    Ok(bytes)
}

/// Every path's verdict for one pick.
fn replay_pick(
    pick: &Pick,
    entry: Option<&FileEntry>,
    runs: &mut [PathRun],
    run: &RunDir,
) -> Vec<FileVerdict> {
    let fail_all = |runs: &[PathRun], why: String| -> Vec<FileVerdict> {
        runs.iter()
            .map(|r| failed(&pick.sha256, &r.path, why.clone()))
            .collect()
    };
    let Some(file) = entry.and_then(|e| e.paths.first()) else {
        return fail_all(runs, "not in the inventory".into());
    };
    if let Some(p) = pick.prefix_seconds {
        let stored = digest::digest_file(Some(&pick.sha256), file, Some(p))
            .and_then(|d| digest::store(run, &d));
        if let Err(e) = stored {
            return fail_all(runs, format!("prefix digest: {e}"));
        }
    }
    let bytes = match read_pick(file, &pick.sha256, pick.prefix_seconds) {
        Ok(b) => b,
        Err(e) => return fail_all(runs, e),
    };
    // The bytes already end at the cut, so the pacer takes them whole.
    let pacer = match PcrPacer::new(&bytes, None) {
        Ok(p) => p,
        Err(e) => return fail_all(runs, e),
    };
    let logs = run.root.join("logs");
    let largest_au = entry.map_or(0, |e| e.largest_au_bytes);
    std::thread::scope(|s| {
        let threads: Vec<_> = runs
            .iter_mut()
            .map(|r| s.spawn(|| r.replay(pick, &pacer, &logs, largest_au)))
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("a path thread does not panic"))
            .collect()
    })
}

fn check_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("--paths names no path".into());
    }
    let mut seen = BTreeSet::new();
    for p in paths {
        if !PATHS.contains(&p.as_str()) {
            return Err(format!(
                "unknown path '{p}' (want some of {})",
                PATHS.join(",")
            ));
        }
        if !seen.insert(p) {
            return Err(format!("path '{p}' is named twice"));
        }
    }
    Ok(())
}

/// [`run_with`] without a per-verdict callback.
pub fn run(args: &ReplayArgs) -> Result<ReplaySummary, String> {
    run_with(args, &mut |_| {})
}

/// Fails when a capture the inventory names lies outside `dir`: the run
/// directory was swept from another corpus. A file that cannot be resolved
/// is left to its pick to report.
fn check_dir(dir: &Path, inv: &inventory::Inventory) -> Result<(), String> {
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("cannot open the corpus directory: {}", e.kind()))?;
    let outside = inv
        .files
        .iter()
        .filter_map(|f| f.paths.first()?.canonicalize().ok())
        .any(|p| !p.starts_with(&dir));
    if outside {
        return Err("the run directory was swept from another corpus directory".into());
    }
    Ok(())
}

/// Replays the subset of the swept corpus and writes `replay-plan.json`,
/// `replay.json` and `replay-rss.csv` into the run directory, calling
/// `on_verdict` as each verdict lands. Needs the sweep's `digests/` and
/// `inventory.json` in the run directory. The receivers are this
/// executable's own `corpus receive`, so it is meant to run from the
/// `tst-interop` binary.
pub fn run_with(
    args: &ReplayArgs,
    on_verdict: &mut dyn FnMut(&FileVerdict),
) -> Result<ReplaySummary, String> {
    check_paths(&args.paths)?;
    if !args.run.root.join("digests").is_dir() {
        return Err("the run directory holds no digests: run the sweep first".into());
    }
    let inv = inventory::read(&args.run)?;
    check_dir(&args.dir, &inv)?;
    if args.paths.iter().any(|p| p == "rtsp-publish") {
        check_publisher()?;
    }
    let plan = subset::select(&inv, &args.paths, args.budget_seconds);
    subset::write(&plan, &args.run)?;
    let entries: BTreeMap<&Sha256Hex, &FileEntry> =
        inv.files.iter().map(|f| (&f.sha256, f)).collect();
    if args.paths.iter().any(|p| p == "udp") {
        check_rmem_max(&plan, &entries)?;
    }

    let csv_path = args.run.root.join("replay-rss.csv");
    let mut csv = File::create(&csv_path).map_err(|e| {
        format!(
            "cannot create replay-rss.csv in the run directory: {}",
            e.kind()
        )
    })?;
    writeln!(csv, "{RSS_HEADER}")
        .map_err(|e| format!("cannot write replay-rss.csv: {}", e.kind()))?;

    let mut runs: Vec<PathRun> = Vec::new();
    for path in &args.paths {
        match PathRun::spawn(path, &args.run) {
            Ok(r) => runs.push(r),
            Err(e) => {
                runs.iter_mut().for_each(PathRun::finish);
                return Err(e);
            }
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let procs = runs
            .iter()
            .map(|r| (r.path.clone(), r.child.id()))
            .collect();
        let every = Duration::from_secs(args.rss_sample_seconds.max(1));
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || sample_rss(csv, procs, every, stop))
    };

    let mut verdicts = Vec::new();
    for pick in &plan.picks {
        for v in replay_pick(
            pick,
            entries.get(&pick.sha256).copied(),
            &mut runs,
            &args.run,
        ) {
            on_verdict(&v);
            verdicts.push(v);
        }
    }
    runs.iter_mut().for_each(PathRun::finish);
    stop.store(true, Ordering::Relaxed);
    let samples = sampler.join().unwrap_or_default();

    let mut rss_max_kb_by_path = BTreeMap::new();
    let mut rss_slope_kb_per_hour_by_path = BTreeMap::new();
    for path in &args.paths {
        let points: Vec<(f64, f64)> = samples
            .iter()
            .filter(|(p, _, _)| p == path)
            .map(|(_, t, kb)| (*t, *kb as f64))
            .collect();
        if points.is_empty() {
            continue;
        }
        let max = points.iter().map(|(_, kb)| *kb as u64).max().unwrap_or(0);
        rss_max_kb_by_path.insert(path.clone(), max);
        // The first sample is the child at spawn, before it allocates.
        rss_slope_kb_per_hour_by_path
            .insert(path.clone(), linear_regression_slope(&points[1..]) * 3600.0);
    }
    let rss = judge_rss(&samples, &args.paths, args.rss_sample_seconds.max(1) as f64);
    let failed = verdicts.iter().filter(|v| !v.pass).count() as u64 + rss.failed.len() as u64;
    let every_path_judged = args
        .paths
        .iter()
        .all(|p| verdicts.iter().any(|v| &v.path == p));
    let summary = ReplaySummary {
        schema_version: 1,
        files: plan.picks.len() as u64,
        paths: args.paths.clone(),
        failed,
        pass: failed == 0 && every_path_judged,
        plan,
        verdicts,
        rss_max_kb_by_path,
        rss_slope_kb_per_hour_by_path,
        rss_gated_by_path: rss.gated_by_path,
        rss_gate_by_path: rss.gate_by_path,
    };
    let text = serde_json::to_string_pretty(&summary).map_err(|e| format!("replay json: {e}"))?;
    std::fs::write(args.run.root.join("replay.json"), text).map_err(|e| {
        format!(
            "cannot write replay.json in the run directory: {}",
            e.kind()
        )
    })?;
    Ok(summary)
}

// The only unit test here reads a sparse file's length through the Linux
// pick-size check, so the whole module is Linux-only; a bare `cfg(test)`
// would leave `use super::*` unused on macOS and Windows, which clippy
// rejects under `-D warnings`.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_pick_over_the_size_cap_is_refused_before_it_is_read() {
        let dir = std::env::temp_dir().join(format!("tst-corpus-pickcap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.ts");
        // Sparse: the length is all the check reads.
        File::create(&file)
            .unwrap()
            .set_len(REPLAY_PICK_MAX_BYTES + 188)
            .unwrap();
        let e = read_pick(&file, &Sha256Hex("0".repeat(64)), None).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(e.starts_with("skipped: "), "{e}");
        assert!(e.contains("over the pick size cap"), "{e}");
    }
}
