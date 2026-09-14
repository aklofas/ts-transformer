//! Integration tests for `proxy::run`: transparent relay (byte
//! transparency at loss=0), scheduled outage windows, phase-scheduled
//! impairment (the echoed phase table and per-phase counters), resilience
//! to an unwritable stats path, and an end-to-end SRT-through-proxy cell
//! (SRT's own retransmission recovering from a lossy/jittered link).
//!
//! # nextest group placement
//!
//! This binary's name (`proxy`) doesn't match any of
//! `.config/nextest.toml`'s `binary(...)` filter entries, so no test
//! here lands in the capped `network` test-group by binary name alone.
//! Following `tests/serve.rs`'s and `tests/loopback.rs`'s own convention
//! (deliberately naming a timing-sensitive network test so its NAME
//! matches one of the `test(...)` filter entries, opting it into the
//! serialized group), only
//! [`srt_round_trip_through_lossy_proxy_recovers_via_retransmission`]
//! does that (via `round_trip`) — it's the one cell in this file where
//! CPU/port contention from unrelated parallel tests could plausibly
//! perturb a real SRT handshake + TSBPD delivery under injected loss.
//! It therefore gets the `network` group's 10s-period/2-terminate (20s
//! hard kill; 40s on Windows via the platform override) instead of the
//! default profile's 30s/2 (60s) global safety net.
//!
//! Every other test here ([`transparent_relay_preserves_order_and_bytes`],
//! [`outage_windows_produce_periodic_gaps`],
//! [`scheduled_proxy_echoes_its_phase_table_and_per_phase_counters`],
//! [`fixed_mode_proxy_records_one_phase_and_no_schedule`],
//! [`unwritable_stats_path_does_not_fail_the_relay`], and the two
//! subprocess argument-validation cells) is left unmatched
//! (default group, full parallelism): they use ephemeral OS-assigned
//! ports (no cross-test port contention) and are written with generous
//! tolerances (or no wall-clock assertion at all) rather than tight
//! timing checks, so ordinary CPU contention from sibling tests shouldn't
//! flake them. (`transparent_relay_preserves_order_and_bytes` DID flake
//! under macOS runner load in 2026-08/09 — its original fixed 3s windows
//! vs. a sleep-paced send loop, see its own comment — and is now
//! sender-completion-driven with an explicitly stopped proxy: THAT is
//! the deterministic shape to copy, rather than promoting a test into
//! the `network` group to hide a wall-clock dependency.)
//! `outage_windows_produce_periodic_gaps` is the one with
//! any real wall-clock-shape assertion (gap counts between arrivals) —
//! if it's ever observed to flake under load, promoting it into the
//! `network` group (rename to include e.g. `round_trip`, or add a
//! `.config/nextest.toml` override) is the fix; nothing about that
//! decision belongs in `proxy.rs` itself.
//!
//! Deterministic-test policy (mirrors `tests/loopback.rs`/`tests/
//! serve.rs`): ports are ephemeral (OS-assigned via a throwaway bind);
//! `proxy::run`'s `on_bound` callback (not stdout-JSON parsing) is how
//! these tests learn the proxy's actual listen port; every thread join
//! is timeout-bounded, never a bare `.join()`.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_interop::impair::ImpairConfig;
use tst_interop::report_types::CellMetrics;
use tst_interop::verify::KlvExpect;
use tst_interop::{profiles, proxy, recv, send};

/// Ask the OS for an unused port via a throwaway UDP bind — mirrors
/// `tests/loopback.rs::free_port` (this crate's convention: small
/// per-test-file duplicated helpers rather than a shared test-utils
/// module).
fn free_port() -> u16 {
    let probe = UdpSocket::bind("127.0.0.1:0").expect("bind probe port");
    probe.local_addr().expect("read probe local_addr").port()
}

/// Poll `handle.is_finished()` until it's done or `timeout` elapses —
/// mirrors `tests/loopback.rs::join_with_timeout`.
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

/// Retry `send::run` on a bounded budget — mirrors `tests/loopback.rs::
/// send_with_retry` (a failed SRT `connect()` attempt sends no data at
/// all, so retrying the whole call never risks a double/partial send).
fn send_with_retry(
    profile: &profiles::Profile,
    url: &str,
    seconds: f64,
    budget: Duration,
) -> CellMetrics {
    let deadline = Instant::now() + budget;
    loop {
        match send::run(
            profile,
            url,
            seconds,
            None,
            false,
            tst_interop::fixtures::AuSizeMode::Compact,
            tst_interop::fixtures::KlvSet::Compact,
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

/// Spawn `proxy::run` on its own thread and block until it reports its
/// bound listen address via `on_bound` — the in-process bound-address
/// discovery these tests use instead of parsing the CLI's stdout JSON
/// line. Also returns the proxy's stop flag: set it to end the relay
/// promptly once a test's traffic is done (tests that simply wait out
/// `run_seconds` ignore it).
///
/// `schedule` mirrors `proxy::run`'s own parameter: `None` is fixed mode
/// (`cfg`'s knobs for the whole run), `Some((seed, phases, phase_s))`
/// runs the generated phase schedule.
fn spawn_proxy(
    listen: SocketAddr,
    forward: SocketAddr,
    cfg: ImpairConfig,
    schedule: Option<(u64, u32, u64)>,
    stats_json: Option<PathBuf>,
    run_seconds: u64,
) -> (
    SocketAddr,
    thread::JoinHandle<Result<proxy::ProxyStats, String>>,
    Arc<AtomicBool>,
) {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            proxy::run(
                listen,
                forward,
                cfg,
                schedule,
                stats_json,
                Some(run_seconds),
                Some(Box::new(move |addr| {
                    let _ = tx.send(addr);
                })),
                Some(stop),
            )
        })
    };
    let bound = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("proxy must report its bound address via on_bound");
    (bound, handle, stop)
}

