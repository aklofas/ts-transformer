//! `tst_srt::shells` — the `from_url` family over a real SRT loopback.
//! Each test is the shape of a binding's `open`. Every wait that could
//! park is bounded by [`WATCHDOG`] and FAILS on expiry.
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
use tst_core::transport::EndReason;
use tst_pipeline::binding::{BindingErrorKind, ManagedHandles, OpenCancel};
use tst_pipeline::{
    BackoffStrategy, ManagedRecvTransport, MuxSender, Receiver, ReconnectPolicy, ShellErrorKind,
};
use tst_srt::shells::{
    managed_demux_receiver_from_url, managed_mux_sender_from_url, managed_raw_receiver_from_url,
    managed_raw_sender_from_url, managed_receiver_from_url, managed_receiver_from_url_with_cancel,
    managed_recv_transport_from_url, managed_sender_from_url,
};
use tst_srt::{SrtError, SrtListenerBuilder, SrtTransport, SrtUrl};
use tst_test_helpers::synthetic_nal;
use tst_test_helpers::timing::scaled;

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
    ReconnectPolicy::default()
        .with_max_attempts(Some(0))
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO))
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
fn push_frames_until(sock: tst_srt::SrtSocket, stop: Arc<AtomicBool>) {
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
fn send_until(mut sock: tst_srt::SrtSocket, payload: Vec<u8>, stop: Arc<AtomicBool>) {
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
    let mut builder = SrtListenerBuilder::new();
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
/// `ExplicitClose`; the observed shell kind stays `Closed`, so this test
/// does not depend on that SRT-level kind.
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
    assert_eq!(handles.end_reason.get(), Some(EndReason::Cancelled));

    let _ = release_tx.send(());
    accept.join();
}

/// IPv6 bracketing through the whole composition: a `[::1]` URL opens.
#[test]
fn managed_demux_receiver_from_url_ipv6_literal() {
    if !ipv6_loopback_available() {
        eprintln!("SKIP: IPv6 loopback unavailable on this host");
        return;
    }
    let mut builder = SrtListenerBuilder::new();
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
            match tst_srt::SrtSocketBuilder::new()
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
/// (a Rust caller that needs the FIRST accept cancellable) and the only
/// direct evidence that the first
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
        start.elapsed() < scaled(WATCHDOG),
        "the cancelled open must return promptly, took {:?}",
        start.elapsed()
    );
    // Nothing was left listening: a plain UDP bind on the same port
    // fails while an SRT listener holds it.
    std::net::UdpSocket::bind(("127.0.0.1", port))
        .expect("port still bindable — the cancelled open left no listening socket");
}

/// The caller-mode twin: a slot cancelled before the call refuses the
/// first connect, and nothing is dialled. The listener is live, so a dial
/// would connect; the proof that none happened is that the only connection
/// in the listener's accept queue, once an ordinary caller has connected
/// after the refused open, is that caller's (matched by its local port).
#[test]
fn managed_recv_transport_from_url_cancelled_slot_refuses_the_first_connect_without_dialling() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}")).expect("parse");
    let slot = Arc::new(tst_pipeline::FactoryCancel::new());
    slot.cancel();
    let err = managed_recv_transport_from_url(&url, no_reconnect(), Arc::clone(&slot))
        .err()
        .expect("a pre-cancelled slot must refuse the first connect");
    assert!(
        matches!(
            err,
            SrtError::Transport(tst_pipeline::TransportError::ExplicitClose)
        ),
        "{err:?}"
    );

    let caller = tst_srt::SrtSocketBuilder::new()
        .connect_timeout(scaled(WATCHDOG))
        .connect(format!("127.0.0.1:{port}"))
        .expect("an ordinary connect to the same listener");
    let caller_port = caller.local_addr().expect("local_addr").port();

    // Once that connect has returned, any dial the open made is queued
    // too. libsrt hands queued connections out by socket id, not in
    // arrival order, so drain the queue instead of reading one entry.
    let mut listener = lb.listener;
    let mut queued = Vec::new();
    let mut wait = scaled(WATCHDOG);
    loop {
        match listener.accept_timeout(wait) {
            Ok((_sock, peer)) => queued.push(peer.port()),
            Err(tst_srt::AcceptError::TimedOut) => break,
            Err(e) => panic!("accept: {e}"),
        }
        wait = Duration::ZERO;
    }
    assert_eq!(
        queued,
        vec![caller_port],
        "the listener must have seen only the ordinary caller: \
         a second entry is the cancelled open's dial"
    );
    drop(caller);
}

