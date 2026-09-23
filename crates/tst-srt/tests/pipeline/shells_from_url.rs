//! `tst_srt::shells` — the `from_url` family over a real SRT loopback
//! (Arc 2 WP-A3). Each test is the shape a binding's `open` used to be.
//! Every wait that could park is bounded by [`WATCHDOG`] and FAILS on
//! expiry.
//!
//! # Why every peer streams continuously instead of sending a fixed burst
//!
//! In these tests the shell under test is the CALLER (the shape the
//! bindings use, and the one the project mandates — a managed listener on
//! the main thread blocks it in libsrt's linger). The listener-side peer's
//! `accept()` therefore completes BEFORE the caller's `connect()` returns,
//! and anything the peer sends into that window is gone by the time the
//! caller first reads: measured, a peer that sent two bundles and then
//! held the socket 500 ms produced
//! `Broken { msg: "connection broken", errno_code: Some(2) }` on the
//! caller's first `next_packet()` — not the bundles. So each peer below
//! sends in a loop until the test releases it, which also matches the
//! "post-reconnect bytes must actually flow" pattern the live SRT tests in
//! this repo already use.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, DemuxerConfig};
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec};
use tst_pipeline::{BackoffStrategy, MuxSender, ReconnectPolicy, RecvEndReason, ShellErrorKind};
use tst_srt::shells::{
    managed_demux_receiver_from_url, managed_mux_sender_from_url, managed_raw_receiver_from_url,
    managed_raw_sender_from_url, managed_receiver_from_url, managed_recv_transport_from_url,
    managed_sender_from_url,
};
use tst_srt::{ListenerBuilder, SrtError, SrtTransport, SrtUrl};
use tst_test_helpers::synthetic_nal;

const WATCHDOG: Duration = Duration::from_secs(10);

/// How long a streaming peer keeps going if the test never releases it —
/// a backstop so a failing test cannot wedge the binary at exit with a
/// thread still looping (or parked) inside libsrt.
const PEER_MAX_RUNTIME: Duration = Duration::from_secs(30);

/// Pace between a streaming peer's sends. Fast enough that a reader is
/// never starved, slow enough not to flood the loopback.
const PEER_TICK: Duration = Duration::from_millis(10);

/// No reconnect budget and no backoff: a peer that goes away ends the
/// stream at once instead of re-dialling a port nobody listens on.
fn no_reconnect() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: Some(0),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    }
}

fn video_only_config() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, MuxVideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().expect("build mux config")
}

/// Peer half for the demux tests: a plain `MuxSender` over the accepted
/// socket pushes H.264 AUs at 30 fps PTS spacing until `stop` flips. Send
/// errors are ignored — the receiver under test closes first by design.
fn push_frames_until(sock: tst_srt::Socket, stop: Arc<AtomicBool>) {
    let sender = MuxSender::new(SrtTransport::new(sock), video_only_config()).expect("sender");
    let give_up = Instant::now() + PEER_MAX_RUNTIME;
    let mut i: i64 = 0;
    while !stop.load(Ordering::SeqCst) && Instant::now() < give_up {
        let key = i == 0;
        let nal = synthetic_nal::h264_au(500, key);
        if sender
            .send_video(&nal, Pts90khz::new(i * 3_000), key)
            .is_err()
        {
            break;
        }
        i += 1;
        std::thread::sleep(PEER_TICK);
    }
    sender.close();
}

/// Peer half for the byte-level tests: re-send `payload` until `stop`.
fn send_until(mut sock: tst_srt::Socket, payload: Vec<u8>, stop: Arc<AtomicBool>) {
    let give_up = Instant::now() + PEER_MAX_RUNTIME;
    while !stop.load(Ordering::SeqCst) && Instant::now() < give_up {
        if sock.send(&payload).is_err() {
            break;
        }
        std::thread::sleep(PEER_TICK);
    }
}

/// Seven null-PID (0x1FFF) TS packets in one 1316-byte SRT message — what
/// the shells' syncer needs to lock on (it wants four confirming sync
/// bytes before emitting the first packet).
fn null_ts_bundle() -> Vec<u8> {
    let mut bundle = vec![0xFFu8; 1316];
    for i in 0..7 {
        bundle[i * 188] = 0x47;
        bundle[i * 188 + 1] = 0x1F;
        bundle[i * 188 + 2] = 0xFF;
        bundle[i * 188 + 3] = 0x10;
    }
    bundle
}

fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind("[::1]:0").is_ok()
}

/// Caller-mode managed demux receiver from a URL: the PMT and ≥ 5 samples
/// arrive, and the handles report an untouched, live stream.
#[test]
fn managed_demux_receiver_from_url_reads_events() {
    require_loopback!();
    let mut builder = ListenerBuilder::new();
    builder.recv_latency(Duration::from_millis(120));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| push_frames_until(sock, peer_stop));
    accept.wait_ready();

    // `x-recvtimeout` is the backstop: a silent peer makes the recv fail
    // with a Backpressure-kind error after 5 s instead of parking forever.
    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?latency=120&x-recvtimeout=5000"
    ))
    .expect("parse");
    let (mut rx, handles) =
        managed_demux_receiver_from_url(&url, no_reconnect(), DemuxerConfig::default())
            .expect("managed_demux_receiver_from_url");

    let mut got_pmap = false;
    let mut samples = 0usize;
    let deadline = Instant::now() + WATCHDOG;
    while samples < 5 {
        if Instant::now() >= deadline {
            stop.store(true, Ordering::SeqCst); // release the peer before failing
            panic!("fewer than 5 samples within {WATCHDOG:?} (pmap={got_pmap}, samples={samples})");
        }
        match rx.recv_event().expect("recv_event") {
            Some(DemuxEvent::ProgramMap(_)) => got_pmap = true,
            Some(DemuxEvent::Sample { .. }) => samples += 1,
            Some(_) => {}
            None => {
                stop.store(true, Ordering::SeqCst);
                panic!("stream ended before 5 samples arrived");
            }
        }
    }
    assert!(got_pmap, "the PMT precedes the samples");
    assert_eq!(
        handles.attempts.load(Ordering::Acquire),
        0,
        "no factory call on a healthy stream"
    );
    assert_eq!(handles.reconnects.load(Ordering::Acquire), 0);
    assert!(!handles.reconnecting.load(Ordering::Acquire));
    assert!(
        handles.end_reason.get().is_none(),
        "the stream is still live"
    );

    stop.store(true, Ordering::SeqCst);
    rx.close();
    accept.join();
}

