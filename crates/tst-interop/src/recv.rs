//! `recv` subcommand: build a live transport from a URL, drive a
//! [`DemuxReceiver`] over it until either the stream ends or a
//! wall-clock deadline passes, and check the result against a
//! [`Profile`]'s invariants — the live-capture counterpart to
//! `verify::verify_file`'s offline-file check.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{BrokenCause, RecvTransport, TransportError};
use tst_pipeline::{
    DemuxReceiver, ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
    ReconnectPolicy, ShellError, ShellErrorKind,
};

use crate::cli::write_json;
use crate::profiles::Profile;
use crate::report_types::VerifyReport;
use crate::transport::{self, Teeing};
use crate::verify::{self, Tally};

/// How long to wait for the FIRST demuxed event before giving up
/// entirely — the sender never connected or never sent anything at all
/// (a genuinely broken run, not a slow-starting one). Generous on
/// purpose: connection setup time varies across schemes (e.g. a
/// retry-on-connect SRT caller, see `tests/loopback.rs`, can take a
/// few seconds even in the well-behaved case).
const NO_DATA_TIMEOUT: Duration = Duration::from_secs(15);

/// Once data has started arriving, how much longer than the profile's
/// `seconds` window to keep listening before closing — covers sender
/// startup jitter and in-flight packets near the end of the window.
const POST_START_GRACE: Duration = Duration::from_secs(2);

/// Factory closure type `run_managed` hands to `ManagedRecvTransport`
/// — named so the type isn't repeated inline (clippy's
/// `type_complexity` lint).
type ManagedRecvFactory =
    Box<dyn FnMut() -> Result<Teeing<Box<dyn RecvTransport>>, TransportError> + Send>;

/// Build a transport from `url`, receive `seconds` of `expect`'s
/// traffic from it, and check the result against `expect`'s
/// invariants. Writes the same [`VerifyReport`] as JSON to `json_out`
/// (or stdout for `"-"`) when given.
///
/// `no_klv_digest` skips the per-record digest accumulation
/// `CellMetrics::klv_set_sha256` needs — that field comes back `None`
/// instead. See its own doc comment for why a multi-day soak run needs
/// this.
pub fn run(
    url: &str,
    expect: &Profile,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
) -> Result<VerifyReport, String> {
    let transport = transport::make_recv(url)?;
    let report = recv_over_transport(transport, expect, seconds, no_klv_digest)?;
    if let Some(target) = json_out {
        write_json(target, &report)?;
    }
    Ok(report)
}

