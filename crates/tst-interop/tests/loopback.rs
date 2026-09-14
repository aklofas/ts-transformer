//! Network-loopback round-trip tests for `send`/`recv`'s URL transport
//! dispatch: udp cell (byte-transparent loopback) + srt cell (reliable
//! caller/listener loopback).
//!
//! This binary's name (`loopback`) puts it in the `network` nextest
//! test-group (`.config/nextest.toml`), which caps each test at a hard
//! 20s kill (`slow-timeout = { period = "10s", terminate-after = 2 }`).
//! Every timing constant below is sized to keep the happy path (which
//! is what actually runs almost every time — connection setup on
//! loopback is sub-millisecond) at a few seconds, with generous but
//! still-bounded margin for a loaded CI box.
//!
//! Deterministic-test policy (no fixed sleeps as synchronization):
//! - Ports are ephemeral, discovered via a throwaway UDP bind (see
//!   `free_port`) — never hardcoded.
//! - The udp cell binds its recv transport (`transport::make_recv`) on
//!   THIS thread before spawning either peer — UDP has no handshake, so
//!   datagrams sent before the socket is bound are silently dropped;
//!   binding first, synchronously, makes the ordering race-free without
//!   a sleep.
//! - The srt cell can't do the same trick (`make_recv`'s listener path
//!   blocks on `accept()`, which must run on its own thread), so the
//!   sender instead retries its connect attempt on a short bounded
//!   budget (`send_with_retry`) — deterministic and fast in the common
//!   case, and safe from partial sends either way (a failed SRT connect
//!   attempt sends no data at all; see `send_with_retry`'s doc comment).
//! - All thread joins are timeout-bounded (`join_with_timeout`), never
//!   a bare `.join()`.

use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, Instant};

use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_interop::verify::KlvExpect;
use tst_interop::{profiles, recv, send, transport};

/// Shared by both cells — long enough to clear the 70%-of-nominal count
/// floors (`verify::NOMINAL_COUNT_SLACK`) with margin, short enough to
/// keep each test comfortably under the network test-group's 20s kill.
const SECONDS: f64 = 3.0;

/// Ask the OS for an unused port via a throwaway UDP bind. Used for both
/// cells — SRT runs over UDP too, so probing the UDP namespace matches
/// where an SRT socket will actually be allocated from. Small TOCTOU
/// race between this probe's drop and the real bind that follows; the
/// same accepted trade-off `examples/sending/encrypted_send_recv.rs`
/// documents for its own analogous port pick.
fn free_port() -> u16 {
    let probe = UdpSocket::bind("127.0.0.1:0").expect("bind probe port");
    probe.local_addr().expect("read probe local_addr").port()
}

/// Poll `handle.is_finished()` until it's done or `timeout` elapses.
/// The bounded-wait counterpart to a bare `.join()`, which this test
/// suite never uses (a hung receive loop must fail the test fast, not
/// hang the whole nextest run).
fn join_with_timeout<T>(handle: thread::JoinHandle<T>, timeout: Duration) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            return handle.join().expect("spawned thread panicked");
        }
        assert!(
            Instant::now() < deadline,
            "thread did not finish within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn udp_baseline_loopback_round_trips_and_matches() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());

    // Bind the receiver here, on the test's own thread, before any peer
    // sends — see the module doc's synchronization note.
    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                false,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    let send_metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        false,
        AuSizeMode::Compact,
        KlvSet::Compact,
        0,
        None,
    )
    .expect("udp send must succeed");

    let recv_report = join_with_timeout(recv_handle, Duration::from_secs(10))
        .expect("recv_over_transport must succeed");

    assert!(
        recv_report.pass,
        "recv failures: {:?}",
        recv_report.failures
    );
    // (regression pin) the default (no `--no-klv-digest`) path must
    // still produce a real hash on both sides, not silently regress to
    // `None` — see `no_klv_digest_true_yields_null_hash_with_counts_unchanged`
    // below for the opposite case.
    assert!(
        send_metrics.klv_set_sha256.is_some(),
        "default send-side klv_set_sha256 must be Some(..)"
    );
    assert!(
        recv_report.metrics.klv_set_sha256.is_some(),
        "default recv-side klv_set_sha256 must be Some(..)"
    );
    assert_eq!(
        send_metrics.klv_set_sha256, recv_report.metrics.klv_set_sha256,
        "sent and received KLV record sets must match"
    );
    assert_eq!(
        send_metrics.stream_sha256, recv_report.metrics.stream_sha256,
        "UDP loopback must be byte-transparent"
    );
    assert_eq!(
        send_metrics.bytes, recv_report.metrics.bytes,
        "sent and received byte counts must match"
    );
}