/// Every re-dial reads the slot too. A caller-mode stream whose slot was
/// cancelled directly (not through the managed cancel handle, which would
/// end the stream on its own) loses its peer: the reconnect's first
/// factory call refuses without dialling and the stream ends as a cancel,
/// instead of spending the budget on re-dials and ending as `Closed`.
#[test]
fn caller_mode_redial_after_a_direct_slot_cancel_ends_as_a_cancel() {
    use tst_core::transport::RecvTransport;
    use tst_pipeline::TransportError;

    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| send_until(sock, null_ts_bundle(), peer_stop));
    accept.wait_ready();

    // `?conntimeo=300` keeps a re-dial into the closed port short if the
    // slot is not honoured, so the test fails on the outcome, not by
    // timing out.
    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?x-recvtimeout=1000&conntimeo=300"
    ))
    .expect("parse");
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO));
    let slot = Arc::new(tst_pipeline::FactoryCancel::new());
    let mut rx = managed_recv_transport_from_url(&url, policy, Arc::clone(&slot))
        .expect("managed_recv_transport_from_url");
    let attempts = rx.attempts_handle();
    let mut buf = vec![0u8; 1500];
    rx.recv_bytes(&mut buf)
        .expect("the stream is live before the cancel");

    slot.cancel();
    // Cut the peer AND its listener (the accept thread owns both).
    stop.store(true, Ordering::SeqCst);
    accept.join();

    let deadline = Instant::now() + WATCHDOG;
    let err = loop {
        assert!(
            Instant::now() < deadline,
            "the stream never ended (attempts={})",
            attempts.load(Ordering::Acquire)
        );
        match rx.recv_bytes(&mut buf) {
            // Bytes still in flight, or `?x-recvtimeout` expiry: not the end.
            Ok(_) | Err(TransportError::Backpressure { .. }) => {}
            Err(e) => break e,
        }
    };
    assert!(matches!(err, TransportError::ExplicitClose), "{err:?}");
    assert_eq!(
        attempts.load(Ordering::Acquire),
        1,
        "one factory call, refused by the slot, ends the reconnect"
    );
}

/// A port nothing will answer a dial on for as long as the returned socket
/// lives: a plain UDP socket never replies to an SRT handshake, so a dial
/// to it runs to its connect timeout.
fn dead_port() -> (std::net::UdpSocket, u16) {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a dead port");
    let port = sock.local_addr().expect("local_addr").port();
    (sock, port)
}

/// A dial budget that outlasts `ParkLatch::assert_parked`'s look on a
/// loaded box (both are scaled).
fn dead_dial_url(port: u16) -> SrtUrl {
    let ms = scaled(Duration::from_secs(3)).as_millis();
    SrtUrl::parse(&format!("srt://127.0.0.1:{port}?conntimeo={ms}")).expect("parse")
}

