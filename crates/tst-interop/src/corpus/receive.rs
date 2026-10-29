//! The receiving child of a corpus replay: one process per transport path,
//! driven over stdin and stdout, one UTF-8 line per message.
//!
//! ```text
//! parent: file <sha256> <prefix_seconds|none> <largest_au_bytes> <largest_chunk_bytes>
//! child : ready <sha256> <url the sender uses>
//! parent: sent <sha256>                (the sender finished and closed)
//! child : verdict <sha256> PASS|FAIL <FileVerdict as one line of JSON>
//! parent: quit
//! child : bye                          (exit 0)
//! ```
//!
//! The prefix on a `file` line is the sender's business (it cuts the replay
//! there) and names the digest to judge by; the child digests every byte
//! that arrives, so a replay that overshoots the prefix fails against the
//! prefix digest. `largest_au_bytes` is the inventory's largest video
//! access unit and `largest_chunk_bytes` the largest PCR interval the
//! sender's pacer writes at once (0 when unknown); the `udp` child sizes its
//! socket buffer from both ([`udp_rcvbuf_for`]), reads back what the kernel
//! granted, and, when the granted buffer is smaller than the request, adds
//! `receive buffer <granted> < requested <requested>` to a failed verdict.
//!
//! The expected digest is read from `<digests>/<sha256>.json`, or from
//! `<digests>/<sha256>.prefix<seconds>.json` for a `file` with a prefix
//! (the name [`digest::file_name`] gives; the parent writes it). A `file`
//! that cannot be prepared (no digest, a listener that will not bind)
//! answers with a FAIL verdict in place of `ready`. Each verdict is also
//! appended, as the same JSON, to the `--out` file. A protocol error exits 2
//! and names only the kind of line that was wrong. No path is ever printed:
//! the digests directory is an argument, the sha names the capture.
//!
//! # What the sender uses
//!
//! The child listens on a kernel-picked loopback port and prints the URL of
//! the matching sender, which the parent opens with `transport::make_send`:
//!
//! | path | the child binds | `ready` prints |
//! |---|---|---|
//! | `udp` | `udp://127.0.0.1:0?rcvbuf=<n>`, `n` per [`udp_rcvbuf_for`] on every platform | `udp://127.0.0.1:<port>` |
//! | `tcp` | `tcp://127.0.0.1:0?listen=1` | `tcp://127.0.0.1:<port>` (the sender is the caller) |
//! | `srt` | `srt://127.0.0.1:0?mode=listener` | `srt://127.0.0.1:<port>` (the sender is the caller) |
//! | `rist` | `rist://@127.0.0.1:<even port>` | `rist://127.0.0.1:<port>` |
//! | `rtsp-publish` | `rtsp-publish://127.0.0.1:0/corpus` | `rtsp-publish://127.0.0.1:<port>/corpus` |
//!
//! # How a replay ends
//!
//! Received bytes feed a [`digest::Incremental`] as they arrive; nothing
//! accumulates. `tcp` and `srt` end when the sender closes, so after `sent`
//! the child waits for that close. `udp`, `rist` and `rtsp-publish` have no
//! end of stream (a mount stays open across publishers), so after `sent`
//! they end once [`QUIET_SECONDS`] pass without a byte. A receiver is
//! `Stalled` when [`STALL_SECONDS`] pass without a byte before `sent`
//! (including the wait for the first one), or when, after `sent`, that long
//! passes without the end the path needs, however many bytes still arrive. `Exited` is never set here: only the
//! parent knows its child died.
//!
//! The receive loop runs on its own thread so a blocking `recv` (`tcp` has
//! no read timeout) can always be cancelled from the control loop.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tst_core::transport::{TransportCancel, TransportError};

use super::digest::{self, FileDigest, Incremental};
use super::judge::{FileVerdict, ReceiverOutcome, judge};
use super::{STALL_SECONDS, Sha256Hex};
use crate::transport::{BoundRecv, make_recv_bound};

