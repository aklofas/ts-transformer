//! `SrtUrl::connect` / `SrtUrl::accept_one` — the one open path every
//! binding composes through (Arc 2 WP-A3, ARCH-01). Requires libsrt
//! loopback. The accept tests park a thread on purpose, so every wait is
//! bounded by [`WATCHDOG`] and FAILS on expiry instead of hanging.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};
use tst_srt::{ListenerBuilder, SocketBuilder, SrtError, SrtUrl};

/// Upper bound on anything that must NOT park forever.
const WATCHDOG: Duration = Duration::from_secs(10);

/// Reserve an ephemeral UDP port and release it again, so a listener-mode
/// URL can name its port BEFORE the listener under test binds it (same
/// helper as `listener_accept_one_cancellable.rs`).
fn reserve_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind("[::1]:0").is_ok()
}

/// Caller mode: the overlay is applied, the sender defaults are merged,
/// the transport is connected and alive, bytes arrive at the peer.
#[test]
fn connect_round_trips_over_loopback() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?latency=120&x-sendtimeout=5000"
    ))
    .expect("parse");
    let mut t = url.connect().expect("SrtUrl::connect");
    assert!(t.is_alive(), "a freshly connected transport is alive");
    t.send_bytes(b"hello via SrtUrl::connect")
        .expect("send_bytes");

    assert_eq!(accept.join(), b"hello via SrtUrl::connect");
    t.close();
}

/// `connect_recv` dials the same way minus the sender preset — the open
/// the plain Python/JVM receivers do today. That the preset is genuinely
/// absent is asserted on the config in `url.rs`'s unit tests (a live
/// socket reports back neither the linger nor the role); this pins that
/// the overlay-only config still produces a working connection.
#[test]
fn connect_recv_round_trips_over_loopback() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!(
        "srt://127.0.0.1:{port}?latency=120&x-sendtimeout=5000"
    ))
    .expect("parse");
    let mut t = url.connect_recv().expect("SrtUrl::connect_recv");
    assert!(t.is_alive(), "a freshly connected transport is alive");
    t.send_bytes(b"hello via connect_recv").expect("send_bytes");

    assert_eq!(accept.join(), b"hello via connect_recv");
    t.close();
}