/// A cancel that arrives while the first connect is in progress does not
/// interrupt it, but a connect that then FAILS reports the cancel, not its
/// own error: the same answer the listener path gives. Nothing listens on
/// the port, so the dial can only time out; the slot is cancelled only
/// once the dial is proven in progress (past the open's pre-dial check).
#[test]
fn caller_mode_dial_that_fails_after_its_slot_was_cancelled_reports_the_cancel() {
    require_loopback!();
    let (_dead, port) = dead_port();
    let url = dead_dial_url(port);
    let slot = Arc::new(tst_pipeline::FactoryCancel::new());
    let latch = crate::common::ParkLatch::default();
    let open = {
        let slot = Arc::clone(&slot);
        latch.park(move || {
            managed_recv_transport_from_url(&url, no_reconnect(), slot)
                .map(drop)
                .map_err(|e| format!("{e:?}"))
        })
    };
    latch.assert_parked();
    slot.cancel();

    let deadline = Instant::now() + scaled(WATCHDOG);
    while !open.is_finished() {
        assert!(Instant::now() < deadline, "the dial never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    let r = open.join().expect("open thread");
    assert_eq!(
        r,
        Err(format!(
            "{:?}",
            SrtError::Transport(tst_pipeline::TransportError::ExplicitClose)
        ))
    );
}

/// The wiring a recv-side getter can get wrong by copying `successes`
/// into `reconnect_attempts`: [`ManagedHandles::attempts`] is the factory-CALL counter, and on a
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
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO));
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

fn unpaced_unlimited() -> ReconnectPolicy {
    ReconnectPolicy::default()
        .with_max_attempts(None)
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO))
}

/// A refused policy never reaches a socket: nothing listens on port 1, so a
/// connect attempt would take the connect timeout.
#[test]
fn managed_sender_from_url_refuses_an_unpaced_unlimited_policy_before_connecting() {
    let url = SrtUrl::parse("srt://127.0.0.1:1").expect("parse");
    let started = Instant::now();
    let r = managed_sender_from_url(&url, unpaced_unlimited(), Default::default());
    assert!(matches!(r, Err(SrtError::Option(_))), "got {:?}", r.err());
    assert!(started.elapsed() < scaled(Duration::from_millis(500)));
}

/// The receive side is refused before it binds or accepts.
#[test]
fn managed_receiver_from_url_refuses_an_unpaced_unlimited_policy_before_binding() {
    let url = SrtUrl::parse("srt://:0?mode=listener").expect("parse");
    let started = Instant::now();
    let r = managed_receiver_from_url(&url, unpaced_unlimited());
    assert!(matches!(r, Err(SrtError::Option(_))), "got {:?}", r.err());
    assert!(started.elapsed() < scaled(Duration::from_millis(500)));
}

// ------------------------------------------------------------------------
// `OpenCancel`: one host object reaching the opens it was given.
// ------------------------------------------------------------------------

/// The outcome of one open: the opener's result as a binding kind
/// (`Opened` is not `Send`) and what `OpenSlot::finish` said.
type OpenOutcome = (Result<(), BindingErrorKind>, bool);

/// A listener-mode plain receive open (the bindings' `Receiver.open`) on a
/// thread, with a slot of `oc`.
fn spawn_listener_open(oc: &OpenCancel, port: u16) -> mpsc::Receiver<OpenOutcome> {
    let oc = oc.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let slot = oc.slot();
        let url = format!("srt://127.0.0.1:{port}?mode=listener");
        let r = tst_srt::opener::open_recv(&url, &slot);
        let finished = slot.finish();
        let _ = tx.send((r.map(drop).map_err(|e| e.kind), finished));
    });
    rx
}