/// After `sent`, how long a path without an end of stream stays quiet
/// before the replay is over.
pub const QUIET_SECONDS: u64 = 2;

/// Test hook: `<n>` makes the child exit 0 straight after its `n`-th
/// verdict line; `<path>:<n>` does so only in the child for that path, so a
/// whole replay can carry the variable and lose one receiver. Never set
/// outside tests.
pub const DIE_AFTER_ENV: &str = "TST_CORPUS_TEST_RECEIVE_DIE_AFTER";

/// Test hook: set to a path name, that path's child accepts its sender and
/// then never reads from it, holding the connection open, so the sender's
/// writes stop completing. On `tcp` it also asks for small socket buffers
/// (the listener's `rcvbuf`, the announced sender's `sndbuf`), so the
/// writes block after a few kilobytes. Never set outside tests.
pub const STOP_READING_ENV: &str = "TST_CORPUS_TEST_RECEIVE_STOP_READING";

/// The socket buffer [`STOP_READING_ENV`] asks for on `tcp`.
const STOP_READING_BUF: &str = "4096";

/// The verdict count [`DIE_AFTER_ENV`] sets for the child of `path`.
fn die_after(path: &str, value: &str) -> Option<u64> {
    let n = match value.split_once(':') {
        Some((p, n)) if p == path => n,
        Some(_) => return None,
        None => value,
    };
    n.parse().ok().filter(|&n| n > 0)
}

/// Test hook: whole seconds that replace [`STALL_SECONDS`] (and cap
/// [`QUIET_SECONDS`], so the quiet window never outlasts the stall rule),
/// so a stall can be provoked in a test without a ten-second wait. Never set
/// outside tests.
pub const STALL_ENV: &str = "TST_CORPUS_TEST_STALL_SECONDS";

/// How many bursts a `udp` receiver's socket buffer is asked to hold. Linux
/// stores twice the value set (the other half pays for bookkeeping), so two
/// bursts is the stress harness's rule there. macOS stores the value set
/// exactly and charges each datagram's mbuf (about 2 KB for a 1 316-byte
/// datagram) against it, and Windows has its own accounting, so elsewhere
/// the request is four bursts.
pub const UDP_RCVBUF_BURST_FACTOR: u64 = if cfg!(target_os = "linux") { 2 } else { 4 };

/// The smallest receive buffer a `udp` receiver asks for: 256 KiB on Linux
/// (stored as 512 KiB), 1 MiB elsewhere. The request never depends on a
/// platform's default buffer, which differs widely between kernels.
pub const UDP_RCVBUF_FLOOR: u64 = if cfg!(target_os = "linux") {
    256 * 1024
} else {
    1024 * 1024
};

/// Test hook: a byte count the `udp` child puts on its socket in place of
/// [`udp_rcvbuf_for`]'s request, which stays the size the verdict compares
/// the granted buffer with. A tiny value stands for a kernel that grants
/// less than the replay needs. Never set outside tests.
pub const RCVBUF_ENV: &str = "TST_CORPUS_TEST_RCVBUF";

/// The receive buffer, in bytes, the bursts of a capture need: the burst is
/// the larger of the access unit of `largest_au_bytes` as 188-byte packets
/// of 184 payload bytes (the stress harness's rule) and the
/// `largest_chunk_bytes` the pacer writes in one go (an access unit plus
/// the audio and KLV beside it), times [`UDP_RCVBUF_BURST_FACTOR`]. 0 when
/// both sizes are unknown (0).
pub fn udp_rcvbuf_need(largest_au_bytes: u64, largest_chunk_bytes: u64) -> u64 {
    (largest_au_bytes.div_ceil(184) * 188).max(largest_chunk_bytes) * UDP_RCVBUF_BURST_FACTOR
}