/// Transparent mode (`ImpairConfig::default()`, i.e. loss/dup/reorder/
/// jitter all off) is the byte-transparent tier's foundation: 500
/// distinct datagrams sent through the proxy must arrive at the real
/// destination in the exact order they were sent, byte-identical.
#[test]
fn transparent_relay_preserves_order_and_bytes() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    // The proxy is stopped explicitly (`proxy_stop`, below) the moment
    // the receiver is done, so `run_seconds` here is only a safety net
    // against a wedged test — NOT a window the traffic has to fit inside.
    // This test's original shape (proxy window 3s, receiver deadline 3s,
    // both fixed before the first send) was its macOS CI flake: 4
    // failures in 2026-08-31..09-04, every one pure tail loss (492-497
    // of 500, in order). The 500 x 500us pacing sleeps below oversleep
    // to a full scheduler timeslice on a loaded runner, the send loop
    // overran 3s, and whichever window expired first truncated the
    // tail. Reproduced locally by widening the pace to 6.5ms/packet:
    // 458/500, same shape.
    let (proxy_addr, proxy_handle, proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        ImpairConfig::default(),
        None,
        None,
        30,
    );

    let payloads: Vec<Vec<u8>> = (0..500u32)
        .map(|i| format!("packet-{i:04}").into_bytes())
        .collect();
    let expected_count = payloads.len();

    // Read on its own thread, CONCURRENTLY with the send loop below,
    // rather than sending all 500 first and only then reading: the
    // latter would let all 500 datagrams pile up in the destination
    // socket's kernel receive buffer before this test ever drains it,
    // risking an overflow-driven loss under a burst this size (see the
    // outage-window test's own note for the closely related "arrival
    // timing gets destroyed by batch-draining" failure mode this same
    // pattern avoids).
    //
    // The receiver's end condition is derived from the SENDER, not the
    // wall clock: it reads until it has every packet, or until the
    // sender has been finished for `TAIL_GRACE` (the in-flight tail has
    // had that long to land). `SAFETY_CAP` only bounds a genuinely
    // broken relay so the failure is a clear message, not a hang.
    let (send_done_tx, send_done_rx) = mpsc::channel::<Instant>();
    let receiver = thread::spawn(move || {
        const TAIL_GRACE: Duration = Duration::from_secs(2);
        const SAFETY_CAP: Duration = Duration::from_secs(20);
        let started = Instant::now();
        let mut received = Vec::with_capacity(expected_count);
        let mut buf = [0u8; 256];
        let mut send_done_at: Option<Instant> = None;
        while received.len() < expected_count {
            if send_done_at.is_none() {
                send_done_at = send_done_rx.try_recv().ok();
            }
            if send_done_at.is_some_and(|t| t.elapsed() >= TAIL_GRACE) {
                break;
            }
            assert!(
                started.elapsed() < SAFETY_CAP,
                "receiver hit its {SAFETY_CAP:?} safety cap with {}/{expected_count} packets",
                received.len()
            );
            match dest_sock.recv(&mut buf) {
                Ok(n) => received.push(buf[..n].to_vec()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("recv error: {e}"),
            }
        }
        received
    });

    // A small per-send pace (500us/packet, ~250ms total) rather than
    // firing all 500 in a zero-delay tight loop: this single-threaded
    // proxy's recv/decide/send loop has real per-packet syscall +
    // scheduling overhead, and an instantaneous burst this size can
    // outrun it fast enough to overflow the LISTEN socket's own kernel
    // receive buffer before the proxy ever gets scheduled to drain it —
    // genuine UDP behavior, not a proxy bug, but not what this test
    // means to exercise either (it's checking transparent-mode
    // byte/order fidelity, not this crate's raw burst throughput
    // ceiling).
    //
    // Paced against an ABSOLUTE schedule (the outage-window test's
    // pattern), sleeping only while ahead of it: a late wakeup is
    // absorbed by the following iterations instead of accumulating, so
    // the whole send phase is bounded by ~250ms plus real syscall time —
    // never 500 x (however far one sleep overshoots on a loaded runner).
    const PACE: Duration = Duration::from_micros(500);
    let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
    let send_start = Instant::now();
    for (i, p) in payloads.iter().enumerate() {
        sender
            .send_to(p, proxy_addr)
            .expect("send datagram to proxy");
        let target = PACE * (i as u32 + 1);
        let elapsed = send_start.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }
    }
    let _ = send_done_tx.send(Instant::now());

    let received = join_with_timeout(receiver, Duration::from_secs(25));

    // Everything the receiver was ever going to see has landed (or the
    // grace expired): end the proxy now and collect its stats.
    proxy_stop.store(true, Ordering::Relaxed);
    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");

    assert_eq!(
        received, payloads,
        "a transparent proxy must preserve order and content exactly"
    );

    assert_eq!(stats.forwarded, payloads.len() as u64);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.duped, 0);
}