/// Two first accepts parked at once on one object: one `cancel()` ends
/// both as a cancel. Each open is proven parked first: both are inside a
/// tst-srt operation (bind plus accept) for the whole of `park_look`, and
/// neither has returned.
#[test]
fn open_cancel_ends_two_parked_first_accepts() {
    require_loopback!();
    let oc = OpenCancel::new();
    // Both probes are bound before either is released, so the two ports
    // differ.
    let probes = [
        std::net::UdpSocket::bind("127.0.0.1:0").expect("probe"),
        std::net::UdpSocket::bind("127.0.0.1:0").expect("probe"),
    ];
    let ports = probes.map(|p| p.local_addr().expect("local_addr").port());
    let opens = ports.map(|port| spawn_listener_open(&oc, port));

    let deadline = Instant::now() + crate::common::park_deadline();
    let mut held_since = Instant::now();
    while held_since.elapsed() < crate::common::park_look() {
        assert!(Instant::now() < deadline, "the two opens never parked");
        for open in &opens {
            if let Ok(r) = open.try_recv() {
                panic!("an open returned before the cancel: {r:?}");
            }
        }
        if tst_srt::operations_in_flight() < 2 {
            held_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    oc.cancel();
    for open in opens {
        let r = open
            .recv_timeout(scaled(WATCHDOG))
            .expect("the cancel ends the parked open");
        assert_eq!(r, (Err(BindingErrorKind::Closed), true));
    }
}

/// An open given an object that is already cancelled is refused before it
/// binds: the port is free afterwards.
#[test]
fn open_cancel_refuses_a_later_open_without_binding() {
    require_loopback!();
    let oc = OpenCancel::new();
    oc.cancel();
    let port = crate::common::reserve_port();
    let r = spawn_listener_open(&oc, port)
        .recv_timeout(scaled(Duration::from_secs(1)))
        .expect("refused at once");
    assert_eq!(r, (Err(BindingErrorKind::Closed), true));
    std::net::UdpSocket::bind(("127.0.0.1", port))
        .expect("port still bindable: the refused open bound nothing");
}

/// A caller-mode managed open given a slot of a cancelled object is
/// refused without dialling (the queue-drain proof of
/// `managed_recv_transport_from_url_cancelled_slot_refuses_the_first_connect_without_dialling`).
#[test]
fn open_cancel_refuses_a_caller_mode_managed_open_without_dialling() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let oc = OpenCancel::new();
    oc.cancel();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}")).expect("parse");
    let slot = oc.slot();
    let err = managed_receiver_from_url_with_cancel(&url, no_reconnect(), Arc::clone(&slot))
        .expect_err("a cancelled object must refuse the first connect");
    assert!(slot.finish());
    assert!(
        matches!(
            err,
            SrtError::Transport(tst_pipeline::TransportError::ExplicitClose)
        ),
        "{err:?}"
    );

    let caller = tst_srt::SrtSocketBuilder::new()
        .connect_timeout(scaled(WATCHDOG))
        .connect(format!("127.0.0.1:{port}"))
        .expect("an ordinary connect to the same listener");
    let caller_port = caller.local_addr().expect("local_addr").port();
    let mut listener = lb.listener;
    let mut queued = Vec::new();
    let mut wait = scaled(WATCHDOG);
    loop {
        match listener.accept_timeout(wait) {
            Ok((_sock, peer)) => queued.push(peer.port()),
            Err(tst_srt::AcceptError::TimedOut) => break,
            Err(e) => panic!("accept: {e}"),
        }
        wait = Duration::ZERO;
    }
    assert_eq!(
        queued,
        vec![caller_port],
        "a second entry is the refused open's dial"
    );
    drop(caller);
}

