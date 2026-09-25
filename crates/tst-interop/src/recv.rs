//! `recv` subcommand: build a live transport from a URL, drive a
//! [`DemuxReceiver`] over it until either the stream ends or a
//! wall-clock deadline passes, and check the result against a
//! [`Profile`]'s invariants — the live-capture counterpart to
//! `verify::verify_file`'s offline-file check.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{BrokenCause, RecvTransport, TransportError};
use tst_pipeline::{
    DemuxReceiver, ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
    ReconnectPolicy, ShellError, ShellErrorKind,
};

use crate::cli::write_json;
use crate::corrupt;
use crate::profiles::{self, Profile};
use crate::rawts::WireSummary;
use crate::report_types::VerifyReport;
use crate::transport::{self, Teeing};
use crate::verify::{self, KlvExpect, Tally, VerifyMode};

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

/// Split `tee_tally`'s wire result into a usable [`WireSummary`]
/// (falling back to an empty default) and an optional trailing-bytes
/// error string, shared by [`recv_over_transport`] and [`run_managed`]
/// so callers can still run the wire-level oracles against whatever
/// packets DID parse cleanly while still surfacing a truncated final
/// packet as an explicit `rawts_trailing_bytes` failure rather than
/// losing it silently.
fn wire_and_trailing_bytes_error(
    wire_result: Result<WireSummary, String>,
) -> (WireSummary, Option<String>) {
    match wire_result {
        Ok(w) => (w, None),
        Err(e) => (WireSummary::default(), Some(e)),
    }
}

/// Open `path`'s corruption log, attach it to `tally` as the judge of
/// this capture, and put the tee's raw reader into sync-recovery mode —
/// shared by both receive loops, and called BEFORE either reads its
/// first byte. Returns the tail for the loop to keep polling.
///
/// **The log may not exist yet.** The SENDER creates it, and a receiver
/// that must be running first (so that it does not miss the start of the
/// stream — see `run`'s doc comment) will therefore reach this before
/// there is anything to open. So it retries until the file exists AND
/// carries a complete header line, bounded by the same
/// [`NO_DATA_TIMEOUT`] budget the receive loop gives the stream itself,
/// and reports the last failure if the budget runs out.
///
/// Resync mode is the load-bearing half of the attach: a capture whose
/// sender deliberately truncated packets WILL fall out of 188-byte
/// alignment, and without it the reader latches that as a
/// `rawts_sync_loss` failure — a verdict that only restates the premise.
/// In resync mode each recovery is recorded instead, and fed to the
/// attribution as evidence that the receiver noticed
/// (`corrupt::Signal::Resync`).
fn attach_corruption_log(
    tally: &mut Tally,
    tap: &Arc<Mutex<transport::TeeState>>,
    path: &Path,
    strict: bool,
) -> Result<corrupt::LogTail, String> {
    let deadline = Instant::now() + NO_DATA_TIMEOUT;
    let mut tail = loop {
        match corrupt::LogTail::open(path) {
            Ok(t) => break t,
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "waited {NO_DATA_TIMEOUT:?} for the sender to write {}: {e}",
                    path.display()
                ));
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    };
    let injections = tail.poll()?;
    // The tier is fixed here rather than at `Tally::finish`: a lossy
    // capture's excusal has to be applied as each event arrives (see
    // `corrupt::Attribution::lossy`). It must agree with the
    // `VerifyMode` this loop finishes in, which `Tally::finish` asserts.
    tally.attach_attribution(if strict {
        corrupt::Attribution::strict(injections, tail.header())
    } else {
        corrupt::Attribution::lossy(injections, tail.header())
    });
    transport::tee_set_resync_mode(tap, true);
    Ok(tail)
}