/// The client peer is learned from the FIRST forward-direction datagram
/// only (per `proxy::run`'s own module doc) -- a stray datagram from a
/// third socket later in the session must not re-aim the return path.
/// Regression test for a bug where `client_peer` was unconditionally
/// overwritten on every forward-direction packet.
#[test]
fn spoofed_third_party_datagram_does_not_hijack_the_return_path() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    let (proxy_addr, proxy_handle, _proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        ImpairConfig::default(),
        None,
        None,
        3,
    );

    let real_client = UdpSocket::bind("127.0.0.1:0").expect("bind real client socket");
    real_client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let spoofer = UdpSocket::bind("127.0.0.1:0").expect("bind spoofer socket");
    spoofer
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");

    // Real client's first datagram learns client_peer.
    real_client
        .send_to(b"hello-from-real-client", proxy_addr)
        .expect("send from real client");
    let mut buf = [0u8; 64];
    let n = dest_sock
        .recv(&mut buf)
        .expect("destination must see the real client's datagram");
    assert_eq!(&buf[..n], b"hello-from-real-client");

    // A stray datagram from a THIRD, unrelated socket -- must still be
    // relayed (impairment applies to forward-direction traffic
    // regardless of source), but must NOT re-aim the learned return path.
    spoofer
        .send_to(b"spoofed", proxy_addr)
        .expect("send spoofed datagram");
    let n = dest_sock
        .recv(&mut buf)
        .expect("spoofed datagram must still be relayed");
    assert_eq!(&buf[..n], b"spoofed");

    // The "server" (forward) now replies -- this must reach the REAL
    // client, not the spoofer.
    dest_sock
        .send_to(b"reply", proxy_addr)
        .expect("send reply from destination");

    let n = real_client
        .recv(&mut buf)
        .expect("real client must receive the reply");
    assert_eq!(&buf[..n], b"reply");
    assert!(
        spoofer.recv(&mut buf).is_err(),
        "the spoofer must NOT receive the reply -- the return path must stay aimed at the real client"
    );

    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");
    assert_eq!(
        stats.forwarded, 2,
        "both the real and spoofed datagrams were forwarded"
    );
}

/// (Fix-wave regression) A datagram from a NEW source, arriving after
/// the current `client_peer` has gone quiet for longer than
/// `CLIENT_RELEARN_GRACE`, must be treated as a legitimate reconnect —
/// replies must route to the NEW source afterward, not the stale
/// original one. Positive-control twin of the spoofed-datagram test
/// above (that one's spoofer arrives with essentially zero gap and must
/// NOT hijack the path; this one's "new client" arrives after a
/// genuine multi-second quiet gap and MUST take over it) — together
/// they pin exactly the boundary the grace period is meant to draw.
/// Found empirically while developing `recv --managed`: a `send
/// --managed` reconnect after a real SRT-level break uses a brand-new
/// ephemeral source port every attempt (`tst-srt`'s `Socket::
/// connect_with` never explicitly binds a local port first), and the
/// proxy's OLD "learn the first datagram, forever" rule left every
/// reconnect attempt's replies routed to a dead port, so the handshake
/// could never complete no matter how many times either side retried.
#[test]
fn client_relearns_after_a_genuine_quiet_period() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    let (proxy_addr, proxy_handle, _proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        ImpairConfig::default(),
        None,
        None,
        10,
    );

    let original_client = UdpSocket::bind("127.0.0.1:0").expect("bind original client socket");
    original_client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let reconnected_client =
        UdpSocket::bind("127.0.0.1:0").expect("bind reconnected client socket");
    reconnected_client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");

    // Original client establishes client_peer.
    original_client
        .send_to(b"hello-from-original", proxy_addr)
        .expect("send from original client");
    let mut buf = [0u8; 64];
    let n = dest_sock
        .recv(&mut buf)
        .expect("destination must see the original client's datagram");
    assert_eq!(&buf[..n], b"hello-from-original");

    // Let client_peer go quiet past the grace period — simulates the
    // real gap a genuine SRT peer-idle-timeout break produces (per this
    // module's own doc comment: confirmed empirically to be several
    // seconds, never sub-second — 2.2s sits safely past the proxy's own
    // 2s `CLIENT_RELEARN_GRACE`).
    thread::sleep(Duration::from_millis(2200));

    // "Reconnect": a DIFFERENT source sends forward-direction traffic.
    reconnected_client
        .send_to(b"hello-from-reconnect", proxy_addr)
        .expect("send from reconnected client");
    let n = dest_sock
        .recv(&mut buf)
        .expect("destination must see the reconnected client's datagram");
    assert_eq!(&buf[..n], b"hello-from-reconnect");

    // The reply must now route to the RECONNECTED client, not the
    // stale original one.
    dest_sock
        .send_to(b"reply", proxy_addr)
        .expect("send reply from destination");
    let n = reconnected_client
        .recv(&mut buf)
        .expect("reconnected client must receive the reply after a genuine reconnect");
    assert_eq!(&buf[..n], b"reply");
    assert!(
        original_client.recv(&mut buf).is_err(),
        "the stale original client must NOT receive the reply after a genuine reconnect"
    );

    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");
    assert_eq!(
        stats.forwarded, 2,
        "both the original and reconnected datagrams were forwarded"
    );
}