/// An open started while another thread cancels always ends, whichever
/// wins: refused before the bind, or woken in its bind or accept. Even
/// rounds cancel as soon as the opener is released (the cancel mostly
/// wins); odd rounds cancel once the opener holds its slot, so the cancel
/// lands somewhere in the open.
#[test]
fn open_cancel_racing_an_open_never_leaves_it_parked() {
    require_loopback!();
    for round in 0..200 {
        let oc = OpenCancel::new();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (registered_tx, registered_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        {
            let oc = oc.clone();
            std::thread::spawn(move || {
                let _ = go_rx.recv();
                let slot = oc.slot();
                let _ = registered_tx.send(());
                let r = tst_srt::opener::open_recv("srt://127.0.0.1:0?mode=listener", &slot);
                let finished = slot.finish();
                let _ = tx.send((r.map(drop).map_err(|e| e.kind), finished));
            });
        }
        go_tx.send(()).expect("opener thread");
        if round % 2 == 1 {
            registered_rx
                .recv_timeout(scaled(WATCHDOG))
                .expect("the opener takes its slot");
        }
        oc.cancel();
        let r = rx
            .recv_timeout(scaled(WATCHDOG))
            .unwrap_or_else(|_| panic!("round {round}: the open was left parked"));
        assert_eq!(r, (Err(BindingErrorKind::Closed), true), "round {round}");
    }
}

/// `cancel()` while a caller-mode managed open (the bindings'
/// `from_url`) dials a dead port: the dial is not interrupted, and when it
/// fails the open reports the cancel. Cancelled only once the dial is
/// proven in progress, past the open's pre-dial check.
#[test]
fn open_cancel_during_a_failing_caller_mode_dial_reports_the_cancel() {
    require_loopback!();
    let (_dead, port) = dead_port();
    let url = dead_dial_url(port);
    let oc = OpenCancel::new();
    let latch = crate::common::ParkLatch::default();
    let open = {
        let oc = oc.clone();
        latch.park(move || {
            let slot = oc.slot();
            let r = managed_receiver_from_url_with_cancel(&url, no_reconnect(), Arc::clone(&slot));
            assert!(slot.finish(), "the cancel came before the open finished");
            r.map(drop).map_err(|e| format!("{e:?}"))
        })
    };
    latch.assert_parked();
    oc.cancel();

    let deadline = Instant::now() + scaled(WATCHDOG);
    while !open.is_finished() {
        assert!(Instant::now() < deadline, "the dial never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        open.join().expect("open thread"),
        Err(format!(
            "{:?}",
            SrtError::Transport(tst_pipeline::TransportError::ExplicitClose)
        ))
    );
}

/// The object is not the opened shell's cancel. A managed caller-mode
/// receiver keeps its slot for every re-dial, but once the open finished
/// whole (`finish()` false) the object no longer holds that slot: a later
/// `cancel()` leaves the reconnects alone, and the stream reconnects to a
/// second peer after the first goes away.
#[test]
fn open_cancel_after_the_open_finished_leaves_a_managed_receiver_reconnecting() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let first_stop = Arc::new(AtomicBool::new(false));
    let second_stop = Arc::new(AtomicBool::new(false));
    let peer = {
        let (first_stop, second_stop) = (Arc::clone(&first_stop), Arc::clone(&second_stop));
        let mut listener = lb.listener;
        std::thread::spawn(move || {
            let (first, _) = listener
                .accept_timeout(scaled(WATCHDOG))
                .expect("first peer");
            send_until(first, null_ts_bundle(), first_stop);
            if let Ok((second, _)) = listener.accept_timeout(scaled(WATCHDOG)) {
                send_until(second, null_ts_bundle(), second_stop);
            }
        })
    };

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?x-recvtimeout=1000&conntimeo=1000"
    ))
    .expect("parse");
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO));
    let oc = OpenCancel::new();
    let slot = oc.slot();
    let (mut rx, handles) = managed_receiver_from_url_with_cancel(&url, policy, Arc::clone(&slot))
        .expect("managed_receiver_from_url_with_cancel");
    assert!(!slot.finish(), "nothing cancelled the object yet");
    rx.next_packet().expect("the stream is live");

    oc.cancel();
    first_stop.store(true, Ordering::SeqCst);

    let deadline = Instant::now() + scaled(WATCHDOG);
    while handles.reconnects.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "the stream never reconnected");
        match rx.next_packet() {
            Ok(_) => {}
            Err(e) if e.kind == ShellErrorKind::Backpressure => {}
            Err(e) => panic!(
                "the stream ended instead of reconnecting (attempts={}): {e:?}",
                handles.attempts.load(Ordering::Acquire)
            ),
        }
    }
    rx.next_packet()
        .expect("bytes flow from the second peer after the reconnect");

    second_stop.store(true, Ordering::SeqCst);
    rx.close();
    peer.join().expect("peer thread");
}