/// `ManagedHandles::cancel` fired from another thread ends a receive
/// parked in libsrt with no data in sight, the shell reports kind
/// `Closed`, and the end reason reads `Cancelled`.
///
/// Inside the managed loop the SRT-level variant the wake produces is
/// `ExplicitClose` (WP-C2); the observed shell kind stays `Closed`, so this
/// test did not move when that kind changed.
#[test]
fn cancel_handle_ends_a_parked_recv_and_records_cancelled() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    // The peer sends nothing and holds its socket until released.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let accept = lb.spawn_accept(move |sock| {
        let _ = release_rx.recv_timeout(PEER_MAX_RUNTIME);
        drop(sock);
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}")).expect("parse");
    let (mut rx, handles) =
        managed_demux_receiver_from_url(&url, no_reconnect(), DemuxerConfig::default())
            .expect("managed_demux_receiver_from_url");

    // The parked recv runs on its own thread: a cancel that fails to wake
    // it surfaces as a FAILED test at the watchdog, never a hung one. The
    // reader latches `entered` immediately before the call, so the cancel
    // lands on a recv that has actually started — a fixed sleep would be
    // setup dressed up as proof.
    let entered = Arc::new(AtomicBool::new(false));
    let reader_entered = Arc::clone(&entered);
    let reader = std::thread::spawn(move || {
        reader_entered.store(true, Ordering::SeqCst);
        let outcome = rx.recv_event();
        rx.close();
        outcome
    });
    let entry_deadline = Instant::now() + WATCHDOG;
    while !entered.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < entry_deadline,
            "the reader thread never reached recv_event"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    handles.cancel.cancel();

    let deadline = Instant::now() + WATCHDOG;
    while !reader.is_finished() {
        if Instant::now() > deadline {
            let _ = release_tx.send(()); // free the peer so the binary can exit
            panic!("recv_event still parked {WATCHDOG:?} after ManagedHandles::cancel fired");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let err = match reader.join().expect("reader thread") {
        Err(e) => e,
        Ok(ev) => panic!("expected the cancelled recv to fail with kind Closed; got {ev:?}"),
    };
    assert_eq!(err.kind, ShellErrorKind::Closed, "{err:?}");
    assert_eq!(handles.end_reason.get(), Some(RecvEndReason::Cancelled));

    let _ = release_tx.send(());
    accept.join();
}

/// The #188 class through the whole composition: a `[::1]` URL opens.
#[test]
fn managed_demux_receiver_from_url_ipv6_literal() {
    if !ipv6_loopback_available() {
        eprintln!("SKIP: IPv6 loopback unavailable on this host");
        return;
    }
    let mut builder = ListenerBuilder::new();
    builder.recv_latency(Duration::from_millis(120));
    let lb = crate::common::Loopback::bind_at(builder, "[::1]:0");
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| push_frames_until(sock, peer_stop));
    accept.wait_ready();

    let url = SrtUrl::parse(&format!(
        "srt://[::1]:{port}?latency=120&x-recvtimeout=5000"
    ))
    .expect("parse");
    let (mut rx, _handles) =
        managed_demux_receiver_from_url(&url, no_reconnect(), DemuxerConfig::default())
            .expect("v6 open — parse strips the brackets, the open path must re-add them");

    let deadline = Instant::now() + WATCHDOG;
    loop {
        if Instant::now() >= deadline {
            stop.store(true, Ordering::SeqCst);
            panic!("no sample within {WATCHDOG:?}");
        }
        match rx.recv_event().expect("recv_event") {
            Some(DemuxEvent::Sample { .. }) => break,
            Some(_) => {}
            None => {
                stop.store(true, Ordering::SeqCst);
                panic!("stream ended before the first sample");
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
    rx.close();
    accept.join();
}

/// The plain TS-bytes receiver from a URL yields aligned packets; its end
/// reason is the documented never-set handle.
#[test]
fn managed_receiver_from_url_yields_aligned_packets() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| send_until(sock, null_ts_bundle(), peer_stop));
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}?x-recvtimeout=5000")).expect("parse");
    let (mut rx, handles) =
        managed_receiver_from_url(&url, no_reconnect()).expect("managed_receiver_from_url");
    let pkt = rx.next_packet().expect("first aligned packet");
    assert_eq!(pkt[0], 0x47);
    assert!(
        handles.end_reason.get().is_none(),
        "the plain Receiver records no end reason — fresh handle, never set"
    );
    assert_eq!(handles.attempts.load(Ordering::Acquire), 0);

    stop.store(true, Ordering::SeqCst);
    rx.close();
    accept.join();
}

/// The raw-bytes receiver twin (C's `tst_managed_raw_receiver`): one
/// message in, the same bytes out, never-set end reason.
#[test]
fn managed_raw_receiver_from_url_yields_the_peer_bytes() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let payload = b"raw bytes through a managed raw receiver".to_vec();
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let peer_payload = payload.clone();
    let accept = lb.spawn_accept(move |sock| send_until(sock, peer_payload, peer_stop));
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}?x-recvtimeout=5000")).expect("parse");
    let (mut rx, handles) =
        managed_raw_receiver_from_url(&url, no_reconnect()).expect("managed_raw_receiver_from_url");
    // `RawReceiver::recv_one(&mut self) -> Result<Vec<u8>, RawReceiverError>`
    // (`crates/tst-pipeline/src/raw_receiver.rs:234`): one transport message.
    let msg = rx.recv_one().expect("recv_one");
    assert_eq!(msg, payload);
    assert!(
        handles.end_reason.get().is_none(),
        "RawReceiver records no end reason"
    );

    stop.store(true, Ordering::SeqCst);
    rx.close();
    accept.join();
}