/// `outage_period_s=2, outage_dur_s=1` at 10Hz for ~6s (3 full periods):
/// the receiver must observe MULTIPLE periodic ~1s gaps where the outage
/// windows suppressed traffic — not just one contiguous pause — and the
/// stats JSON written at exit must agree with `proxy::run`'s own
/// returned counters.
///
/// Three periods (not two) is deliberate: a single-max-gap check can't
/// tell "period=2s,dur=1s repeating" apart from a proxy bug that (say)
/// computed `elapsed_ms` from the wrong reference instant and produced
/// one contiguous ~2s pause instead of two separate ~1s ones — both
/// shapes have the same max gap and roughly the same drop fraction. This
/// test instead asserts on the COUNT of qualifying gaps. A gap only
/// shows up in `arrivals.windows(2)` when there's a real delivery both
/// immediately before AND after it (the very first and very last outage
/// windows the send loop happens to straddle are NOT bounded this way,
/// since there's no prior/subsequent arrival to diff against) — so with
/// only 2 periods (one interior down window) an unlucky phase alignment
/// could leave zero or one FULLY interior down window observable
/// regardless of correctness. 3 periods guarantees at least 2 fully
/// interior down windows land inside the observed arrivals no matter
/// where in the cycle the send loop happens to start (worked through in
/// the review-fix report).
#[test]
fn outage_windows_produce_periodic_gaps() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    let stats_path = std::env::temp_dir().join(format!(
        "tst-interop-proxy-outage-stats-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos(),
    ));

    let cfg = ImpairConfig {
        outage_period_s: Some(2),
        outage_dur_s: 1,
        ..ImpairConfig::default()
    };
    let (proxy_addr, proxy_handle, _proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        cfg,
        None,
        Some(stats_path.clone()),
        8,
    );

    const HZ: u32 = 10;
    const TOTAL: u32 = HZ * 6; // ~6s at 10Hz -- 3 full outage periods, see the test's own doc comment
    // Send window is ~6s; give the collector a further 2s margin past
    // that for the last few packets' proxy-relay + wire latency to
    // land. Runs on its own thread, CONCURRENTLY with the paced send
    // loop below — reading only AFTER the send loop finished would just
    // batch-drain whatever piled up in the kernel receive buffer over
    // the whole 6s window, and every arrival would then read back nearly
    // simultaneously: the real wall-clock gaps this test's assertions
    // depend on only exist if arrivals are timestamped as they actually
    // land, not after the fact.
    let collect_for = Duration::from_secs(6) + Duration::from_secs(2);
    let receiver = thread::spawn(move || {
        let deadline = Instant::now() + collect_for;
        let mut arrivals: Vec<Instant> = Vec::new();
        let mut buf = [0u8; 16];
        while Instant::now() < deadline {
            match dest_sock.recv(&mut buf) {
                Ok(_) => arrivals.push(Instant::now()),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("recv error: {e}"),
            }
        }
        arrivals
    });

    let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
    let send_start = Instant::now();
    for i in 0..TOTAL {
        sender
            .send_to(&i.to_le_bytes(), proxy_addr)
            .expect("send datagram to proxy");
        let target = Duration::from_millis((i as u64 + 1) * 1000 / HZ as u64);
        let elapsed = send_start.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }
    }

    let arrivals = join_with_timeout(receiver, Duration::from_secs(10));

    assert!(
        !arrivals.is_empty(),
        "expected at least some packets to survive the up windows"
    );
    assert!(
        arrivals.len() < TOTAL as usize,
        "expected some packets to be dropped by the outage windows, got all {}/{TOTAL}",
        arrivals.len()
    );

    // Count, not just detect, outage-sized gaps between consecutive
    // arrivals — a single contiguous pause (e.g. a proxy bug that
    // dropped for one long stretch instead of periodically) would also
    // produce one big max gap, so a max-gap-only check can't tell
    // "periodic" apart from "one pause" (see this test's own doc
    // comment). Threshold (500ms) is generous — half the configured 1s
    // outage — so ordinary scheduling jitter can't flake this; requiring
    // >= 2 qualifying gaps is what actually exercises the periodicity
    // wiring (the elapsed-ms reference instant and the period/duration
    // math), not just "a pause happened somewhere."
    let big_gaps: Vec<Duration> = arrivals
        .windows(2)
        .map(|w| w[1].duration_since(w[0]))
        .filter(|&gap| gap >= Duration::from_millis(500))
        .collect();
    assert!(
        big_gaps.len() >= 2,
        "expected at least 2 separate outage-sized (>=500ms) gaps between arrivals — \
         period=2s,dur=1s over a 6s send window should straddle multiple periodic outage \
         windows, not one contiguous pause; found {} qualifying gap(s): {:?}",
        big_gaps.len(),
        big_gaps
    );

    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");
    assert!(
        stats.dropped > 0,
        "expected some packets to be dropped by the outage windows"
    );
    assert!(
        stats.forwarded > 0,
        "expected some packets to survive the up windows"
    );
    // Roughly half of a 6s send window (3 full 2s periods) falls inside
    // outage windows (period=2s, dur=1s) — generous 30%/70% band around
    // that 50% nominal split to tolerate where the 6s window happens to
    // land relative to the proxy's own outage-window phase.
    let total = stats.dropped + stats.forwarded;
    let dropped_frac = stats.dropped as f64 / total as f64;
    assert!(
        (0.3..=0.7).contains(&dropped_frac),
        "dropped fraction {dropped_frac:.2} out of the expected ~50% band (dropped={}, forwarded={})",
        stats.dropped,
        stats.forwarded
    );

    let json = std::fs::read_to_string(&stats_path).expect("read stats json written at exit");
    let parsed: proxy::ProxyStats = serde_json::from_str(&json).expect("stats json must parse");
    assert_eq!(
        parsed.dropped, stats.dropped,
        "stats file must match proxy::run's own returned counters"
    );
    assert_eq!(parsed.forwarded, stats.forwarded);
    assert_eq!(parsed.seed, cfg.seed);
    assert_eq!(parsed.config.outage_period_s, cfg.outage_period_s);
    assert_eq!(parsed.config.outage_dur_s, cfg.outage_dur_s);

    let _ = std::fs::remove_file(&stats_path);
}