/// The receive buffer, in bytes, a `udp` receiver asks for (`?rcvbuf=`) to
/// absorb a burst sent at line rate: [`udp_rcvbuf_need`], at least
/// [`UDP_RCVBUF_FLOOR`]. Every replay sets it, on every platform.
pub fn udp_rcvbuf_for(largest_au_bytes: u64, largest_chunk_bytes: u64) -> u64 {
    udp_rcvbuf_need(largest_au_bytes, largest_chunk_bytes).max(UDP_RCVBUF_FLOOR)
}

/// A `net.core` sysctl in bytes; `None` where it cannot be read (not Linux).
pub fn net_core(name: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/sys/net/core/{name}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

const TICK: Duration = Duration::from_millis(100);
/// How long a cancelled receiver gets to wind down before it is left behind.
const WIND_DOWN: Duration = Duration::from_secs(5);
const READ_BUF: usize = 64 * 1024;

enum Cmd {
    File(Sha256Hex, Option<u32>, u64, u64),
    Sent(Sha256Hex),
    Quit,
    /// The parent closed stdin.
    Eof,
    /// A line that is not part of the protocol; names its kind only.
    Bad(&'static str),
}

enum Event {
    Cmd(Cmd),
    /// A receiver thread's result, tagged with the replay it belongs to so a
    /// thread left behind by a stalled replay cannot answer a later one.
    Done(u64, Result<FileDigest, String>),
}

fn parse_sha(s: &str) -> Option<Sha256Hex> {
    (s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| Sha256Hex(s.to_string()))
}

fn parse_line(line: &str) -> Cmd {
    let words: Vec<&str> = line.split_whitespace().take(7).collect();
    match words.as_slice() {
        ["file", sha, prefix, au, chunk] => {
            let prefix = match *prefix {
                "none" => Some(None),
                n => n.parse::<u32>().ok().map(Some),
            };
            match (
                parse_sha(sha),
                prefix,
                au.parse::<u64>(),
                chunk.parse::<u64>(),
            ) {
                (Some(sha), Some(p), Ok(au), Ok(chunk)) => Cmd::File(sha, p, au, chunk),
                _ => Cmd::Bad("malformed file line"),
            }
        }
        ["file", ..] => Cmd::Bad("malformed file line"),
        ["sent", sha] => parse_sha(sha).map_or(Cmd::Bad("malformed sent line"), Cmd::Sent),
        ["quit"] => Cmd::Quit,
        _ => Cmd::Bad("unrecognised line"),
    }
}

/// The bind URL and the function that turns the bound address into the sender URL.
type PathUrls = (&'static str, fn(std::net::SocketAddr) -> String);

fn urls(path: &str) -> Option<PathUrls> {
    Some(match path {
        "udp" => ("udp://127.0.0.1:0", |a| format!("udp://{a}")),
        "tcp" => ("tcp://127.0.0.1:0?listen=1", |a| format!("tcp://{a}")),
        "srt" => ("srt://127.0.0.1:0?mode=listener&conntimeo=10000", |a| {
            format!("srt://{a}")
        }),
        "rist" => ("rist://@127.0.0.1:0", |a| format!("rist://{a}")),
        "rtsp-publish" => ("rtsp-publish://127.0.0.1:0/corpus", |a| {
            format!("rtsp-publish://{a}/corpus")
        }),
        _ => return None,
    })
}

/// Whether the path signals the end of a stream (the sender closing).
fn has_end(path: &str) -> bool {
    matches!(path, "tcp" | "srt")
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn protocol_error(kind: &str) -> ! {
    eprintln!("corpus receive: protocol error: {kind}");
    std::process::exit(2);
}

/// Bytes seen so far and when the last arrived, shared with the receiver
/// thread.
struct Progress {
    epoch: Instant,
    bytes: AtomicU64,
    last_ms: AtomicU64,
    stop: AtomicBool,
    cancels: Mutex<Vec<Arc<dyn TransportCancel + Send + Sync>>>,
}

impl Progress {
    fn touch(&self) {
        self.last_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn since_last(&self) -> Duration {
        self.epoch
            .elapsed()
            .saturating_sub(Duration::from_millis(self.last_ms.load(Ordering::Relaxed)))
    }

    fn cancel_all(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for c in self
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            c.cancel();
        }
    }

    fn register(&self, c: Arc<dyn TransportCancel + Send + Sync>) {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&c));
        // A cancel that came before this handle existed still has to land.
        if self.stop.load(Ordering::Relaxed) {
            c.cancel();
        }
    }
}

/// Accepts, then drains the transport into an [`Incremental`] until it
/// ends. Any end the transport reports counts as the stream ending; whether
/// everything arrived is the digest's to say.
fn receive(
    bound: BoundRecv,
    progress: &Progress,
    stop_reading: bool,
) -> Result<FileDigest, String> {
    let mut transport = bound.into_transport()?;
    if stop_reading {
        // Hold the connection open and never read, until the process ends.
        loop {
            std::thread::park();
        }
    }
    if let Some(c) = transport.cancel_handle() {
        progress.register(c);
    }
    let mut inc = Incremental::new(None);
    let mut buf = vec![0u8; READ_BUF];
    while !progress.stop.load(Ordering::Relaxed) {
        match transport.recv_bytes(&mut buf) {
            // A non-empty buffer never yields `Ok(0)`: it is the end of the stream.
            Ok(0) => break,
            Ok(n) => {
                inc.feed(&buf[..n]);
                progress.bytes.fetch_add(n as u64, Ordering::Relaxed);
                progress.touch();
            }
            Err(TransportError::Backpressure { .. }) => {}
            Err(TransportError::TooLarge { .. }) => {
                return Err("a message was larger than the receive buffer".to_string());
            }
            Err(_) => break,
        }
    }
    inc.finish()
}

fn load_expected(
    digests: &Path,
    sha: &Sha256Hex,
    prefix: Option<u32>,
) -> Result<FileDigest, String> {
    let text = std::fs::read_to_string(digests.join(digest::file_name(sha, prefix)))
        .map_err(|e| format!("cannot read the sweep digest: {}", e.kind()))?;
    serde_json::from_str(&text).map_err(|e| format!("cannot parse the sweep digest: {e}"))
}

fn failed(sha: &Sha256Hex, path: &str, why: String) -> FileVerdict {
    FileVerdict {
        sha256: sha.clone(),
        path: path.to_string(),
        pass: false,
        failures: vec![why],
    }
}

struct Child<'a> {
    path: &'a str,
    digests: &'a Path,
    out: std::fs::File,
    /// [`STOP_READING_ENV`] names this path.
    stop_reading: bool,
    /// Exit after this many verdicts ([`DIE_AFTER_ENV`]).
    die_after: Option<u64>,
    verdicts: u64,
    rx: mpsc::Receiver<Event>,
    tx: Sender<Event>,
    last_verdict: Option<Sha256Hex>,
    replays: u64,
    stall: Duration,
    quiet: Duration,
    /// [`RCVBUF_ENV`]: the `udp` socket's buffer in place of the request.
    rcvbuf_override: Option<u64>,
    /// Commands that arrived while a replay was winding down.
    pending: VecDeque<Cmd>,
}

impl Child<'_> {
    fn verdict(&mut self, v: &FileVerdict) {
        let json = serde_json::to_string(v).expect("a verdict is plain data");
        // The record is on disk before the line announces it. One write per
        // record: a child killed mid-record cannot leave half a line for the
        // next child on the path to append to.
        let _ = self.out.write_all(format!("{json}\n").as_bytes());
        let _ = self.out.flush();
        say(&format!(
            "verdict {} {} {json}",
            v.sha256,
            if v.pass { "PASS" } else { "FAIL" }
        ));
        self.last_verdict = Some(v.sha256.clone());
        self.verdicts += 1;
        if self.die_after == Some(self.verdicts) {
            std::process::exit(0);
        }
    }

    /// One `file`: bind, announce, receive, judge.
    fn replay(&mut self, sha: Sha256Hex, prefix: Option<u32>, largest_au: u64, largest_chunk: u64) {
        let path = self.path;
        let expected = match load_expected(self.digests, &sha, prefix) {
            Ok(d) => d,
            Err(e) => return self.verdict(&failed(&sha, path, e)),
        };
        let (bind_url, sender_url) = urls(path).expect("the path was checked at start");
        let small_tcp = self.stop_reading && path == "tcp";
        let udp_request = (path == "udp").then(|| udp_rcvbuf_for(largest_au, largest_chunk));
        let bind_url = if small_tcp {
            format!("{bind_url}&rcvbuf={STOP_READING_BUF}")
        } else if let Some(n) = udp_request {
            format!("{bind_url}?rcvbuf={}", self.rcvbuf_override.unwrap_or(n))
        } else {
            bind_url.to_string()
        };
        let bound = match make_recv_bound(&bind_url) {
            Ok(b) => b,
            Err(e) => return self.verdict(&failed(&sha, path, format!("cannot listen: {e}"))),
        };
        // (requested, granted) when the kernel granted less than requested.
        let short_buffer = udp_request.and_then(|requested| {
            let granted = bound.recv_buffer_size()?;
            eprintln!("corpus receive: udp receive buffer requested {requested} granted {granted}");
            (granted < requested).then_some((requested, granted))
        });
        let mut announce = sender_url(bound.addr());
        if small_tcp {
            announce.push_str(&format!("?sndbuf={STOP_READING_BUF}"));
        }
        self.replays += 1;
        let progress = Arc::new(Progress {
            epoch: Instant::now(),
            bytes: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            cancels: Mutex::new(Vec::new()),
        });
        if let Some(c) = bound.cancel_handle() {
            progress.register(c);
        }
        let worker = {
            let progress = Arc::clone(&progress);
            let tx = self.tx.clone();
            let id = self.replays;
            let stop_reading = self.stop_reading;
            std::thread::spawn(move || {
                let _ = tx.send(Event::Done(id, receive(bound, &progress, stop_reading)));
            })
        };
        say(&format!("ready {sha} {announce}"));
        progress.touch();

        let mut sent_at: Option<Instant> = None;
        let mut stalled = false;
        let mut quit = false;
        let done = loop {
            match self.rx.recv_timeout(TICK) {
                Ok(Event::Done(id, r)) if id == self.replays => break Some(r),
                Ok(Event::Cmd(Cmd::Sent(s))) if s == sha => sent_at = Some(Instant::now()),
                Ok(Event::Cmd(Cmd::Sent(_))) => protocol_error("sent for another file"),
                Ok(Event::Cmd(Cmd::File(..))) => protocol_error("file while receiving"),
                Ok(Event::Cmd(Cmd::Bad(kind))) => protocol_error(kind),
                Ok(Event::Done(..)) => {}
                Ok(Event::Cmd(Cmd::Quit)) => {
                    quit = true;
                    break self.wind_down(&progress);
                }
                // The parent closed stdin: this file is still judged, then the child leaves.
                Ok(Event::Cmd(Cmd::Eof)) => self.pending.push_back(Cmd::Eof),
                Err(RecvTimeoutError::Disconnected) => break None,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let idle = match sent_at {
                Some(at) => progress.since_last().min(at.elapsed()),
                None => progress.since_last(),
            };
            let got_bytes = progress.bytes.load(Ordering::Relaxed) > 0;
            if sent_at.is_some() && !has_end(path) && got_bytes && idle >= self.quiet {
                // Quiet long enough: the replay is over.
                break self.wind_down(&progress);
            }
            if idle >= self.stall || sent_at.is_some_and(|at| at.elapsed() >= self.stall) {
                stalled = true;
                break self.wind_down(&progress);
            }
        };
        if quit {
            say("bye");
            std::process::exit(0);
        }
        let outcome = match (&done, stalled) {
            (_, true) => ReceiverOutcome::Stalled,
            (Some(Ok(_)), false) => ReceiverOutcome::Completed,
            (Some(Err(e)), false) => ReceiverOutcome::Error(e.clone()),
            (None, false) => ReceiverOutcome::Error("receiver ended without a result".into()),
        };
        let received = done.and_then(Result::ok);
        let mut v = judge(path, &expected, received.as_ref(), &outcome);
        // Attribution, never a pass: a datagram the socket had no room for
        // is lost without a trace.
        if !v.pass
            && let Some((requested, granted)) = short_buffer
        {
            v.failures
                .push(format!("receive buffer {granted} < requested {requested}"));
        }
        // A thread that did not report is left behind, never waited for.
        if !stalled && received.is_some() {
            let _ = worker.join();
        }
        self.verdict(&v);
    }

    /// Cancels the receiver and takes its result, giving it [`WIND_DOWN`].
    fn wind_down(&mut self, progress: &Progress) -> Option<Result<FileDigest, String>> {
        progress.cancel_all();
        let until = Instant::now() + WIND_DOWN;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(Event::Done(id, r)) if id == self.replays => return Some(r),
                Ok(Event::Cmd(c)) => self.pending.push_back(c),
                Ok(Event::Done(..)) => {}
                Err(_) => break,
            }
        }
        None
    }
}