/// A listener that streams null-TS bundles to every caller it accepts,
/// each on its own thread, until that caller goes away, [`cut`](Self::cut)
/// ends the newest connection, or the peer is dropped.
struct ServingPeer {
    port: u16,
    newest: Arc<std::sync::Mutex<Option<Arc<AtomicBool>>>>,
    stop: Arc<AtomicBool>,
    listener_cancel: tst_srt::ResourceCancel,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl ServingPeer {
    fn start() -> Self {
        let lb = crate::common::Loopback::bind();
        let port = lb.port;
        let newest = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let listener_cancel = lb.listener.cancel_handle();
        let accept = {
            let (newest, stop) = (Arc::clone(&newest), Arc::clone(&stop));
            let mut listener = lb.listener;
            std::thread::spawn(move || {
                let mut conns = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept_timeout(Duration::from_millis(50)) {
                        Ok((mut sock, _)) => {
                            let cut = Arc::new(AtomicBool::new(false));
                            *newest.lock().unwrap() = Some(Arc::clone(&cut));
                            let stop = Arc::clone(&stop);
                            conns.push(std::thread::spawn(move || {
                                let bundle = null_ts_bundle();
                                while !stop.load(Ordering::SeqCst) && !cut.load(Ordering::SeqCst) {
                                    if sock.send(&bundle).is_err() {
                                        break;
                                    }
                                    std::thread::sleep(PEER_TICK);
                                }
                            }));
                        }
                        Err(tst_srt::AcceptError::TimedOut) => {}
                        Err(_) => break,
                    }
                }
                for c in conns {
                    let _ = c.join();
                }
            })
        };
        Self {
            port,
            newest,
            stop,
            listener_cancel,
            accept: Some(accept),
        }
    }

    /// End the newest connection (the peer goes away).
    fn cut(&self) {
        if let Some(c) = self.newest.lock().unwrap().as_ref() {
            c.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for ServingPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept at once instead of at its next poll.
        self.listener_cancel.cancel();
        if let Some(a) = self.accept.take() {
            let _ = a.join();
        }
    }
}

/// What one racing open hands back: the shell (or the open's error), what
/// `finish()` said, and the slot the shell keeps.
type RacedOpen = (
    Result<(Receiver<ManagedRecvTransport<SrtTransport>>, ManagedHandles), SrtError>,
    bool,
    Arc<tst_pipeline::FactoryCancel>,
);

/// `cancel()` racing the end of a managed caller-mode open that succeeds:
/// every round is one of the two whole outcomes. `finish()` true: the
/// receiver is dropped (the binding reports CLOSED). `finish()` false: the
/// object never touches the receiver's slot, so the receiver is whole;
/// for a few such rounds the peer goes away and the receiver must
/// reconnect. Never a receiver that was handed out and then refuses to
/// reconnect. Rounds rotate where the cancel lands: at once, once the
/// open holds its slot, the moment the open returned (racing `finish()`),
/// and after `finish()`.
#[test]
fn open_cancel_racing_the_end_of_a_managed_open_gives_one_whole_outcome() {
    require_loopback!();
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::ZERO));
    let mut outcomes = [[0u32; 2]; 4];
    let mut reconnects_checked = 0;
    // Twelve rounds per landing point: each round is a full SRT connect and
    // close, about 0.1 s on the slowest runner.
    for round in 0..48 {
        let mode = round % 4;
        // A fresh listener per round: one listener taking a few hundred
        // connects in a row made a caller's connect time out now and then,
        // with no cancel involved at all.
        let peer = ServingPeer::start();
        let url = SrtUrl::parse(&format!(
            "srt://127.0.0.1:{}?x-recvtimeout=1000&conntimeo=1000&linger=0",
            peer.port
        ))
        .expect("parse");
        let oc = OpenCancel::new();
        let (registered_tx, registered_rx) = mpsc::channel::<()>();
        let (returned_tx, returned_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<RacedOpen>();
        let opener = {
            let (oc, url, policy) = (oc.clone(), url.clone(), policy.clone());
            std::thread::spawn(move || {
                let slot = oc.slot();
                let _ = registered_tx.send(());
                let r = managed_receiver_from_url_with_cancel(&url, policy, Arc::clone(&slot));
                let kept = Arc::clone(&slot);
                let _ = returned_tx.send(());
                let finished = slot.finish();
                let _ = done_tx.send((r, finished, kept));
            })
        };
        let wait = |rx: &mpsc::Receiver<()>, what: &str| {
            rx.recv_timeout(scaled(WATCHDOG))
                .unwrap_or_else(|_| panic!("round {round}: {what}"));
        };
        let mut done = None;
        match mode {
            0 => {}
            1 => wait(&registered_rx, "the open never took its slot"),
            2 => wait(&returned_rx, "the open never returned"),
            _ => {
                done = Some(
                    done_rx
                        .recv_timeout(scaled(WATCHDOG))
                        .unwrap_or_else(|_| panic!("round {round}: the open never finished")),
                );
            }
        }
        oc.cancel();
        let (r, finished, kept) = done.unwrap_or_else(|| {
            done_rx
                .recv_timeout(scaled(WATCHDOG))
                .unwrap_or_else(|_| panic!("round {round}: the open never finished"))
        });
        opener.join().expect("opener thread");
        outcomes[mode][usize::from(finished)] += 1;
        if mode == 3 {
            assert!(!finished, "round {round}: the cancel came after finish()");
        }

        if finished {
            // The binding reports CLOSED and closes what the open returned.
            drop(r);
            continue;
        }
        let (mut rx, handles) =
            r.unwrap_or_else(|e| panic!("round {round}: a whole open failed: {e:?}"));
        assert!(
            !kept.is_cancelled(),
            "round {round}: finish() said whole, then the cancel latched the receiver's slot"
        );
        if reconnects_checked < 3 {
            reconnects_checked += 1;
            rx.next_packet()
                .unwrap_or_else(|e| panic!("round {round}: the stream is not live: {e:?}"));
            peer.cut();
            let deadline = Instant::now() + scaled(WATCHDOG);
            while handles.reconnects.load(Ordering::Acquire) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "round {round}: the receiver never reconnected"
                );
                match rx.next_packet() {
                    Ok(_) => {}
                    Err(e) if e.kind == ShellErrorKind::Backpressure => {}
                    Err(e) => {
                        panic!("round {round}: the receiver ended instead of reconnecting: {e:?}")
                    }
                }
            }
            rx.next_packet()
                .unwrap_or_else(|e| panic!("round {round}: no bytes after the reconnect: {e:?}"));
        }
        rx.close();
    }
    assert_eq!(reconnects_checked, 3, "too few whole rounds: {outcomes:?}");
    eprintln!("outcomes per mode, [finish false, finish true]: {outcomes:?}");
}