/// (Fix-round regression) `--no-klv-digest` (`no_klv_digest: true` at
/// the library level) must skip the digest accumulation entirely on
/// BOTH sides — `klv_set_sha256` comes back `None`, not just omitted
/// from the JSON — while every count (`video_aus`, `klv_records`) and
/// the byte-transparent `stream_sha256`/`bytes` fields stay exactly as
/// correct as the default path above. UDP loopback chosen for this
/// (rather than a live SRT/RIST cell) because it's the cheapest real
/// end-to-end exercise of the actual `send_over_transport`/
/// `recv_over_transport` code paths this flag touches — no handshake,
/// sub-millisecond setup, same cost class as the default-path test
/// above.
#[test]
fn no_klv_digest_true_yields_null_hash_with_counts_unchanged() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());

    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                true,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    let send_metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        true,
        AuSizeMode::Compact,
        KlvSet::Compact,
        0,
        None,
    )
    .expect("udp send must succeed");

    let recv_report = join_with_timeout(recv_handle, Duration::from_secs(10))
        .expect("recv_over_transport must succeed");

    assert!(
        recv_report.pass,
        "recv failures: {:?}",
        recv_report.failures
    );
    assert!(
        send_metrics.klv_set_sha256.is_none(),
        "--no-klv-digest must make the send-side hash None, not Some"
    );
    assert!(
        recv_report.metrics.klv_set_sha256.is_none(),
        "--no-klv-digest must make the recv-side hash None, not Some"
    );
    assert!(
        send_metrics.klv_records > 0,
        "the flag must not affect the klv_records COUNT, only the hash"
    );
    assert_eq!(
        send_metrics.klv_records, recv_report.metrics.klv_records,
        "sent and received KLV record counts must still match"
    );
    assert_eq!(
        send_metrics.video_aus, recv_report.metrics.video_aus,
        "video AU counts must be entirely unaffected by this KLV-only flag"
    );
    assert_eq!(
        send_metrics.stream_sha256, recv_report.metrics.stream_sha256,
        "byte-transparency must be unaffected by this flag"
    );
    assert_eq!(
        send_metrics.bytes, recv_report.metrics.bytes,
        "sent and received byte counts must still match"
    );
}

/// The LIVE counterpart to `tests/roundtrip.rs`'s
/// `baseline_rich_klv_roundtrips`: `--klv-set rich` pushed through a real
/// transport and judged by a receiver told to expect the same set and
/// seed. Offline `gen`/`verify` share one process and one record factory;
/// this pair does not, so it is what proves the rich records survive PES
/// packetization and a datagram boundary rather than just a `Vec<u8>`
/// handoff.
///
/// udp for the same reason the two tests above use it — cheapest real
/// end-to-end exercise of `send_over_transport`/`recv_over_transport`,
/// and byte-transparent, so `klv_set_sha256` equality means the records
/// themselves round-tripped, not merely that both sides counted 30 of
/// something.
#[test]
fn udp_rich_klv_loopback_round_trips() {
    const SEED: u64 = 5;

    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());

    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                false,
                false,
                KlvExpect {
                    set: KlvSet::Rich,
                    seed: SEED,
                },
                None,
            )
        })
    };

    let send_metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        false,
        AuSizeMode::Compact,
        KlvSet::Rich,
        SEED,
        None,
    )
    .expect("udp send must succeed");

    let recv_report = join_with_timeout(recv_handle, Duration::from_secs(10))
        .expect("recv_over_transport must succeed");

    assert!(
        recv_report.pass,
        "rich recv failures: {:?}",
        recv_report.failures
    );
    assert!(
        send_metrics.klv_records > 0,
        "the rich capture must actually carry KLV records"
    );
    assert_eq!(
        send_metrics.klv_set_sha256, recv_report.metrics.klv_set_sha256,
        "sent and received rich KLV record sets must match"
    );
    assert_eq!(
        send_metrics.stream_sha256, recv_report.metrics.stream_sha256,
        "UDP loopback must be byte-transparent for rich records too"
    );
    // The LIVE receive path must run the rich oracles, not just carry
    // the expectation — `tests/klv_rich.rs` proves the oracles bite, this
    // proves `recv_over_transport` actually reaches them.
    let m = recv_report
        .metrics
        .klv_rich
        .expect("a rich recv must report rich metrics");
    assert!(m.records > 0, "{m:?}");
    assert_eq!(m.decode_errors, 0, "{m:?}");
    assert_eq!(m.census_mismatches, 0, "{m:?}");
    assert_eq!(m.security_ok, m.security_expected, "{m:?}");
}