/// The child's whole life: reads commands until `quit`. Returns the process
/// exit code. Verdicts are appended to `out`, so a child the replay
/// restarts adds to its path's log; the replay truncates the log for a
/// path's first child, and a `corpus receive` run by hand appends to an
/// existing file.
pub fn run(path: &str, digests: &Path, out: &Path) -> i32 {
    if urls(path).is_none() {
        eprintln!("corpus receive: unknown path");
        return 2;
    }
    // Appended: a child restarted on the same path adds to its log.
    let out = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "corpus receive: cannot create the output file: {}",
                e.kind()
            );
            return 2;
        }
    };
    let (tx, rx) = mpsc::channel();
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let cmd = match line {
                Ok(l) => parse_line(&l),
                Err(_) => break,
            };
            if stdin_tx.send(Event::Cmd(cmd)).is_err() {
                return;
            }
        }
        let _ = stdin_tx.send(Event::Cmd(Cmd::Eof));
    });
    let stall = Duration::from_secs(
        std::env::var(STALL_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(STALL_SECONDS),
    );
    let mut child = Child {
        path,
        digests,
        out,
        die_after: std::env::var(DIE_AFTER_ENV)
            .ok()
            .and_then(|v| die_after(path, &v)),
        verdicts: 0,
        stop_reading: std::env::var(STOP_READING_ENV).is_ok_and(|v| v == path),
        rx,
        tx,
        last_verdict: None,
        replays: 0,
        stall,
        quiet: Duration::from_secs(QUIET_SECONDS).min(stall),
        rcvbuf_override: std::env::var(RCVBUF_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0),
        pending: VecDeque::new(),
    };
    loop {
        let next = match child.pending.pop_front() {
            Some(c) => Ok(Event::Cmd(c)),
            None => child.rx.recv(),
        };
        match next {
            Ok(Event::Cmd(Cmd::File(sha, prefix, au, chunk))) => {
                child.replay(sha, prefix, au, chunk)
            }
            // The sender's close can be seen, and judged, before `sent` is read.
            Ok(Event::Cmd(Cmd::Sent(s))) if child.last_verdict.as_ref() == Some(&s) => {}
            Ok(Event::Cmd(Cmd::Sent(_))) => protocol_error("sent before file"),
            Ok(Event::Cmd(Cmd::Bad(kind))) => protocol_error(kind),
            Ok(Event::Cmd(Cmd::Quit)) => {
                say("bye");
                return 0;
            }
            Ok(Event::Cmd(Cmd::Eof)) | Err(_) => return 0,
            Ok(Event::Done(..)) => {}
        }
    }
}