/// Core of [`run`], split out so a caller that already holds a
/// constructed transport (e.g. this crate's `tests/loopback.rs`, which
/// binds/listens on its own thread before spawning a sender so the two
/// sides can't race without a fixed sleep) can drive the same receive
/// loop without going through `--url` parsing twice.
///
/// Drives `rx.recv_event()` in a loop, folding every [`tst_core::
/// mpegts::demux::DemuxEvent`] into a [`Tally`], until one of:
/// - the transport reports a clean/broken end (`Ok(None)`, or an error
///   whose kind is `Closed`/`EndOfStream`/`TransportBroken` — this
///   crate runs bounded test cells, so any of these three just means
///   "the capture is over," not a process-level failure; genuine
///   problems fold into the resulting `VerifyReport.pass` instead, the
///   same way `verify::verify_file` never distinguishes "no data" from
///   an IO error),
/// - or a wall-clock deadline passes, in which case `rx.close()` is
///   called (same-thread close-then-recv — every transport this crate
///   builds maps that to a `Closed`/`EndOfStream` on the very next
///   call, see `transport.rs`'s per-scheme docs) and the loop drains to
///   `Ok(None)`.
///
/// The deadline starts at `NO_DATA_TIMEOUT` (waiting for the stream
/// to start) and is re-anchored to `seconds + POST_START_GRACE`
/// once the first event arrives, so a slow connection setup doesn't eat
/// into the profile's own capture window.
pub fn recv_over_transport(
    transport: Box<dyn RecvTransport>,
    expect: &Profile,
    seconds: f64,
    no_klv_digest: bool,
) -> Result<VerifyReport, String> {
    let (teeing, tap) = Teeing::new(transport);
    let mut rx = DemuxReceiver::new(teeing);

    let mut deadline = Instant::now() + NO_DATA_TIMEOUT;
    let mut streaming = false;
    let mut closed = false;
    let mut tally = Tally::new();
    if no_klv_digest {
        tally.disable_klv_digest_tracking();
    }
    let start = Instant::now();
    let mut events_seen: u64 = 0;
    let mut last_heartbeat = Instant::now();

    loop {
        if !closed && Instant::now() >= deadline {
            rx.close();
            closed = true;
        }
        // Progress heartbeat → stderr → the soak's per-process log
        // file. See `crate::HEARTBEAT_INTERVAL`'s doc comment. Runs
        // even while idle (the transport's bounded recv returns
        // Backpressure every ~200ms), so a silent stream still beats —
        // "receiving nothing" and "process wedged" look different in
        // the log.
        if last_heartbeat.elapsed() >= crate::HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            eprintln!(
                "recv: heartbeat elapsed_s={} events={events_seen} wire_bytes={}",
                start.elapsed().as_secs(),
                transport::tee_bytes_so_far(&tap),
            );
        }
        match rx.recv_event() {
            Ok(Some(ev)) => {
                if !streaming {
                    streaming = true;
                    deadline = Instant::now() + Duration::from_secs_f64(seconds) + POST_START_GRACE;
                }
                events_seen += 1;
                tally.feed(&ev);
            }
            Ok(None) => break,
            Err(e) => match e.kind() {
                ShellErrorKind::Backpressure => continue,
                ShellErrorKind::Closed
                | ShellErrorKind::EndOfStream
                | ShellErrorKind::TransportBroken => break,
                other => return Err(format!("recv_event: {e} (kind {other:?})")),
            },
        }
    }
    // Explicit drop before reading the tee tally back — `tee_tally`
    // requires the `Teeing` (owned by `rx`'s inner transport state) to
    // have no other owner.
    drop(rx);

    let mut report = tally.finish(expect, seconds, verify::NOMINAL_COUNT_SLACK);
    // `Tally`'s own bytes/stream_sha256 fields were never fed (we never
    // called `note_bytes` on it) — the `Teeing` tap captured the exact
    // bytes at the transport boundary instead, which is the
    // byte-transparent ground truth this crate wants (independent of
    // the demuxer's internal packet-alignment chunking). Overwrite the
    // two fields `finish` computed from the unfed (empty) hasher with
    // the real tally.
    let (bytes, stream_sha256) = transport::tee_tally(tap);
    report.metrics.bytes = bytes;
    report.metrics.stream_sha256 = stream_sha256;
    Ok(report)
}