/// What the two re-dial log tests share: an encrypted listener, a URL that
/// carries two secrets, and the checks on what was logged.
#[cfg(feature = "mbedtls")]
mod redial {
    use super::*;

    pub const PASSPHRASE: &str = "redial-passphrase-7Qx2-do-not-log";
    pub const STREAM_SECRET: &str = "redial-streamid-token-9Kd4";

    /// A listener that admits only callers holding `PASSPHRASE`.
    pub fn encrypted_listener() -> crate::common::Loopback {
        let mut b = SrtListenerBuilder::new();
        b.passphrase(tst_srt::Passphrase::new(PASSPHRASE).expect("a valid passphrase"));
        b.recv_timeout(scaled(WATCHDOG));
        b.send_timeout(scaled(WATCHDOG));
        crate::common::Loopback::bind_with(b)
    }

    /// The caller's URL: both secrets, and a connect timeout that keeps a
    /// re-dial to a port nobody listens on short.
    pub fn url(port: u16, more: &str) -> SrtUrl {
        SrtUrl::parse(&format!(
            "srt://127.0.0.1:{port}?passphrase={PASSPHRASE}&streamid=publish/cam1/{STREAM_SECRET}&conntimeo=300{more}"
        ))
        .expect("parse")
    }

    pub fn two_redials() -> ReconnectPolicy {
        ReconnectPolicy::default()
            .with_max_attempts(Some(2))
            .with_backoff(BackoffStrategy::Constant(Duration::ZERO))
    }

    /// One line with the connect error per failed re-dial, from `target`.
    pub fn two_cause_lines(lines: &[&str], target: &str) -> Result<(), String> {
        let failed = lines
            .iter()
            .filter(|l| {
                l.contains(target)
                    && l.contains("reconnect attempt failed")
                    && l.contains("connect: ")
            })
            .count();
        if failed == 2 {
            Ok(())
        } else {
            Err(format!("one cause line per failed re-dial, got {failed}"))
        }
    }