/// Fold any injections the sender has logged since the last look into
/// `tally`'s attribution.
///
/// **Called before stamping EVERY event, not on a timer.** A periodic
/// poll looks cheaper and is wrong: the tap writes and flushes an
/// injection's log line BEFORE the corrupted bytes it describes leave the
/// sender (`corrupt::Corrupter`'s `transform` logs, then `emit` sends),
/// so a poll taken at the moment an event surfaces is guaranteed to see
/// the line that explains it, while a poll up to a second stale is not.
/// Measured, not theorised: a one-second interval made the positive
/// round-trip test fail roughly one run in five, the injection arriving
/// after the event it explained had already been judged unexplained.
/// The cost is one `read` per demux event — at soak rates a few dozen a
/// second, almost always returning zero bytes.
///
/// A log-read failure is surfaced, not swallowed: the log IS the
/// evidence, and a judgement made against half of it would be a quieter
/// kind of wrong than an error.
fn poll_corruption_log(
    tally: &mut Tally,
    tail: Option<&mut corrupt::LogTail>,
) -> Result<(), String> {
    let Some(tail) = tail else { return Ok(()) };
    tally.append_injections(tail.poll()?);
    Ok(())
}

/// Fold everything the tee's raw reader has learned since the last call
/// into `tally`'s attribution, and return the receive-side packet
/// coordinate to stamp the event that is about to be fed.
///
/// PCR bases first (they are what resolve a logged coordinate to a
/// receiver position at all), then any new sync recoveries. Both are
/// DRAINED, so each event reaches the attribution exactly once and the
/// reader never accumulates a history; `std::mem::take` on an empty
/// `Vec` does not allocate, so polling per event is as cheap as the
/// count poll this replaced.
fn drain_wire_evidence(tally: &mut Tally, tap: &Arc<Mutex<transport::TeeState>>) -> u64 {
    let coord = transport::tee_coord(tap);
    tally.note_pcrs(&transport::tee_drain_pcrs(tap));
    tally.note_resyncs(&transport::tee_take_resyncs(tap));
    coord.packets
}

/// Final sweep of the same evidence once the receive loop has ended,
/// including the one recovery the reader's own list never holds (a
/// trailing partial packet — `rawts::Reader::trailing_resync`). Must run
/// before `transport::tee_tally`, which consumes the reader.
fn drain_final_wire_evidence(tally: &mut Tally, tap: &Arc<Mutex<transport::TeeState>>) {
    drain_wire_evidence(tally, tap);
    if let Some(t) = transport::tee_trailing_resync(tap) {
        tally.note_trailing_resync(&t);
    }
}