/// Like [`run`], but drives the capture through a
/// [`ManagedDemuxReceiver`] wrapping a [`ManagedRecvTransport`] instead
/// of a plain [`DemuxReceiver`] — the underlying transport rebuilds
/// (re-binds + re-accepts, for a listener-mode SRT URL; reconnects, for
/// a caller-mode URL) whenever it breaks, instead of ending the
/// capture there.
///
/// `soak.sh`'s SRT leg needs this: `transport::srt_socket`'s listener-
/// mode path binds, accepts exactly ONE connection, and drops the
/// `Listener` (see that function's own doc comment) — once that single
/// accepted socket dies, which every scheduled outage window
/// guarantees via libsrt's own peer-idle timeout, a plain (unmanaged)
/// recv has no way to accept a second connection and the capture ends
/// there permanently, while the managed SEND side keeps retrying
/// forever against a recv that will never accept again — the
/// combination wedges a multi-day soak permanently at its first outage
/// window (`soak.sh`'s own `wait` on both sides never returns).
/// Calling `transport::make_recv(url)` again inside the factory is
/// sufficient to recover: for a listener-mode SRT URL it binds a fresh
/// `Listener` on the same now-freed port and accepts a new connection
/// from whichever peer re-dials it (the managed sender, once the
/// outage clears); for a caller-mode URL it just reconnects.
///
/// Uses `max_attempts: None` (retry forever) for the identical reason
/// `send::run_managed` does — see that function's doc comment. A
/// bounded budget here risks the SAME failure mode this function
/// exists to fix, just moved from "zero attempts" to "some attempts
/// that might not be enough."
///
/// **A `--managed` recv treats ANY transport break as reconnectable —
/// including the SEND side's own ordinary, successful end-of-capture
/// close.** `ManagedRecvTransport::recv_bytes` cannot tell "the peer's
/// connection just broke, still worth reconnecting" apart from "the
/// peer finished normally and is never coming back" (the underlying
/// transport reports the same `Closed`/`Broken` either way) — with
/// `max_attempts: None` it will therefore try to reconnect FOREVER
/// after a perfectly normal capture too, not just after a scheduled
/// outage. Confirmed by hand while developing this function: without
/// the watcher thread below, a managed recv against a sender that had
/// already finished and exited cleanly never returned at all. The
/// wall-clock deadline (same `seconds + POST_START_GRACE` budget the
/// plain, unmanaged path already uses) is what actually ends the
/// capture — not a peer-initiated signal.
///
/// **Why a background thread, not the same single-threaded deadline
/// poll `recv_over_transport` uses.** That poll only gets a chance to
/// run BETWEEN calls to `rx.recv_event()` — fine for a plain transport,
/// whose `recv_bytes` returns every ~200ms regardless of outcome
/// (`transport.rs`'s per-scheme recv timeouts). `ManagedRecvTransport::
/// recv_bytes`'s reconnect loop, by contrast, does NOT return to its
/// caller between failed attempts — with an unbounded policy, a single
/// `recv_event()` call can block indefinitely, and a same-thread
/// deadline check sitting outside that call never gets to run. The
/// cross-thread `cancel_handle` both `ManagedRecvTransport` and
/// `ManagedDemuxReceiver` expose exists exactly for this: the reconnect
/// loop checks its cancelled flag at the top of every retry iteration,
/// so a `.cancel()` call from another thread eventually unblocks it
/// even mid-loop (bounded by one backoff sleep, capped at 10s by
/// `ReconnectPolicy::default`, plus one in-flight factory call, e.g. up
/// to `transport::SRT_ACCEPT_TIMEOUT` — NOT instant, but bounded,
/// unlike waiting for the loop to return control on its own).
///
/// `VerifyReport::reconnects` comes back `Some(n)`,
/// `ManagedDemuxReceiver::reconnects_count()`'s value at the end of the
/// capture — see that field's own doc comment (`report_types.rs`) for
/// exactly what it does and doesn't count.
pub fn run_managed(
    url: &str,
    expect: &Profile,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
) -> Result<VerifyReport, String> {
    let initial_raw = transport::make_recv(url)?;
    let (initial_teed, tap) = Teeing::new(initial_raw);

    // The factory rebuilds a fresh raw transport on every reconnect but
    // must tee into the SAME shared tap `tap` above — a factory that
    // called `Teeing::new` instead would silently start a fresh, empty
    // byte tally on every reconnect, discarding everything counted
    // before the most recent rebuild. See `Teeing::with_tap`'s own doc
    // comment.
    let dial_url = url.to_string();
    let tap_for_factory = Arc::clone(&tap);
    let factory: ManagedRecvFactory = Box::new(move || {
        let raw = transport::make_recv(&dial_url).map_err(|e| TransportError::Broken {
            msg: e,
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })?;
        Ok(Teeing::with_tap(raw, Arc::clone(&tap_for_factory)))
    });

    let policy = ReconnectPolicy {
        max_attempts: None,
        ..ReconnectPolicy::default()
    };
    let managed = ManagedRecvTransport::new(initial_teed, factory, policy);
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    // Shared deadline: the main thread (below) moves it once streaming
    // starts; the watcher thread polls it and cancels once it passes.
    // `Arc<Mutex<Instant>>` rather than a plain local — see this
    // function's own doc comment for why a same-thread check alone
    // can't bound a stuck reconnect loop.
    let deadline: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now() + NO_DATA_TIMEOUT));
    if let Some(cancel) = rx.cancel_handle() {
        let watcher_deadline = Arc::clone(&deadline);
        thread::spawn(move || {
            loop {
                let d = *watcher_deadline.lock().expect("deadline mutex poisoned");
                if Instant::now() >= d {
                    cancel.cancel();
                    return;
                }
                // 100ms poll granularity — matches this crate's other
                // short polling intervals (e.g. `transport.rs`'s
                // `UDP_RECV_POLL`); fine-grained enough that the extra
                // shutdown latency it adds is negligible next to the
                // backoff-sleep/accept-timeout bound described above,
                // coarse enough not to spin.
                thread::sleep(Duration::from_millis(100));
            }
        });
    }
    // A `None` cancel_handle would mean this managed transport can
    // never be cancelled at all — `ManagedRecvTransport::cancel_handle`
    // always returns `Some`, so this branch is unreachable in practice;
    // not treated as a hard error since a future transport that
    // legitimately has none shouldn't crash this function, just lose
    // the safety net (the loop below would then rely solely on a
    // reconnect eventually succeeding or the policy's own budget, and
    // `max_attempts: None` never exhausts — a real regression, but one
    // that would surface as an actual test hang, not a silent bug).

    let mut streaming = false;
    let mut tally = Tally::new();
    if no_klv_digest {
        tally.disable_klv_digest_tracking();
    }
    let start = Instant::now();
    let mut events_seen: u64 = 0;
    let mut last_heartbeat = Instant::now();

    loop {
        // Same heartbeat as `recv_over_transport`, plus the managed
        // wrapper's reconnect counter — a beat whose `reconnects` is
        // climbing while `events` stalls is the log signature of a
        // reconnect storm (vs. a quiet-but-healthy link). Checked
        // between `recv_event` calls only, so during one long blocking
        // reconnect attempt the beat pauses too — a HEARTBEAT GAP in
        // the log is itself diagnostic (the loop is stuck inside the
        // managed transport, not spinning).
        if last_heartbeat.elapsed() >= crate::HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            eprintln!(
                "recv: heartbeat elapsed_s={} events={events_seen} wire_bytes={} reconnects={}",
                start.elapsed().as_secs(),
                transport::tee_bytes_so_far(&tap),
                rx.reconnects_count(),
            );
        }
        match rx.recv_event() {
            Ok(Some(ev)) => {
                if !streaming {
                    streaming = true;
                    let mut d = deadline.lock().expect("deadline mutex poisoned");
                    *d = Instant::now() + Duration::from_secs_f64(seconds) + POST_START_GRACE;
                }
                events_seen += 1;
                tally.feed(&ev);
            }
            Ok(None) => break,
            Err(e) => match e.kind() {
                ShellErrorKind::Backpressure => continue,
                ShellErrorKind::Closed
                | ShellErrorKind::EndOfStream
                | ShellErrorKind::TransportBroken => break,
                other => return Err(format!("recv_event: {e} (kind {other:?})")),
            },
        }
    }

    let reconnects = rx.reconnects_count();
    // Explicit drop before reading the tee tally back — `tee_tally`
    // requires sole ownership of `tap`, which `rx` (via the managed
    // transport's inner Teeing AND the factory closure's own clone,
    // both dropped along with `rx`) is the last other holder of. The
    // watcher thread holds no clone of `tap` at all (only the cancel
    // handle), so it's never in the way here.
    drop(rx);

    let mut report = tally.finish(expect, seconds, verify::NOMINAL_COUNT_SLACK);
    let (bytes, stream_sha256) = transport::tee_tally(tap);
    report.metrics.bytes = bytes;
    report.metrics.stream_sha256 = stream_sha256;
    report.reconnects = Some(reconnects);

    if let Some(target) = json_out {
        write_json(target, &report)?;
    }
    Ok(report)
}