/// A scratch path for one test's corruption log. Process id plus the
/// clock, because `cargo test` runs a binary's tests in ONE process and
/// two of them here write logs.
fn corruption_log_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "tst-interop-corrupt-{tag}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ))
}

/// `send` with a corruption tap writes a log with a header and at least
/// one injection, reports the tap's stats in its metrics, and the
/// receiver — WITHOUT the log — fails on the unexplained damage. (The
/// positive "recv WITH the log passes" case is
/// `udp_recv_with_the_corruption_log_passes_and_explains_the_damage`
/// below; this one pins that the damage is real and that the evidence to
/// judge it lands on disk.)
///
/// udp so it stays cheap and byte-transparent: every corrupted byte the
/// sender emits is a corrupted byte the receiver sees, with no transport
/// retransmission in between to muddy what the log should explain.
///
/// `classes=truncate` rather than anything seed-dependent. Truncation is
/// the one class whose damage is guaranteed regardless of which packet
/// it lands on — a short packet shifts every packet boundary after it,
/// so the raw reader must lose sync — whereas a `header` injection's
/// sub-kind is drawn from the PRNG and only some of those sub-kinds are
/// visible to a `Lossy` receiver at all (a continuity-counter flip on a
/// PAT/PMT PID produces no demux event whatsoever, measured while
/// writing this test). Pinning a seed that happens to draw a visible
/// sub-kind would make this test hostage to the tap's PRNG draw order.
#[test]
fn udp_send_with_corruption_writes_a_log_and_recv_without_it_fails() {
    use tst_interop::corrupt::{Class, parse_corrupt, read_log};

    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());
    let log_path = corruption_log_path("nolog");

    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                false,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    // `rate=10000` corrupts every eligible packet, so `min_gap` (whose
    // 1000-packet floor the config validator enforces) alone sets the
    // count — exactly one injection over a ~180-packet 3s capture,
    // independent of the seed.
    let cfg = parse_corrupt("rate=10000,min_gap=1000,classes=truncate", 5)
        .expect("the corruption spec must parse");
    let metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        false,
        AuSizeMode::Compact,
        KlvSet::Compact,
        0,
        Some((cfg, log_path.clone())),
    )
    .expect("udp send must succeed");

    let report = join_with_timeout(recv_handle, Duration::from_secs(10))
        .expect("recv_over_transport must return");

    let (hdr, inj) = read_log(&log_path).expect("the tap's log must parse");
    let _ = std::fs::remove_file(&log_path);

    assert_eq!(hdr.classes, vec![Class::Truncate]);
    assert_eq!(inj.len(), 1, "one injection per min_gap over this capture");
    let stats = metrics
        .corruption
        .expect("send metrics carry the tap stats");
    assert_eq!(
        stats.injections as usize,
        inj.len(),
        "the counter and the log must agree on how many injections happened"
    );
    assert_eq!(
        stats.packets_seen,
        stats.bytes_in / 188,
        "the tap must have seen the muxer's whole stream, packet-aligned"
    );
    assert!(
        stats.bytes_out < stats.bytes_in,
        "truncation must have removed bytes from the wire: {stats:?}"
    );
    assert_eq!(
        metrics.bytes, stats.bytes_out,
        "the tee sits BELOW the tap, so the reported byte count is the \
         corrupted wire's, not the muxer's"
    );
    assert!(
        !report.pass,
        "unexplained truncation must fail the receiver: {:?}",
        report.failures
    );
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.starts_with("rawts_sync_loss")),
        "and it must fail BECAUSE packet sync was lost, not incidentally: {:?}",
        report.failures
    );
    assert!(
        report.metrics.corruption_attribution.is_none(),
        "a receiver given no log judges no corruption"
    );
}

