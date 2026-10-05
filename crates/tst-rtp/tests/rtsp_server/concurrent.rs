//! Concurrent unicast clients.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspServer, RtspServerBuilder};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// N concurrent clients all connect + DESCRIBE successfully.
#[test]
fn ten_concurrent_describes() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = std::sync::Arc::new(format!("rtsp://127.0.0.1:{port}/live"));

    let mut handles = vec![];
    for _ in 0..10 {
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            let mut client = RtspClient::connect(&url).unwrap();
            client.options().unwrap();
            let sdp = client.describe().unwrap();
            assert!(!sdp.media.is_empty());
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    server.stop().ok();
}

/// active_sessions count tracks correctly across concurrent connects + drops.
#[test]
fn active_sessions_count_reflects_concurrent_clients() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let clients: Vec<_> = (0..5)
        .map(|_| {
            let mut c = RtspClient::connect(&url).unwrap();
            c.options().unwrap();
            c
        })
        .collect();
    // Brief settle.
    std::thread::sleep(Duration::from_millis(200));
    assert!(server.stats().active_sessions >= 5);
    drop(clients);
    std::thread::sleep(Duration::from_millis(200));
    server.stop().ok();
}

/// Adversarial: an unauthenticated connection BURST must never push
/// `active_sessions` past `max_sessions`, and the excess connections must
/// be refused.
///
/// This reproduces the check-then-act race the atomic-reserve fix closes:
/// the single-threaded accept loop used to `load()` the counter, check the
/// cap, then spawn a task that did the `fetch_add` later. A burst that
/// arrives faster than the spawned tasks get polled to increment makes
/// every accept read the same stale low count, so all pass the check and
/// the cap is blown. With the fix the accept loop reserves the slot with a
/// single `fetch_add` BEFORE spawning, so a burst sees the running total.
///
/// Determinism: each accepted connection sends a PARTIAL request (no
/// terminating CRLFCRLF) so its session parks in the server read loop and
/// keeps holding its slot — the cap stays saturated for the whole
/// observation window. We open a large burst as fast as possible (no
/// per-connection handshake), then poll `active_sessions` repeatedly and
/// assert the cap is never exceeded. A refused connection is one the server
/// dropped (the TCP read returns EOF). Bounded + hang-proof: every socket op
/// has a read timeout and the whole test runs under a wall-clock ceiling.
#[test]
fn unauth_connection_burst_never_exceeds_max_sessions() {
    const CAP: usize = 2;
    const BURST: usize = 32;

    let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    builder.max_sessions(CAP);
    let server = builder.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let addr = server.local_addr().unwrap();

    // Open the burst as fast as the kernel will let us — no handshake, just
    // raw connects + a partial line so the session parks holding its slot.
    // Keep the streams alive in `held` so accepted sessions stay open.
    let mut held: Vec<TcpStream> = Vec::with_capacity(BURST);
    for _ in 0..BURST {
        match TcpStream::connect(addr) {
            Ok(mut s) => {
                s.set_read_timeout(Some(Duration::from_millis(200))).ok();
                // Partial request — never terminated, so an accepted session
                // parks in the read loop and holds its slot.
                let _ = s.write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\n");
                held.push(s);
            }
            Err(_) => { /* kernel backlog full — still counts as refused */ }
        }
    }

    // Observe the live session count over a bounded window. It must NEVER
    // exceed the cap. (Before the fix this spikes well above CAP.)
    let mut max_observed = 0usize;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        let n = server.stats().active_sessions;
        max_observed = max_observed.max(n);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        max_observed <= CAP,
        "active_sessions reached {max_observed}, exceeding max_sessions={CAP} — the burst bypassed the cap"
    );

    // The excess connections must be REFUSED: the server drops the TCP, so a
    // read on a refused socket returns EOF (0) or a connection-reset error,
    // whereas an ACCEPTED-and-parked session keeps the connection open (its
    // read times out with no bytes). With CAP accepted, the rest are refused.
    // (The load-bearing assertion is the cap above; this corroborates that the
    // overflow connections were actively dropped rather than silently parked.)
    //
    // Census by bounded RE-POLL, not a single sweep: under CI load (windows
    // especially) an RST can land after one 200 ms read window, and a single
    // pass misclassifies it as parked. Unclassified sockets are re-read until
    // the refused threshold is met or the deadline expires; a met threshold
    // exits immediately, so the common case is FASTER than the old full sweep.
    let threshold = BURST - CAP - 4;
    let mut refused = 0usize;
    let census_deadline = Instant::now() + Duration::from_secs(5);
    let mut pending = held; // classified sockets drop out of the pool
    while refused < threshold && Instant::now() < census_deadline && !pending.is_empty() {
        let mut still_pending = Vec::with_capacity(pending.len());
        for mut s in pending {
            let mut buf = [0u8; 64];
            match s.read(&mut buf) {
                Ok(0) => refused += 1, // clean EOF — server dropped it.
                Ok(_) => {}            // response bytes — accepted; drop it from the census pool.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    still_pending.push(s); // parked or not-yet-signaled — retry next round.
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    refused += 1; // RST/abort — server dropped it (aborts are the
                    // common Windows surface of a refused socket).
                }
                Err(_) => {} // anything else (e.g. EINTR): drop from the pool, count neither —
                             // never recycle an error that can't become readable.
            }
        }
        pending = still_pending;
    }
    assert!(
        refused >= threshold,
        "expected most of the {BURST} burst connections to be refused (at most {CAP} accepted), only {refused} were"
    );

    // Drop every held connection — accepted sessions close, refused ones are
    // already gone. The counter must drain back to 0 (no leaked slots on the
    // accept-reserve / refuse / normal-close paths).
    drop(pending);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut final_count = usize::MAX;
    while Instant::now() < deadline {
        final_count = server.stats().active_sessions;
        if final_count == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        final_count, 0,
        "active_sessions leaked after all connections closed (stuck at {final_count})"
    );

    server.stop().ok();
}