/// A process-and-time unique temp path for a stats JSON — mirrors the
/// expression the outage-window test above builds inline (this file's
/// convention is small local helpers, and the two scheduled-mode tests
/// below both need one).
fn unique_stats_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tst-interop-proxy-{tag}-stats-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos(),
    ))
}

/// Drain `sock` on its own thread until `stop` is set, returning how many
/// datagrams landed. Neither scheduled-mode test below asserts on arrival
/// timing or content (they assert on the proxy's own counters), but the
/// destination must still be drained while traffic flows so nothing piles
/// up in its kernel receive buffer.
fn spawn_drain(sock: UdpSocket, stop: Arc<AtomicBool>) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        let mut seen = 0usize;
        while !stop.load(Ordering::Relaxed) {
            match sock.recv(&mut buf) {
                Ok(_) => seen += 1,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("recv error: {e}"),
            }
        }
        seen
    })
}

/// Scheduled mode (`--schedule seed=9,phases=2,phase_s=1`): the proxy
/// must echo the exact phase table it ran into its stats JSON, and split
/// its own totals across one counter per phase.
///
/// Two 1-second phases under ~3s of traffic guarantees the phase boundary
/// is crossed (phase 0 covers elapsed `[0,1s)`, phase 1 everything after
/// — `Engine::phase_index` clamps to the last phase), which is what makes
/// "both phases saw traffic" a real assertion rather than a tautology.
///
/// Deliberately NOT asserting exact drop counts per phase: the number of
/// packets that land in each phase depends on wall-clock pacing, so only
/// the engine's DECISIONS are seeded-deterministic here, not how many of
/// them each phase gets. The invariant that matters — and the one Task
/// 14's soak verdicts will lean on — is that every counted packet lands
/// in exactly one phase, i.e. the per-phase sums reproduce the totals.
///
/// Left out of the `network` nextest group (its name matches no
/// `test(...)` filter) per this file's module doc: ephemeral port, no
/// wall-clock-shape assertion, ~3s of traffic against the default
/// profile's 60s safety net.
#[test]
fn scheduled_proxy_echoes_its_phase_table_and_per_phase_counters() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    let stats_path = unique_stats_path("scheduled");

    // (schedule seed, phase count, seconds per phase). The schedule seed
    // is its own key, independent of the engine seed below — see
    // `impair::SCHEDULE_SALT`.
    const SCHED_SEED: u64 = 9;
    const PHASES: u32 = 2;
    const PHASE_S: u64 = 1;

    let cfg = ImpairConfig {
        seed: 1,
        ..ImpairConfig::default()
    };
    let (proxy_addr, proxy_handle, proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        cfg,
        Some((SCHED_SEED, PHASES, PHASE_S)),
        Some(stats_path.clone()),
        30,
    );

    let drain_stop = Arc::new(AtomicBool::new(false));
    let drainer = spawn_drain(dest_sock, Arc::clone(&drain_stop));

    // 200 pkt/s for ~3s, paced against an ABSOLUTE schedule (the
    // transparent-relay test's pattern: a late wakeup is absorbed by the
    // following iterations instead of accumulating).
    const PACE: Duration = Duration::from_millis(5);
    const TOTAL: u32 = 600;
    let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
    let send_start = Instant::now();
    for i in 0..TOTAL {
        sender
            .send_to(&i.to_le_bytes(), proxy_addr)
            .expect("send datagram to proxy");
        let target = PACE * (i + 1);
        let elapsed = send_start.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }
    }
    // Let the proxy finish deciding the tail before stopping it: it
    // breaks out of its loop at the top, so datagrams still sitting in
    // its socket buffer would never be counted at all. The schedule's own
    // worst-case delay (base <=60ms + jitter <=40ms + reorder hold
    // <=300ms) is comfortably inside this.
    thread::sleep(Duration::from_millis(600));

    proxy_stop.store(true, Ordering::Relaxed);
    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");
    drain_stop.store(true, Ordering::Relaxed);
    let delivered = join_with_timeout(drainer, Duration::from_secs(5));

    // Non-vacuity floor, deliberately loose (this is not a throughput
    // test): the run must have actually relayed a substantial share of
    // the traffic, so the per-phase assertions below aren't about a
    // handful of packets.
    assert!(
        stats.forwarded + stats.dropped >= (TOTAL / 2) as u64,
        "expected most of the {TOTAL} datagrams to be decided, got forwarded={} dropped={} (delivered={delivered})",
        stats.forwarded,
        stats.dropped,
    );

    assert_eq!(
        stats.phases.len(),
        PHASES as usize,
        "one counter per scheduled phase"
    );
    for (i, p) in stats.phases.iter().enumerate() {
        assert_eq!(p.index as usize, i, "phase counters are position-indexed");
        assert!(
            p.forwarded + p.dropped > 0,
            "phase {i} saw no traffic — the ~3s send window must straddle both 1s phases: {:?}",
            stats.phases
        );
    }
    assert_eq!(
        stats.phases.iter().map(|p| p.forwarded).sum::<u64>(),
        stats.forwarded,
        "per-phase forwarded must sum to the total"
    );
    assert_eq!(
        stats.phases.iter().map(|p| p.dropped).sum::<u64>(),
        stats.dropped,
        "per-phase dropped must sum to the total"
    );
    assert_eq!(
        stats.phases.iter().map(|p| p.duped).sum::<u64>(),
        stats.duped,
        "per-phase duped must sum to the total"
    );

    let json = std::fs::read_to_string(&stats_path).expect("read stats json written at exit");
    let parsed: proxy::ProxyStats = serde_json::from_str(&json).expect("stats json must parse");
    let echo = parsed
        .config
        .schedule
        .as_ref()
        .expect("a scheduled run must echo its schedule");
    assert_eq!(
        (echo.seed, echo.phases, echo.phase_s),
        (SCHED_SEED, PHASES, PHASE_S)
    );
    assert_eq!(echo.table.len(), PHASES as usize);
    assert_eq!(
        echo.table,
        tst_interop::impair::generate_schedule(SCHED_SEED, PHASES),
        "the echoed table must be exactly what this seed generates"
    );
    assert_eq!(
        parsed.phases, stats.phases,
        "stats file must match proxy::run's own returned per-phase counters"
    );
    assert_eq!(parsed.forwarded, stats.forwarded);
    assert_eq!(parsed.dropped, stats.dropped);

    let _ = std::fs::remove_file(&stats_path);
}