/// The #188 class, end-to-end half: `parse` strips the brackets
/// (`host == "::1"`), so the open path must put them back — an IPv6
/// `srt://` URL has to connect and carry bytes, which is what PR #188
/// fixed in the bindings' private joins.
///
/// The bracketing *itself* is pinned by
/// `tst_srt::addr::tests::join_host_port_brackets_bare_ipv6_only`, not
/// here: mutating `join_host_port` to a plain `format!("{host}:{port}")`
/// leaves this test GREEN, because `ToSocketAddrs for str` splits at the
/// LAST `':'` and hands the rest to `getaddrinfo`, so `::1:PORT` still
/// resolves (measured on glibc). The unbracketed join is still wrong: the
/// v6 wildcard `::` joins to `::PORT`, whose split leaves the host `":"`
/// and fails to resolve, and the form is out of contract everywhere.
#[test]
fn connect_ipv6_literal_round_trips() {
    if !ipv6_loopback_available() {
        eprintln!("SKIP: IPv6 loopback unavailable on this host");
        return;
    }
    let mut builder = ListenerBuilder::new();
    builder.recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_at(builder, "[::1]:0");
    let port = lb.port;
    let accept = lb.spawn_accept(|mut sock| {
        let mut buf = [0u8; 1500];
        let n = sock.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://[::1]:{port}")).expect("parse");
    assert_eq!(url.host, "::1", "parse hands the host back bracket-less");
    let mut t = url
        .connect()
        .expect("v6 caller connect through SrtUrl::connect (the #188 bracket class)");
    t.send_bytes(b"hello over v6").expect("send_bytes");

    assert_eq!(accept.join(), b"hello over v6");
    t.close();
}

/// The whole point of taking the slot: a cancel fired from another thread
/// while the accept is parked wakes it, and the wake is reported as a
/// caller-initiated close (`ExplicitClose`), not a transport fault.
///
/// The accept runs on its own thread so a cancel that fails to wake it
/// surfaces as a FAILED test at the watchdog, never as a hung one. (A
/// thread left parked in `srt_accept` after the failure stalls process
/// exit — nextest's per-test timeout reaps it; the failure is already on
/// record by then.)
///
/// WP-C2 note: the outcome asserted here comes from
/// `Listener::accept_one_cancellable`, which already reports
/// `ExplicitClose` on cancel. C2 changes `SrtTransport::recv_bytes` /
/// `send_bytes`, not the accept path — this test does not move.
#[test]
fn accept_one_cancelled_from_another_thread_returns_explicit_close() {
    require_loopback!();
    let port = reserve_port();
    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}?mode=listener")).expect("parse");
    let slot = Arc::new(CancelSlot::new());

    let acceptor = {
        let slot = Arc::clone(&slot);
        std::thread::spawn(move || url.accept_one(&slot))
    };
    // Let the accept park before firing: a cancel that lands before the
    // bind is the other (already pinned) branch of the helper.
    std::thread::sleep(Duration::from_millis(200));
    slot.cancel();

    let deadline = Instant::now() + WATCHDOG;
    while !acceptor.is_finished() {
        assert!(
            Instant::now() < deadline,
            "accept_one still parked {WATCHDOG:?} after the slot was cancelled"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    match acceptor.join().expect("acceptor thread") {
        Err(SrtError::Transport(TransportError::ExplicitClose)) => {}
        Ok(_) => panic!("returned a transport although no peer ever connected"),
        Err(e) => panic!("expected SrtError::Transport(ExplicitClose); got {e:?}"),
    }
    assert!(slot.is_cancelled(), "the slot stays latched after the wake");
}

/// Spawn the peer the two happy-path accept tests need: retry `connect`
/// until the listener under test has bound (its first attempt can run
/// before the bind), then hold the connected socket open until the caller
/// sends on the returned channel — closing it mid-flight lets libsrt's GC
/// reap the listener-side accepted socket before `srt_accept` resolves it
/// (the PR #231 prune class).
///
/// `slot` is the accept's cancel slot, and giving up fires it BEFORE the
/// panic: `accept_one` runs on the test's own thread with nothing else
/// able to reach it, so a connector that dies while the accept is parked
/// would leave the test hanging in `srt_accept` instead of failing (and a
/// thread left parked there stalls process exit — the `atexit(srt_cleanup)`
/// class).
fn spawn_connector(
    port: u16,
    slot: Arc<CancelSlot>,
) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let attempt = SocketBuilder::new()
                .connect_timeout(Duration::from_millis(500))
                .connect(format!("127.0.0.1:{port}"));
            match attempt {
                Ok(socket) => {
                    let _ = release_rx.recv();
                    drop(socket);
                    return;
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        // Release the parked accept first, then report.
                        slot.cancel();
                        panic!("connect never succeeded: {e:?}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });
    (release_tx, handle)
}

/// Happy path: a peer that connects is handed back as a live transport,
/// and a successful accept leaves the slot un-latched. The overlay is
/// applied to the listener side (`latency` is a listener-side key).
#[test]
fn accept_one_hands_back_a_connecting_peer() {
    require_loopback!();
    let port = reserve_port();

    let slot = Arc::new(CancelSlot::new());
    let (release_tx, connector) = spawn_connector(port, Arc::clone(&slot));

    let url =
        SrtUrl::parse(&format!("srt://127.0.0.1:{port}?mode=listener&latency=120")).expect("parse");
    let result = url.accept_one(&slot);
    let _ = release_tx.send(());
    let connector_result = connector.join();

    let transport = result.expect("accept a connecting peer");
    connector_result.expect("connector thread");
    assert!(
        transport.is_alive(),
        "the accepted transport should be alive"
    );
    assert!(
        !slot.is_cancelled(),
        "a successful accept must not latch the slot"
    );
}

/// The empty-host branch: `srt://:PORT?mode=listener` is a legal listener
/// URL (`parse` only demands a host in caller mode), and `accept_one`
/// renders it as the wildcard `0.0.0.0:PORT` — the bind address the
/// bindings' `listen_srt` produced. Without that substitution the join
/// yields `":PORT"`, which does not resolve, and the accept comes back
/// `Broken { msg: "bind: …" }` instead of a peer.
///
/// (Added beyond the WP-A3 brief's test list: no other test exercises
/// this branch, and it is a documented contract of the method.)
#[test]
fn accept_one_with_empty_host_binds_the_wildcard() {
    require_loopback!();
    let port = reserve_port();

    let slot = Arc::new(CancelSlot::new());
    let (release_tx, connector) = spawn_connector(port, Arc::clone(&slot));

    let url = SrtUrl::parse(&format!("srt://:{port}?mode=listener")).expect("parse");
    assert!(
        url.host.is_empty(),
        "the wildcard form parses to an empty host"
    );
    let result = url.accept_one(&slot);
    let _ = release_tx.send(());
    let connector_result = connector.join();

    let transport = result.expect("a wildcard-bound listener accepts a 127.0.0.1 peer");
    connector_result.expect("connector thread");
    assert!(
        transport.is_alive(),
        "the accepted transport should be alive"
    );
}

/// The plain-shell cancel accessor: non-`Option`, obtained before the
/// transport moves into a shell, wakes a parked recv from another thread,
/// and still answers (as cancelled) after `close()`.
///
/// WP-C2 note: the error the woken recv returns is `Broken` today and
/// becomes `ExplicitClose` in C2 — this test asserts only "returned with
/// an error within the watchdog", so it does not move.
#[test]
fn srt_cancel_handle_wakes_a_parked_recv_and_survives_close() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let accept = lb.spawn_accept(move |sock| {
        let _ = release_rx.recv(); // send nothing; hold the socket open
        drop(sock);
    });
    accept.wait_ready();

    let url = SrtUrl::parse(&format!("srt://127.0.0.1:{port}")).expect("parse");
    let mut t = url.connect().expect("connect");
    let cancel = t.srt_cancel_handle();
    assert!(!cancel.is_cancelled());

    // The reader latches `entered` immediately before the call, so the
    // cancel lands on a recv that has actually started — a fixed sleep
    // would be setup dressed up as proof.
    let entered = Arc::new(AtomicBool::new(false));
    let reader_entered = Arc::clone(&entered);
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        reader_entered.store(true, Ordering::SeqCst);
        let outcome = tst_core::transport::RecvTransport::recv_bytes(&mut t, &mut buf);
        t.close();
        outcome
    });
    let entry_deadline = Instant::now() + WATCHDOG;
    while !entered.load(Ordering::SeqCst) {
        if Instant::now() > entry_deadline {
            let _ = release_tx.send(());
            panic!("the reader thread never reached recv_bytes");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    cancel.cancel();

    let deadline = Instant::now() + WATCHDOG;
    while !reader.is_finished() {
        if Instant::now() > deadline {
            let _ = release_tx.send(());
            panic!("recv_bytes still parked {WATCHDOG:?} after srt_cancel_handle().cancel()");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let outcome = reader.join().expect("reader thread");
    assert!(
        outcome.is_err(),
        "a cancelled recv must fail, got {outcome:?}"
    );
    assert!(
        cancel.is_cancelled(),
        "the handle reads cancelled after close()"
    );
    let _ = release_tx.send(());
    accept.join();
}