/// `?mode=listener` through the receiver family: the open BINDS and waits
/// for a peer to dial in, it does not dial out. Nothing else in this file
/// exercises the listener arm of `shells::open`, and without this a
/// mutation of that arm to `url.connect()` passes every other test here.
///
/// The discriminator is the direction: the peer is the CALLER, so a
/// mutated family that dialled `127.0.0.1:{port}` would find nothing
/// listening and fail fast with a `ConnectError` — and the open would
/// never have bound the port the peer is retrying against, so this test
/// fails on evidence rather than by timing out.
#[test]
fn managed_receiver_from_url_in_listener_mode_accepts_a_dialling_peer() {
    require_loopback!();
    // A port the peer can name before the listener under test binds it.
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);

    // Caller-side peer: retry until the family's first accept has bound,
    // then stream until released (same reason as every other peer here).
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let sock = loop {
            match tst_srt::SocketBuilder::new()
                .connect_timeout(Duration::from_millis(500))
                .connect(format!("127.0.0.1:{port}"))
            {
                Ok(s) => break s,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "the family never bound {port} for a caller to dial: {e:?}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        send_until(sock, null_ts_bundle(), peer_stop);
    });

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?mode=listener&x-recvtimeout=5000"
    ))
    .expect("parse");
    let (mut rx, handles) = managed_receiver_from_url(&url, no_reconnect())
        .expect("listener-mode open accepts the dialling peer");
    let pkt = rx.next_packet().expect("first aligned packet");
    assert_eq!(pkt[0], 0x47);
    assert_eq!(
        handles.attempts.load(Ordering::Relaxed),
        0,
        "the FIRST accept is the initial open, not a factory call"
    );

    stop.store(true, Ordering::SeqCst);
    rx.close();
    peer.join().expect("peer thread");
}

/// The listener family's first accept runs through the slot the CALLER
/// passed, not one the function made up: a slot cancelled before the call
/// makes `managed_recv_transport_from_url` return without binding
/// anything. This is what `managed_recv_transport_from_url` exists for
/// (DEBT-16 for Rust callers) and the only direct evidence that the first
/// accept and the re-accepts share one slot. No peer, no network.
#[test]
fn managed_recv_transport_from_url_first_accept_runs_through_the_caller_slot() {
    require_loopback!();
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}?mode=listener")).expect("parse");
    let slot = Arc::new(tst_pipeline::FactoryCancel::new());
    slot.cancel();

    let start = Instant::now();
    let err = managed_recv_transport_from_url(&url, no_reconnect(), Arc::clone(&slot))
        .err()
        .expect("a pre-cancelled slot must abort the first accept");
    assert!(
        matches!(
            err,
            SrtError::Transport(tst_pipeline::TransportError::ExplicitClose)
        ),
        "{err:?}"
    );
    assert!(
        start.elapsed() < WATCHDOG,
        "the cancelled open must return promptly, took {:?}",
        start.elapsed()
    );
    // Nothing was left listening: a plain UDP bind on the same port
    // fails while an SRT listener holds it.
    std::net::UdpSocket::bind(("127.0.0.1", port))
        .expect("port still bindable — the cancelled open left no listening socket");
}

/// The wiring the C recv-side getter used to get wrong (it copied
/// `successes` into `reconnect_attempts` with an apology):
/// [`ManagedHandles::attempts`] is the factory-CALL counter, and on a
/// stream whose peer is gone for good the two must diverge — attempts
/// climb to the budget while reconnects stay 0. This is the only test that
/// tells the two handles apart through the whole `from_url` composition;
/// everywhere else a healthy stream reads 0 for both.
///
/// `?conntimeo=300` keeps the dead re-dials fast: the URL wins over the
/// 15 s sender-preset connect timeout `connect()` merges underneath it.
#[test]
fn recv_handles_attempts_count_failed_factory_calls_not_successes() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| send_until(sock, null_ts_bundle(), peer_stop));
    accept.wait_ready();

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?x-recvtimeout=1000&conntimeo=300"
    ))
    .expect("parse");
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    };
    let (mut rx, handles) =
        managed_receiver_from_url(&url, policy).expect("managed_receiver_from_url");
    rx.next_packet().expect("the stream is live before the cut");

    // Cut the peer AND its listener: the closure returning drops the
    // accepted socket, and `spawn_accept` already moved the listener into
    // that thread — so every re-dial from here on has nothing to reach.
    stop.store(true, Ordering::SeqCst);
    accept.join();

    let deadline = Instant::now() + WATCHDOG;
    loop {
        if Instant::now() >= deadline {
            panic!(
                "the stream never ended (attempts={}, reconnects={})",
                handles.attempts.load(Ordering::Acquire),
                handles.reconnects.load(Ordering::Acquire)
            );
        }
        match rx.next_packet() {
            Ok(_) => {}
            // `?x-recvtimeout` expiry is NOT the end of the stream:
            // `ManagedRecvTransport` propagates `Backpressure` unchanged
            // and `Receiver::next_packet` passes it through, so breaking
            // on it would end the loop before the reconnect budget was
            // spent and make the counter assertions vacuous. The deadline
            // above stays the failing watchdog.
            Err(e) if e.kind == ShellErrorKind::Backpressure => continue,
            Err(_) => break,
        }
    }

    assert_eq!(
        handles.attempts.load(Ordering::Acquire),
        3,
        "every factory call is counted — the budget was spent"
    );
    assert_eq!(
        handles.reconnects.load(Ordering::Acquire),
        0,
        "not one of those calls produced a transport"
    );
    rx.close();
}