/// Fixed mode (no `--schedule`) writes NO schedule echo and exactly one
/// phase counter carrying the whole run — the degenerate one-phase shape
/// `impair::Engine::new` already models internally, now visible in the
/// stats file. `report soak`'s one-phase path relies on this shape.
#[test]
fn fixed_mode_proxy_records_one_phase_and_no_schedule() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    let stats_path = unique_stats_path("fixed");

    const TOTAL: u32 = 20;
    let (proxy_addr, proxy_handle, proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        ImpairConfig::default(),
        None,
        Some(stats_path.clone()),
        30,
    );

    // Transparent config: every datagram is forwarded, so the proxy is
    // known to have decided all of them once the destination has seen all
    // of them — the sender-driven end condition the transparent-relay
    // test uses, rather than a fixed sleep.
    let receiver = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = 0u32;
        let mut buf = [0u8; 64];
        while seen < TOTAL {
            assert!(Instant::now() < deadline, "only {seen}/{TOTAL} relayed");
            match dest_sock.recv(&mut buf) {
                Ok(_) => seen += 1,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("recv error: {e}"),
            }
        }
        seen
    });

    let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
    for i in 0..TOTAL {
        sender
            .send_to(&i.to_le_bytes(), proxy_addr)
            .expect("send datagram to proxy");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(join_with_timeout(receiver, Duration::from_secs(15)), TOTAL);

    proxy_stop.store(true, Ordering::Relaxed);
    let stats =
        join_with_timeout(proxy_handle, Duration::from_secs(10)).expect("proxy::run must succeed");

    assert_eq!(stats.forwarded, TOTAL as u64);
    assert_eq!(stats.dropped, 0);
    assert_eq!(stats.phases.len(), 1, "fixed mode is one implicit phase");
    assert_eq!(stats.phases[0].index, 0);
    assert_eq!(stats.phases[0].forwarded, stats.forwarded);
    assert_eq!(stats.phases[0].dropped, stats.dropped);
    assert_eq!(stats.phases[0].duped, stats.duped);

    let json = std::fs::read_to_string(&stats_path).expect("read stats json written at exit");
    let parsed: proxy::ProxyStats = serde_json::from_str(&json).expect("stats json must parse");
    assert!(
        parsed.config.schedule.is_none(),
        "a fixed-mode run must not claim a schedule it never ran"
    );
    assert_eq!(parsed.phases, stats.phases);

    let _ = std::fs::remove_file(&stats_path);
}