/// The positive twin: the same corrupted stream, judged WITH the log,
/// passes — and the receiver accounts for every injection the sender
/// made.
///
/// The receiver starts FIRST, before the log file exists at all, which
/// is both the arrangement `soak.sh` uses and the one that makes the
/// judgement sound (a receiver that joins late cannot place a coordinate
/// anchored before the stream's first PCR — see `recv::run`'s doc
/// comment). Proving that the receiver waits for a file its peer has not
/// created yet is half the point of this test.
#[test]
fn udp_recv_with_the_corruption_log_passes_and_explains_the_damage() {
    use tst_interop::corrupt::parse_corrupt;

    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());
    let log_path = corruption_log_path("withlog");
    assert!(!log_path.exists(), "the sender has not run yet");

    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        let log_path = log_path.clone();
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                false,
                false,
                KlvExpect::compact(),
                Some(&log_path),
            )
        })
    };

    let cfg = parse_corrupt("rate=10000,min_gap=1000,classes=truncate", 9)
        .expect("the corruption spec must parse");
    let metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        false,
        AuSizeMode::Compact,
        KlvSet::Compact,
        0,
        Some((cfg, log_path.clone())),
    )
    .expect("udp send must succeed");

    let report = join_with_timeout(recv_handle, Duration::from_secs(20))
        .expect("recv_over_transport must return");
    let _ = std::fs::remove_file(&log_path);

    let stats = metrics
        .corruption
        .expect("send metrics carry the tap stats");
    assert!(stats.injections > 0, "the tap must have injected something");
    let a = report
        .metrics
        .corruption_attribution
        .expect("a judged capture carries its attribution");
    assert_eq!(
        a.injected, stats.injections,
        "the receiver must account for every injection the sender logged"
    );
    assert_eq!(a.resolved, a.injected, "and place every one of them: {a:?}");
    assert!(
        a.resyncs > 0 && a.attributed_events == a.events,
        "the damage must produce events, all of them explained: {a:?}"
    );
    assert!(
        report.pass,
        "explained corruption must not fail the capture: {:?}",
        report.failures
    );
}