/// Build a transport from `url`, receive `seconds` of `expect`'s
/// traffic from it, and check the result against `expect`'s
/// invariants. Writes the same [`VerifyReport`] as JSON to `json_out`
/// (or stdout for `"-"`) when given.
///
/// `no_klv_digest` skips the per-record digest accumulation
/// `CellMetrics::klv_set_sha256` needs — that field comes back `None`
/// instead. See its own doc comment for why a multi-day soak run needs
/// this.
///
/// `strict` selects `verify::VerifyMode::Strict` (a `Discontinuity` event
/// fails the check) over the default `Lossy` (a `Discontinuity` is only
/// counted) — see `VerifyMode`'s own doc comment. Either way a
/// `NonConformant` event always fails.
///
/// `corruption_log` names the JSONL log a `send --corrupt` peer is
/// writing, turning the capture into a judgement OF that corruption —
/// see `attach_corruption_log` (private) and `crate::corrupt`'s module
/// doc. The
/// file need not exist yet; the log is read incrementally for the whole
/// capture, so injections the sender records while this receive loop is
/// already running are judged too.
///
/// **Start the receiver FIRST.** A receiver that joins a stream already
/// in progress may misjudge injections the sender logged before the
/// stream's first PCR: those coordinates carry no PCR anchor and mean
/// "this many packets from the START of the stream", which is a position
/// a late joiner never saw and cannot compute. Injections appended after
/// this receiver has seen its own first PCR are stranded rather than
/// guessed at (`corrupt::Attribution::append`), so they are counted
/// `unresolved` and never judged — but ones already in the log when it
/// opened are taken at face value. `soak.sh` starts `recv` before `send`
/// for exactly this reason.
///
/// `klv` says which KLV record set the sender generated — see
/// [`crate::verify::KlvExpect`]. `KlvExpect::compact()` (the default)
/// leaves the rich decode oracles off.
#[allow(clippy::too_many_arguments)]
pub fn run(
    url: &str,
    expect: &Profile,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
    strict: bool,
    klv: KlvExpect,
    corruption_log: Option<&Path>,
) -> Result<VerifyReport, String> {
    let transport = transport::make_recv(url)?;
    let report = recv_over_transport(
        transport,
        expect,
        seconds,
        no_klv_digest,
        strict,
        klv,
        corruption_log,
    )?;
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
#[allow(clippy::too_many_arguments)]
pub fn recv_over_transport(
    transport: Box<dyn RecvTransport>,
    expect: &Profile,
    seconds: f64,
    no_klv_digest: bool,
    strict: bool,
    klv: KlvExpect,
    corruption_log: Option<&Path>,
) -> Result<VerifyReport, String> {
    let (teeing, tap) = Teeing::new(transport);
    // Built per-profile, not `DemuxReceiver::new` — see
    // `profiles::demuxer_config`'s doc comment for why a default-config
    // demuxer silently mis-tallies `av1-klv-a`.
    let mut rx = DemuxReceiver::with_demux_options(teeing, profiles::demuxer_config(expect));

    let mut deadline = Instant::now() + NO_DATA_TIMEOUT;
    let mut streaming = false;
    let mut closed = false;
    let mut tally = Tally::new();
    tally.set_klv_expect(klv);
    if no_klv_digest {
        tally.disable_klv_digest_tracking();
    }
    let mut tail = match corruption_log {
        Some(path) => Some(attach_corruption_log(&mut tally, &tap, path, strict)?),
        None => None,
    };
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
                "recv: heartbeat elapsed_s={} events={events_seen} wire_bytes={} disc={} nc={}",
                start.elapsed().as_secs(),
                transport::tee_bytes_so_far(&tap),
                tally.discontinuities(),
                tally.nonconformant(),
            );
        }
        match rx.recv_event() {
            Ok(Some(ev)) => {
                if !streaming {
                    streaming = true;
                    deadline = Instant::now() + Duration::from_secs_f64(seconds) + POST_START_GRACE;
                }
                events_seen += 1;
                // New injections BEFORE this event is stamped — and
                // before `drain_wire_evidence` notes a PCR, so one logged
                // with no PCR anchor is still placeable (see
                // `corrupt::Attribution::append`).
                poll_corruption_log(&mut tally, tail.as_mut())?;
                let at = drain_wire_evidence(&mut tally, &tap);
                tally.feed_at(&ev, at);
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
    // Everything the sender logged after the last poll, then whatever the
    // reader learned after the last event it stamped — the tail of a
    // capture is exactly where a final injection and a final recovery live.
    poll_corruption_log(&mut tally, tail.as_mut())?;
    drain_final_wire_evidence(&mut tally, &tap);
    // Explicit drop before reading the tee tally back — `tee_tally`
    // requires the `Teeing` (owned by `rx`'s inner transport state) to
    // have no other owner.
    drop(rx);

    // Read the tap BEFORE `finish` — its `wire` (the tee's own
    // `rawts::Reader`, fed every byte `recv_bytes` returned) is what
    // `finish` checks the wire-level oracles against; a captured stream
    // that fell out of 188-byte packet alignment surfaces here as
    // `reader_error`, added below as an explicit failure rather than
    // silently dropped. A trailing partial packet at the very end of the
    // capture (`wire_result` an `Err`) is handled the same way.
    let (bytes, stream_sha256, wire_result, reader_error) = transport::tee_tally(tap);
    let (wire, trailing_bytes_error) = wire_and_trailing_bytes_error(wire_result);

    let mode = if strict {
        VerifyMode::Strict
    } else {
        VerifyMode::Lossy
    };
    let mut report = tally.finish(expect, seconds, verify::NOMINAL_COUNT_SLACK, mode, &wire);
    // `Tally`'s own bytes/stream_sha256 fields were never fed (we never
    // called `note_bytes` on it) — the `Teeing` tap captured the exact
    // bytes at the transport boundary instead, which is the
    // byte-transparent ground truth this crate wants (independent of
    // the demuxer's internal packet-alignment chunking). Overwrite the
    // two fields `finish` computed from the unfed (empty) hasher with
    // the real tally.
    report.metrics.bytes = bytes;
    report.metrics.stream_sha256 = stream_sha256;
    // Which profile this capture was judged against, for `report soak`'s
    // `profile_declared_<leg>` check — see `VerifyReport::profile`.
    report.profile = Some(expect.name.to_string());
    if let Some(e) = reader_error {
        report.failures.push(format!("rawts_sync_loss: {e}"));
        report.pass = false;
    }
    if let Some(e) = trailing_bytes_error {
        report.failures.push(format!("rawts_trailing_bytes: {e}"));
        report.pass = false;
    }
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
#[allow(clippy::too_many_arguments)]
pub fn run_managed(
    url: &str,
    expect: &Profile,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
    strict: bool,
    klv: KlvExpect,
    corruption_log: Option<&Path>,
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
        // A successfully-dialed replacement transport starts delivering
        // bytes at a fresh packet boundary of its own — any partial
        // packet still sitting in the shared tap's raw-TS reader (from
        // the connection that just broke) can never be validly
        // completed by it, and a sync-loss error latched from that same
        // dead connection shouldn't follow the new one either. See
        // `transport::tee_resync`'s own doc comment.
        transport::tee_resync(&tap_for_factory);
        Ok(Teeing::with_tap(raw, Arc::clone(&tap_for_factory)))
    });

    let policy = ReconnectPolicy {
        max_attempts: None,
        ..ReconnectPolicy::default()
    };
    let managed = ManagedRecvTransport::new(initial_teed, factory, policy);
    // Built per-profile, not `ManagedDemuxReceiver::new` — see
    // `profiles::demuxer_config`'s doc comment.
    let mut rx = ManagedDemuxReceiver::with_demux_options(
        managed,
        profiles::demuxer_config(expect),
        ManagedDemuxReceiverConfig::default(),
    );

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
    tally.set_klv_expect(klv);
    if no_klv_digest {
        tally.disable_klv_digest_tracking();
    }
    // Before the first byte — and it survives every reconnect: the
    // factory's `tee_resync` clears only the in-flight carry, never the
    // reader's recovery mode, its recovery list or its packet count (see
    // `rawts::Reader::resync`), so coordinates stay continuous across an
    // outage. A reconnect itself is deliberately NOT recorded as a
    // recovery: the sender's corruption is not what broke the link.
    let mut tail = match corruption_log {
        Some(path) => Some(attach_corruption_log(&mut tally, &tap, path, strict)?),
        None => None,
    };
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
                "recv: heartbeat elapsed_s={} events={events_seen} wire_bytes={} disc={} nc={} \
                 reconnects={}",
                start.elapsed().as_secs(),
                transport::tee_bytes_so_far(&tap),
                tally.discontinuities(),
                tally.nonconformant(),
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
                // See `recv_over_transport`'s loop for why the tail is
                // polled before the event is stamped.
                poll_corruption_log(&mut tally, tail.as_mut())?;
                let at = drain_wire_evidence(&mut tally, &tap);
                tally.feed_at(&ev, at);
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
    poll_corruption_log(&mut tally, tail.as_mut())?;
    drain_final_wire_evidence(&mut tally, &tap);

    let reconnects = rx.reconnects_count();
    // Explicit drop before reading the tee tally back — `tee_tally`
    // requires sole ownership of `tap`, which `rx` (via the managed
    // transport's inner Teeing AND the factory closure's own clone,
    // both dropped along with `rx`) is the last other holder of. The
    // watcher thread holds no clone of `tap` at all (only the cancel
    // handle), so it's never in the way here.
    drop(rx);

    // Read the tap BEFORE `finish` — see `recv_over_transport`'s doc
    // comment for why.
    let (bytes, stream_sha256, wire_result, reader_error) = transport::tee_tally(tap);
    let (wire, trailing_bytes_error) = wire_and_trailing_bytes_error(wire_result);

    let mode = if strict {
        VerifyMode::Strict
    } else {
        VerifyMode::Lossy
    };
    let mut report = tally.finish(expect, seconds, verify::NOMINAL_COUNT_SLACK, mode, &wire);
    report.metrics.bytes = bytes;
    report.metrics.stream_sha256 = stream_sha256;
    report.reconnects = Some(reconnects);
    // See `recv_over_transport`'s own stamp above.
    report.profile = Some(expect.name.to_string());
    if let Some(e) = reader_error {
        report.failures.push(format!("rawts_sync_loss: {e}"));
        report.pass = false;
    }
    if let Some(e) = trailing_bytes_error {
        report.failures.push(format!("rawts_trailing_bytes: {e}"));
        report.pass = false;
    }

    if let Some(target) = json_out {
        write_json(target, &report)?;
    }
    Ok(report)
}