/// A `stats_json` whose containing directory doesn't exist must not
/// turn an otherwise-successful relay into a failure — see `proxy::run`'s
/// module doc's "Stats" section. This exercises `run`'s FINAL (at-exit)
/// stats write, the one guaranteed to fire even for a short run (the
/// periodic write only fires past `STATS_INTERVAL`, well beyond what
/// this test's short `run_seconds` needs to cover). Real traffic must
/// still flow, and `proxy::run` must still return `Ok` carrying the
/// correct counters, despite the doomed write.
#[test]
fn unwritable_stats_path_does_not_fail_the_relay() {
    let dest_sock = UdpSocket::bind("127.0.0.1:0").expect("bind destination socket");
    dest_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let dest_addr = dest_sock.local_addr().expect("destination local_addr");

    // A path whose PARENT directory doesn't exist: `write_stats_atomic`'s
    // `fs::write` (into a sibling `.tmp` path, same containing directory)
    // fails with `NotFound` every time, deterministically — no reliance
    // on real filesystem permissions, which would vary across CI
    // platforms/users.
    let bogus_dir = std::env::temp_dir().join(format!(
        "tst-interop-proxy-nonexistent-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos(),
    ));
    let stats_path = bogus_dir.join("stats.json");
    assert!(
        !bogus_dir.exists(),
        "the whole point of this test is that this directory does NOT exist"
    );

    let (proxy_addr, proxy_handle, _proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        dest_addr,
        ImpairConfig::default(),
        None,
        Some(stats_path.clone()),
        2,
    );

    let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender socket");
    sender
        .send_to(b"hello", proxy_addr)
        .expect("send datagram to proxy");

    // Real traffic must still be relayed despite the doomed stats path.
    let mut buf = [0u8; 16];
    let n = dest_sock
        .recv(&mut buf)
        .expect("datagram must still be relayed despite an unwritable stats path");
    assert_eq!(&buf[..n], b"hello");

    let stats = join_with_timeout(proxy_handle, Duration::from_secs(10))
        .expect("proxy::run must still return Ok even though its stats write failed");
    assert_eq!(stats.forwarded, 1);
    assert_eq!(stats.dropped, 0);

    assert!(
        !stats_path.exists(),
        "sanity check: the stats file was genuinely never written (parent directory never existed)"
    );
}

/// SRT sender -> impaired proxy (2% loss, 10ms jitter, fixed seed) ->
/// real SRT listener, full round trip: SRT's own retransmission over
/// UDP must recover the receiver's invariant checks (`recv report pass
/// == true`) despite the injected loss/jitter — that recovery is the
/// entire point of this cell (see `tst_interop::impair`'s module doc).
///
/// Named with `round_trip` deliberately — see this file's module doc's
/// "nextest group placement" section for why.
///
/// The `GracefulSrtClose` send-side close wait (300ms, reasoned against
/// unimpaired loopback — see `transport.rs`) gets real pressure here, and
/// was the first suspect for this test's video-AU deficit — but it was
/// experimentally RULED OUT (raising it to 1000ms changed nothing; see
/// the assertion's own comment for the full evidence chain, which
/// isolates the real cause to a `tst-pipeline`/`tst-srt` gap unrelated to
/// this margin, this proxy, or impairment at all). Verified enforcement
/// is exact for KLV (`klv_set_sha256` equality) and NARROWLY bounded for
/// video (deficit 0 or 1, never more — never a `send`-side error during
/// close either). A WIDER video deficit or a KLV mismatch would still
/// fail this test loudly — that is the real signal to watch for: don't
/// widen these bounds to chase a flake, investigate it the way the
/// assertion's own comment documents this one was.
#[test]
fn srt_round_trip_through_lossy_proxy_recovers_via_retransmission() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    const SECONDS: f64 = 5.0;

    let listener_port = free_port();
    let recv_url = format!("srt://127.0.0.1:{listener_port}?mode=listener");
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

    let cfg = ImpairConfig {
        loss_pct: 2.0,
        jitter_ms_max: 10,
        seed: 42,
        ..ImpairConfig::default()
    };
    let forward_addr: SocketAddr = format!("127.0.0.1:{listener_port}").parse().unwrap();
    let (proxy_addr, proxy_handle, _proxy_stop) = spawn_proxy(
        "127.0.0.1:0".parse().unwrap(),
        forward_addr,
        cfg,
        None,
        None,
        15,
    );

    let send_url = format!("srt://127.0.0.1:{}", proxy_addr.port());
    let send_metrics = send_with_retry(profile, &send_url, SECONDS, Duration::from_secs(10));

    let recv_report =
        join_with_timeout(recv_handle, Duration::from_secs(20)).expect("recv::run must succeed");

    assert!(
        recv_report.pass,
        "recv failures over the lossy/jittered proxy: {:?}",
        recv_report.failures
    );
    assert_eq!(
        send_metrics.klv_set_sha256, recv_report.metrics.klv_set_sha256,
        "SRT must recover the full KLV record set despite injected loss"
    );
    // `recv_report.pass` alone doesn't catch a video tail-truncation
    // flake (its 70% nominal-count slack floor would tolerate over a
    // second of trailing AUs silently vanishing) -- but exact equality
    // is ALSO wrong here, for a reason that is NOT the `GracefulSrtClose`/
    // POST_START_GRACE margin this test originally suspected. Isolated
    // by experiment (see the arc's fix-round report for the full
    // evidence chain):
    //   1. Raising `transport::CLOSE_DRAIN` 300ms -> 1000ms did NOT
    //      change the deficit (10/10 runs each, always sent=150/
    //      received=149) -- rules out the send-side close margin.
    //   2. Giving `recv::run` a much larger wait budget (15s vs. the
    //      sender's real 5s) did NOT close the gap either (video_aus
    //      stayed 149) -- rules out the receive-side grace window too.
    //   3. `send_metrics.bytes`/`stream_sha256` matched
    //      `recv_report.metrics.bytes`/`stream_sha256` EXACTLY every
    //      run -- SRT delivered every wire byte; this is not data loss.
    //   4. Instrumenting `recv::recv_over_transport`'s loop showed the
    //      session ends with `ShellErrorKind::TransportBroken`, not
    //      `EndOfStream` -- `DemuxReceiver::recv_event()` only flushes
    //      its pending-PES reassembly state (needed to emit the final
    //      video AU: H.264 PES packets carry `PES_packet_length=0` and
    //      are demuxed by watching for the NEXT PES's PUSI, so the
    //      stream's last AU has no "next" to complete it without an
    //      explicit flush) on `EndOfStream` specifically; `Broken`
    //      bypasses that flush entirely.
    //   5. Confirmed this is NOT proxy/impairment-specific: the same
    //      exact-video-AU check against `tests/loopback.rs`'s plain,
    //      UNIMPAIRED SRT loopback cell showed the identical shape (5/5
    //      runs, deficit exactly 1, e.g. sent=90/received=89) -- this is
    //      a pre-existing gap in `tst-pipeline`/`tst-srt`'s
    //      `TransportBroken`-vs-`Closed` classification on a clean SRT
    //      disconnect, not something this proxy task introduced or can
    //      fix from `tst-interop` (flagged separately to the team lead;
    //      out of scope here).
    // Given that, the correct assertion for THIS test is the same
    // narrow, named bound as before -- deficit is 0 or 1, NEVER more --
    // but the comment above now reflects the actual, verified mechanism
    // instead of the disproven TSBPD-margin theory.
    let video_deficit = send_metrics
        .video_aus
        .saturating_sub(recv_report.metrics.video_aus);
    assert!(
        video_deficit <= 1 && recv_report.metrics.video_aus <= send_metrics.video_aus,
        "SRT with retransmission over 2% loss must deliver every video AU except, at \
         most, the one whose demux-side flush is skipped by the TransportBroken-vs-\
         EndOfStream gap documented above (sent={}, received={}, allowed deficit=0..=1)",
        send_metrics.video_aus,
        recv_report.metrics.video_aus
    );

    // Don't wait out the proxy's own run_seconds budget on the happy
    // path (mirrors tests/serve.rs's convention for its lingering
    // producer threads) — only surface its result if it already
    // finished (a fast failure). nextest reaps this test process's
    // threads at process exit either way.
    if proxy_handle.is_finished() {
        proxy_handle
            .join()
            .expect("proxy thread panicked")
            .expect("proxy::run must succeed");
    }
}