/// The receiver must keep reading the log for the WHOLE capture, not
/// snapshot it at startup: the sender appends to that same file as it
/// runs, and injections it records after the receiver started are the
/// normal case in any run longer than a few seconds.
///
/// Driven deterministically rather than by racing a real tap. The stream
/// is clean, the log starts as a bare header, and a line is appended
/// mid-capture; if the receiver never re-read the file its attribution
/// would report `injected: 0`. The appended injection carries no PCR
/// anchor, so it also pins the other half of the rule — an anchorless
/// coordinate arriving after the receiver has passed its own first PCR
/// is stranded (counted `unresolved`) instead of being blamed on a
/// receiver that could not have placed it.
#[test]
fn udp_recv_picks_up_injections_appended_after_it_started() {
    use std::io::Write as _;

    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let url = format!("udp://127.0.0.1:{}", free_port());
    let log_path = corruption_log_path("growing");

    // A real tap's header line, so the receiver validates exactly what a
    // live sender would have written.
    std::fs::write(
        &log_path,
        b"{\"header\":{\"tap_version\":1,\"seed\":3,\"rate_per_10k\":5,\"min_gap\":1000,\
          \"classes\":[\"drop\"],\"attribution_window\":500,\"recovery_bound\":600}}\n",
    )
    .expect("write the header");

    let recv_transport = transport::make_recv(&url).expect("bind udp recv");
    let recv_handle = {
        let seconds = SECONDS;
        let log_path = log_path.clone();
        thread::spawn(move || {
            recv::recv_over_transport(
                recv_transport,
                profile,
                seconds,
                false,
                false,
                KlvExpect::compact(),
                Some(&log_path),
            )
        })
    };

    // Appended a third of the way in — after the receiver has opened the
    // log and seen PCRs of its own, and well before it stops reading.
    let appender = {
        let log_path = log_path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs_f64(SECONDS / 3.0));
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .expect("append to the log");
            f.write_all(
                b"{\"injection\":{\"ordinal\":7,\"coord\":{\"pcr_base\":null,\"since_pcr\":7},\
                  \"class\":\"drop\",\"pid\":4113,\"offsets\":[],\"before\":[],\"after\":[],\
                  \"detectable\":false,\"psi\":false,\"pes_start\":false}}\n",
            )
            .expect("write the injection line");
            f.flush().expect("flush the injection line");
        })
    };

    let send_metrics = send::run(
        profile,
        &url,
        SECONDS,
        None,
        false,
        AuSizeMode::Compact,
        KlvSet::Compact,
        0,
        None,
    )
    .expect("udp send must succeed");

    join_with_timeout(appender, Duration::from_secs(10));
    let report = join_with_timeout(recv_handle, Duration::from_secs(20))
        .expect("recv_over_transport must return");
    let _ = std::fs::remove_file(&log_path);

    let a = report
        .metrics
        .corruption_attribution
        .expect("a judged capture carries its attribution");
    assert_eq!(
        a.injected, 1,
        "the receiver must have re-read the log it opened empty: {a:?}"
    );
    assert_eq!(
        a.unresolved, 1,
        "an anchorless coordinate arriving mid-capture cannot be placed: {a:?}"
    );
    assert!(
        a.undetected.is_empty() && a.unrecovered.is_empty(),
        "and an injection that was never placed is never judged: {a:?}"
    );
    // The stream itself was clean, so nothing else should have failed.
    assert!(
        report.pass,
        "recv failures: {:?} (send pushed {} AUs)",
        report.failures, send_metrics.video_aus
    );
}

/// Retry `send::run` on a bounded budget. A failed SRT `connect()`
/// attempt (e.g. because the listener hasn't bound yet — see the
/// module doc's synchronization note) fails before any data is pushed,
/// so retrying the whole call never risks a double/partial send: either
/// the whole `seconds`-long session succeeds once connected, or nothing
/// was sent at all.
fn send_with_retry(
    profile: &profiles::Profile,
    url: &str,
    seconds: f64,
    budget: Duration,
) -> tst_interop::report_types::CellMetrics {
    send_with_retry_sized(profile, url, seconds, budget, AuSizeMode::Compact)
}