/// A managed mux sender from a URL pushes a bundle the peer receives;
/// the sender's handles report no reconnect activity and a never-set end
/// reason.
#[test]
fn managed_mux_sender_from_url_sends_bundles() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}?latency=120")).expect("parse");
    let (tx, handles, stats) =
        managed_mux_sender_from_url(&url, ReconnectPolicy::default(), video_only_config())
            .expect("managed_mux_sender_from_url");
    // A 5000-byte AU spans ≥ 27 TS packets, so at least one full
    // 7-packet bundle leaves the muxer on this single push.
    let nal = synthetic_nal::h264_au(5000, true);
    tx.send_video(&nal, Pts90khz::new(0), true)
        .expect("send_video");

    let received = accept.join();
    assert_eq!(received.len(), 1316, "one 7×188 bundle");
    assert_eq!(received[0], 0x47);
    assert_eq!(handles.attempts.load(Ordering::Acquire), 0);
    assert_eq!(handles.reconnects.load(Ordering::Acquire), 0);
    assert!(
        !handles.reconnecting.load(Ordering::Acquire),
        "Blocking mode: never set"
    );
    assert!(
        handles.end_reason.get().is_none(),
        "senders never record an end reason"
    );
    // The stats handle is the live gap/reconnect telemetry the bindings'
    // reconnect_stats() read; a healthy stream shows zeros.
    let s = stats.stats().expect("no poison");
    assert_eq!(
        (s.reconnect_attempts, s.reconnect_successes, s.gap_len),
        (0, 0, 0)
    );
    tx.close();
}

/// The raw-bytes sender twin (C's `tst_managed_raw_sender`).
#[test]
fn managed_raw_sender_from_url_sends_the_bytes() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}")).expect("parse");
    let (mut tx, handles, stats) = managed_raw_sender_from_url(
        &url,
        ReconnectPolicy::default(),
        tst_pipeline::RawSenderConfig::default(),
    )
    .expect("managed_raw_sender_from_url");
    tx.send(b"raw bytes through a managed raw sender")
        .expect("send");

    assert_eq!(accept.join(), b"raw bytes through a managed raw sender");
    assert!(handles.end_reason.get().is_none());
    assert_eq!(stats.stats().expect("no poison").reconnect_attempts, 0);
    tx.close();
}

/// Senders are caller-only: all three refuse `?mode=listener` with
/// `SrtError::Option`, which is all this test asserts. (They also refuse
/// it before touching the network — that is what the guard is FOR, and
/// dropping the guard makes this test take 15 s instead of 0 s on a
/// dead port — but the name now claims only the assertion.)
#[test]
fn sender_from_url_refuses_listener_mode_with_an_option_error() {
    let url = SrtUrl::parse("srt://127.0.0.1:9000?mode=listener").expect("parse");

    let err = managed_sender_from_url(
        &url,
        ReconnectPolicy::default(),
        tst_pipeline::SenderConfig::default(),
    )
    .err()
    .expect("listener mode must be refused");
    assert!(matches!(err, SrtError::Option(_)), "{err:?}");
    assert!(err.to_string().contains("callers only"), "{err}");

    let err = managed_mux_sender_from_url(&url, ReconnectPolicy::default(), video_only_config())
        .err()
        .expect("listener mode must be refused");
    assert!(matches!(err, SrtError::Option(_)), "{err:?}");

    let err = managed_raw_sender_from_url(
        &url,
        ReconnectPolicy::default(),
        tst_pipeline::RawSenderConfig::default(),
    )
    .err()
    .expect("listener mode must be refused");
    assert!(matches!(err, SrtError::Option(_)), "{err:?}");
}