/// `--stats-json` with no following value must exit 2 like every sibling
/// flag (`--listen`/`--forward`/`--jitter`/etc.), not silently no-op.
/// Regression test for a bug where `args.get(i+1).map(PathBuf::from)`
/// swallowed a missing value into `None` instead of requiring presence.
/// Drives the real built CLI binary as a subprocess (the only way to
/// exercise `main.rs`'s argument loop directly -- it calls
/// `std::process::exit`, so it can't be unit-tested in-process).
///
/// `--run-seconds 1` is included even though the FIXED behavior never
/// reaches it (argument validation fails and exits before `proxy::run`
/// is ever called): without it, a REGRESSION back to the silent-no-op
/// bug would let `stats_json` fall back to `None` and `run_seconds` stay
/// unset too, starting a real relay that runs until killed -- turning a
/// test failure into an indefinitely hanging subprocess. This bounds
/// that failure mode to ~1s instead, so a regression fails fast with a
/// clear mismatch instead of wedging the test run.
#[test]
fn stats_json_missing_value_exits_with_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "proxy",
            "--listen",
            "127.0.0.1:0",
            "--forward",
            "127.0.0.1:1",
            "--run-seconds",
            "1",
            "--stats-json",
        ])
        .output()
        .expect("spawn tst-interop binary");

    assert_eq!(
        output.status.code(),
        Some(2),
        "missing --stats-json value must exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("stats-json"),
        "usage error should name the flag, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `--schedule` overrides loss/jitter/reorder/delay per phase, so passing
/// one of those alongside it must be a usage error naming the offending
/// flag — NOT a silently ignored knob on a run that may be hours long.
/// Drives the real binary (argument validation calls `std::process::exit`,
/// so it can't be unit-tested in-process).
#[test]
fn schedule_combined_with_a_fixed_impairment_flag_exits_with_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "proxy",
            "--listen",
            "127.0.0.1:0",
            "--forward",
            "127.0.0.1:1",
            "--run-seconds",
            "1",
            "--schedule",
            "seed=7,phases=2,phase_s=1",
            "--loss",
            "2",
        ])
        .output()
        .expect("spawn tst-interop binary");

    assert_eq!(
        output.status.code(),
        Some(2),
        "--schedule with --loss must exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--loss") && stderr.contains("--schedule"),
        "usage error should name both conflicting flags, got: {stderr}"
    );
}

/// Positive control for the exclusion above: `--schedule` WITH the knobs
/// that stay per-run (`--dup`, `--seed`, `--outage`) is a valid
/// combination and must run normally — without this, a check that simply
/// rejected `--schedule` outright would satisfy the test above.
#[test]
fn schedule_combined_with_per_run_knobs_is_accepted() {
    let output = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args([
            "proxy",
            "--listen",
            "127.0.0.1:0",
            "--forward",
            "127.0.0.1:1",
            "--run-seconds",
            "1",
            "--schedule",
            "seed=7,phases=2,phase_s=1",
            "--dup",
            "1",
            "--seed",
            "3",
            "--outage",
            "period=10s,dur=1s",
        ])
        .output()
        .expect("spawn tst-interop binary");

    assert_eq!(
        output.status.code(),
        Some(0),
        "a scheduled run with only per-run knobs must succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