/// [`send_with_retry`] with an explicit [`AuSizeMode`].
fn send_with_retry_sized(
    profile: &profiles::Profile,
    url: &str,
    seconds: f64,
    budget: Duration,
    au_sizes: AuSizeMode,
) -> tst_interop::report_types::CellMetrics {
    let deadline = Instant::now() + budget;
    loop {
        match send::run(
            profile,
            url,
            seconds,
            None,
            false,
            au_sizes,
            KlvSet::Compact,
            0,
            None,
        ) {
            Ok(metrics) => return metrics,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "send::run kept failing until the retry budget ({budget:?}) ran out: {e}"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// A listener-mode `srt://` recv against a port nobody ever connects to
/// must fail cleanly within a bound, not hang forever. `Listener::
/// accept()` (libsrt's plain accept call) has no timeout at all — see
/// `transport::srt_socket`'s doc comment on why it uses `accept_timeout`
/// instead — so this proves that fix actually bounds the wait.
///
/// Overrides the URL's `conntimeo`/`connect_timeout` overlay (which
/// `transport::srt_socket` reuses as its accept-timeout bound in
/// listener mode — see `SRT_ACCEPT_TIMEOUT`'s doc comment) down to 2s so
/// this test stays fast, rather than waiting out the 15s production
/// default and eating most of the network test-group's 20s per-test
/// kill.
#[test]
fn srt_listener_accept_times_out_when_nobody_connects() {
    let port = free_port();
    let url = format!("srt://127.0.0.1:{port}?mode=listener&conntimeo=2000");

    let started = Instant::now();
    let result = transport::make_recv(&url);
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "accept against a port nobody connects to must fail, not silently return"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "accept must return within its bounded timeout (~2s), not hang; took {elapsed:?}"
    );
}

#[test]
fn srt_baseline_loopback_round_trips_and_matches() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let port = free_port();
    let recv_url = format!("srt://127.0.0.1:{port}?mode=listener");
    let send_url = format!("srt://127.0.0.1:{port}"); // default mode=caller

    // The listener's bind()+listen() is fast, but recv::run's accept()
    // call blocks until a peer connects, so it must run on its own
    // thread — unlike the udp cell, there's no way to pre-bind and hand
    // off an already-accepted transport without also blocking this
    // thread. The sender's bounded retry (see `send_with_retry`) covers
    // the resulting race instead.
    let recv_handle = {
        let recv_url = recv_url.clone();
        thread::spawn(move || {
            recv::run(
                &recv_url,
                profile,
                SECONDS,
                None,
                false,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    let send_metrics = send_with_retry(profile, &send_url, SECONDS, Duration::from_secs(5));

    let recv_report =
        join_with_timeout(recv_handle, Duration::from_secs(15)).expect("recv::run must succeed");

    assert!(
        recv_report.pass,
        "recv failures: {:?}",
        recv_report.failures
    );
    assert_eq!(
        send_metrics.klv_set_sha256, recv_report.metrics.klv_set_sha256,
        "sent and received KLV record sets must match"
    );
    assert_eq!(
        send_metrics.stream_sha256, recv_report.metrics.stream_sha256,
        "SRT is a reliable in-order transport, so a loopback capture should be byte-transparent too"
    );
}

/// Realistic (GOP-structured, multi-KB) AU sizes must survive the full
/// mux → transport → demux round trip exactly like the compact
/// fixtures do — a keyframe here spans hundreds of TS packets, so this
/// exercises real PES/TS packetization bursts the compact tests never
/// reach. SRT (reliable, in-order) rather than UDP so byte-transparency
/// is guaranteed by the protocol and the burst can't flake the test via
/// loopback rcvbuf overflow.
#[test]
fn srt_realistic_au_sizes_round_trip_and_match() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let port = free_port();
    let recv_url = format!("srt://127.0.0.1:{port}?mode=listener");
    let send_url = format!("srt://127.0.0.1:{port}");

    let recv_handle = {
        let recv_url = recv_url.clone();
        thread::spawn(move || {
            recv::run(
                &recv_url,
                profile,
                SECONDS,
                None,
                false,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    let send_metrics = send_with_retry_sized(
        profile,
        &send_url,
        SECONDS,
        Duration::from_secs(5),
        AuSizeMode::Realistic,
    );

    let recv_report =
        join_with_timeout(recv_handle, Duration::from_secs(15)).expect("recv::run must succeed");

    assert!(
        recv_report.pass,
        "recv failures: {:?}",
        recv_report.failures
    );
    assert_eq!(
        send_metrics.video_aus, recv_report.metrics.video_aus,
        "every realistic-size AU must survive the round trip"
    );
    assert_eq!(
        send_metrics.stream_sha256, recv_report.metrics.stream_sha256,
        "SRT loopback must stay byte-transparent at realistic sizes"
    );
    // ~3s at ~217 KB/s of elementary stream — far beyond what the
    // compact fixtures could ever produce (~25 KB total). Pins that
    // Realistic mode actually changed the traffic regime rather than
    // silently falling back to compact sizes.
    assert!(
        send_metrics.bytes > 300_000,
        "realistic mode must produce hundreds of KB in {SECONDS}s, got {} bytes",
        send_metrics.bytes
    );
}

/// `recv --managed`'s watcher/cancel path must actually bound the
/// reconnect loop's runtime — the bug this fix wave found and fixed was
/// exactly a caller relying on that bound and getting an unbounded hang
/// instead (see `recv.rs`'s `run_managed` doc comment). Manual dry-runs
/// proved that during development; this pins it as a real regression
/// test.
///
/// Deliberately does NOT use a factory that never connects even once:
/// `run_managed`'s FIRST-ever connection is bounded by the hardcoded
/// 15s `NO_DATA_TIMEOUT`, not by anything this test controls, and
/// landing a bounded assertion safely under this test-group's 20s
/// per-test kill against that fixed value plus the exponential backoff
/// schedule's up-to-10s-capped sleeps (`ReconnectPolicy::default`) — a
/// sleep that can't be interrupted mid-attempt, only checked between
/// attempts — would be within a few seconds of the kill itself on a
/// loaded runner (verified by hand-computing the backoff schedule
/// before writing this). Instead: one real, short-lived sender
/// connects ONCE (so `streaming` flips true and the deadline becomes
/// the fully test-controlled `seconds + POST_START_GRACE`, a couple of
/// seconds, not 15) and then closes for good; nobody ever connects
/// again, so every subsequent factory rebuild attempt times out against
/// `conntimeo` (shortened for the same reason
/// `srt_listener_accept_times_out_when_nobody_connects` shortens it).
/// This still exercises the identical watcher-thread/cancel mechanism
/// the fix added, with a much wider safety margin under the per-test
/// kill, and is arguably closer to the real soak scenario (an
/// established connection that breaks and never recovers) than a peer
/// that never shows up at all.
#[test]
fn srt_managed_recv_returns_after_peer_never_reconnects() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let port = free_port();
    // conntimeo=2000: every factory rebuild's own accept() call — the
    // first (for the real sender) and every retry after — is bounded to
    // 2s instead of the 15s production default. 2s (not shorter) leaves
    // enough headroom for the real sender's thread-scheduling +
    // handshake latency on a loaded runner to land inside the FIRST
    // accept call reliably (a too-short value here raced the real
    // sender against the listener's own accept timeout during
    // development and failed with a spurious connect timeout on the
    // sender side, not the reconnect-loop behavior this test exists to
    // check).
    let recv_url = format!("srt://127.0.0.1:{port}?mode=listener&conntimeo=2000");
    let send_url = format!("srt://127.0.0.1:{port}");

    // seconds=0.1: once the real sender's data starts streaming, the
    // deadline becomes seconds + POST_START_GRACE (a fixed 2s) from that
    // moment — a couple of seconds total, not the 15s NO_DATA_TIMEOUT
    // that only governs before the first successful event.
    let recv_handle = {
        let recv_url = recv_url.clone();
        thread::spawn(move || {
            recv::run_managed(
                &recv_url,
                profile,
                0.1,
                None,
                false,
                false,
                KlvExpect::compact(),
                None,
            )
        })
    };

    // One short, real send: connects once, pushes a handful of AUs,
    // closes cleanly. Enough for `streaming` to flip true inside
    // `run_managed` — an empty capture would `break` on `Ok(None)`
    // before ever driving the reconnect loop at all, testing nothing.
    let send_metrics = send_with_retry(profile, &send_url, 0.3, Duration::from_secs(5));
    assert!(
        send_metrics.video_aus > 0,
        "the one-shot sender must have pushed at least one AU"
    );

    let report = join_with_timeout(recv_handle, Duration::from_secs(15))
        .expect("run_managed must return Ok (the watcher's cancel is a graceful break, not a hard error) rather than hang");

    // Not an exact-equality check against `send_metrics.video_aus`: the
    // sender's own close can race SRT's TSBPD delivery of its very last
    // AU (observed directly during development — 8 of 9 sent AUs
    // tallied on one run), which is real transport timing, not a
    // reconnect-loop bug. What this DOES pin: the tally from the one
    // real connection survived into the final report (not silently
    // dropped/reset by the reconnect attempts that follow it), and the
    // reconnect loop never fabricates or double-counts data it was
    // never actually given.
    assert!(
        report.metrics.video_aus > 0 && report.metrics.video_aus <= send_metrics.video_aus,
        "expected 1..={} AUs tallied from the one real connection, got {}",
        send_metrics.video_aus,
        report.metrics.video_aus
    );
}