/// Adversarial: over-cap REFUSALS must never make the `active_sessions`
/// stats gauge overshoot `max_sessions`, even transiently.
///
/// This is the companion invariant to the burst test above, at the STATS
/// layer rather than the accept layer. The refusal path used to reserve
/// with `fetch_add`, bound-check, then release with `fetch_sub` — correct
/// for cap ENFORCEMENT (accepted sessions never exceeded the cap), but
/// between the two atomics the shared counter read `cap + 1`, and
/// `stats().active_sessions` reads that same atomic. A poller sampling
/// during a refusal could observe an impossible value (that transient is
/// exactly what made the burst test above flake on loaded CI runners —
/// its 20 ms-cadence poll occasionally landed inside the window when the
/// accept loop was preempted between the two atomics). The CAS
/// reservation (compare-exchange loop, increment only while below the cap) makes
/// the gauge invariant structural: the counter can never exceed the cap,
/// so this test is deterministic-pass on correct code.
///
/// Detection strategy: saturate a cap-1 server with one parked session,
/// drive a storm of guaranteed-refusals through the accept loop, and
/// spin-poll the gauge (no sleep — a cadenced poll would need scheduler
/// luck to catch a nanosecond-scale window; a spin poll catches it within
/// a few thousand refusals). Against the pre-CAS reserve/release code
/// this trips in under 2 seconds on an idle machine.
///
/// Bounded + hang-proof: the storm is capped by count AND wall-clock, the
/// poller is stopped by flag, and every wait is a deadline poll.
#[test]
fn over_cap_refusals_never_overshoot_active_sessions_gauge() {
    const CAP: usize = 1;
    const STORM_MAX: usize = 5000;
    const STORM_DEADLINE: Duration = Duration::from_secs(4);

    let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    builder.max_sessions(CAP);
    let server = builder.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let addr = server.local_addr().unwrap();

    // Saturate the cap: one parked session (partial request, never
    // terminated) holds the single slot for the whole storm, so every
    // storm connect below is an over-cap refusal.
    let mut parked = TcpStream::connect(addr).unwrap();
    parked
        .write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\n")
        .unwrap();
    // Deadline-poll the precondition (a fixed settle sleep is exactly the
    // kind of load-sensitive timing this file has been burned by).
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.stats().active_sessions < CAP && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        server.stats().active_sessions,
        CAP,
        "precondition: the parked session must saturate the cap before the storm"
    );

    let stop = std::sync::atomic::AtomicBool::new(false);
    let max_seen = std::thread::scope(|scope| {
        let poller = scope.spawn(|| {
            let mut max = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                max = max.max(server.stats().active_sessions);
                // CPU-relax hint (PAUSE) — kinder to the runner's SMT
                // sibling without ceding the timeslice like a sleep would,
                // so the sampling density that catches the window is kept.
                std::hint::spin_loop();
            }
            max
        });

        // Refusal storm: each connect drives the accept loop through the
        // reserve/refuse path once. Count- and wall-clock-bounded so a
        // slow loaded runner exits early rather than overrunning the
        // network group's per-test budget.
        let storm_deadline = Instant::now() + STORM_DEADLINE;
        for _ in 0..STORM_MAX {
            if Instant::now() >= storm_deadline {
                break;
            }
            if let Ok(mut s) = TcpStream::connect(addr) {
                let _ = s.write_all(b"X");
            }
        }
        // Keep observing while the accept loop drains its backlog of
        // queued connects (bounded observation window, not a wait).
        std::thread::sleep(Duration::from_millis(500));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.join().unwrap()
    });

    assert!(
        max_seen <= CAP,
        "active_sessions gauge read {max_seen}, exceeding max_sessions={CAP} — \
         a refusal transiently overshot the counter"
    );

    // Slot hygiene: dropping the parked session must drain the gauge to 0
    // (no leaked reservation on the CAS-reserve or refusal paths).
    drop(parked);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut final_count = usize::MAX;
    while Instant::now() < deadline {
        final_count = server.stats().active_sessions;
        if final_count == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        final_count, 0,
        "active_sessions leaked after the parked session closed (stuck at {final_count})"
    );

    server.stop().ok();
}