    /// Every captured line of the process, not only the calling test's span.
    pub fn assert_no_secret_was_logged() {
        let captured = tracing_test::internal::global_buf().lock().unwrap().clone();
        let captured = String::from_utf8_lossy(&captured);
        for secret in [PASSPHRASE, STREAM_SECRET] {
            assert!(
                !captured.contains(secret),
                "{secret:?} reached the log: {:?}",
                captured.lines().find(|l| l.contains(secret))
            );
        }
    }
}

/// A re-dial that fails says why in the log, and the line is built from the
/// connect error alone: neither the URL's `passphrase` nor its stream id is
/// in anything the run logged (libsrt's own lines included).
#[cfg(feature = "mbedtls")]
#[tracing_test::traced_test]
#[test]
fn a_failed_redial_logs_its_cause_and_no_passphrase() {
    require_loopback!();
    let lb = redial::encrypted_listener();
    let port = lb.port;
    // One accepted connection, one message, then the peer and its listener
    // are gone: every re-dial runs to its connect timeout.
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        sock.recv(&mut buf).expect("the first message")
    });
    accept.wait_ready();

    let (mut tx, _handles, stats) = managed_raw_sender_from_url(
        &redial::url(port, ""),
        redial::two_redials(),
        tst_pipeline::RawSenderConfig::default(),
    )
    .expect("the first connect, encrypted");
    tx.send(b"first").expect("send");
    assert_eq!(accept.join(), b"first".len());

    // Send until the break is seen and the two re-dials have failed.
    let deadline = Instant::now() + scaled(WATCHDOG);
    let gave_up = loop {
        assert!(Instant::now() < deadline, "the sender never gave up");
        match tx.send(b"after the peer left") {
            Ok(()) => std::thread::sleep(PEER_TICK),
            Err(e) => break e,
        }
    };
    assert_eq!(gave_up.kind, ShellErrorKind::TransportBroken, "{gave_up:?}");
    assert_eq!(stats.stats().expect("no poison").reconnect_attempts, 2);
    tx.close();

    logs_assert(|lines: &[&str]| redial::two_cause_lines(lines, "tst_pipeline::reconnect"));
    redial::assert_no_secret_was_logged();
}

/// The receiving twin: a managed receiver's failed re-dial goes through the
/// same connect error, and logs as little of the URL.
#[cfg(feature = "mbedtls")]
#[tracing_test::traced_test]
#[test]
fn a_failed_receive_redial_logs_its_cause_and_no_passphrase() {
    require_loopback!();
    let lb = redial::encrypted_listener();
    let port = lb.port;
    let stop = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stop);
    let accept = lb.spawn_accept(move |sock| send_until(sock, null_ts_bundle(), peer_stop));
    accept.wait_ready();

    let (mut rx, handles) = managed_receiver_from_url(
        &redial::url(port, "&x-recvtimeout=1000"),
        redial::two_redials(),
    )
    .expect("the first connect, encrypted");
    rx.next_packet().expect("the stream is live before the cut");

    // The peer and its listener go away: every re-dial runs to its connect
    // timeout, and the stream ends when both have failed.
    stop.store(true, Ordering::SeqCst);
    accept.join();
    let deadline = Instant::now() + scaled(WATCHDOG);
    loop {
        assert!(Instant::now() < deadline, "the receiver never gave up");
        match rx.next_packet() {
            Ok(_) => {}
            // A receive timeout is not the end of the stream.
            Err(e) if e.kind == ShellErrorKind::Backpressure => {}
            Err(_) => break,
        }
    }
    assert_eq!(handles.attempts.load(Ordering::Acquire), 2);
    rx.close();

    logs_assert(|lines: &[&str]| redial::two_cause_lines(lines, "tst_pipeline::managed_receive"));
    redial::assert_no_secret_was_logged();
}
