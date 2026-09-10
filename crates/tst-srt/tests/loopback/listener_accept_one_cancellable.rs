//! `Listener::accept_one_cancellable` — bind plus a single accept with the
//! accept reachable by a [`CancelSlot`] from another thread. Every managed
//! listener-mode factory (C ABI, Python, JVM) routes through this helper, so
//! the cancel contract is pinned here once. Requires libsrt loopback.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};
use tst_srt::config::ListenerConfig;
use tst_srt::{Listener, SocketBuilder};

/// Reserve an ephemeral UDP port and release it again. Unlike
/// `127.0.0.1:0`, the resulting port is knowable by a connecting peer
/// *before* the listener under test has bound it — which is what a caller
/// of `accept_one_cancellable` needs, since the helper owns its listener
/// and never exposes a `local_addr`.
fn reserve_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

/// A slot cancelled before the call must skip the bind entirely: a cancelled
/// caller gets no socket, and nothing is left listening on the port.
#[test]
fn cancelled_before_bind_returns_explicit_close_and_leaves_the_port_free() {
    require_loopback!();
    let port = reserve_port();
    let addr = format!("127.0.0.1:{port}");
    let slot = CancelSlot::new();
    slot.cancel();

    let start = Instant::now();
    let result = Listener::accept_one_cancellable(&ListenerConfig::default(), &addr, &slot);
    let elapsed = start.elapsed();

    match result {
        Err(TransportError::ExplicitClose) => {}
        Ok(_) => panic!("returned a transport although the slot was cancelled before the call"),
        Err(e) => panic!("expected ExplicitClose; got {e:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(1),
        "an already-cancelled slot must return at once: {elapsed:?}"
    );

    // Nothing is listening on the port: a plain UDP bind (no SO_REUSEADDR)
    // fails with EADDRINUSE while an SRT listener holds the same address.
    std::net::UdpSocket::bind(("127.0.0.1", port))
        .expect("port still bindable — the cancelled call left no listening socket");
}

/// The whole point of the helper: a cancel fired while the accept is parked
/// wakes it, and the wake is reported as a caller-initiated close rather
/// than a transport fault.
#[test]
fn cancel_during_accept_returns_explicit_close_promptly() {
    require_loopback!();
    let slot = Arc::new(CancelSlot::new());
    let canceller = {
        let slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            slot.cancel();
        })
    };

    let start = Instant::now();
    let result = Listener::accept_one_cancellable(&ListenerConfig::default(), "127.0.0.1:0", &slot);
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    match result {
        Err(TransportError::ExplicitClose) => {}
        Ok(_) => panic!("returned a transport although no peer ever connected"),
        Err(e) => panic!("expected ExplicitClose; got {e:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel did not wake the parked accept promptly: {elapsed:?}"
    );
}

/// The happy path still works: a peer that connects is handed back as a live
/// transport, and a successful accept leaves the slot un-latched.
#[test]
fn accepts_a_connecting_peer() {
    require_loopback!();
    let port = reserve_port();

    // The connector retries because its first attempt can run before the
    // listener under test has bound. The connected socket is held past the
    // accept's return through the release channel: closing it mid-flight
    // lets libsrt's GC reap the listener-side accepted socket before
    // srt_accept resolves it (same hazard as listener_accept_timeout.rs).
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let connector = std::thread::spawn(move || {
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
                    assert!(Instant::now() < deadline, "connect never succeeded: {e:?}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });

    let slot = CancelSlot::new();
    let result = Listener::accept_one_cancellable(
        &ListenerConfig::default(),
        &format!("127.0.0.1:{port}"),
        &slot,
    );
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
