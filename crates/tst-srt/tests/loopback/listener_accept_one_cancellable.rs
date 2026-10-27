//! `SrtListener::accept_one_cancellable` — bind plus a single accept with the
//! accept reachable by a [`CancelSlot`] from another thread. Every managed
//! listener-mode factory (C ABI, Python, JVM) routes through this helper, so
//! the cancel contract is pinned here once. Requires libsrt loopback.

use crate::common::{reserve_port, spawn_connector};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tst_core::cancel::CancelSlot;
use tst_core::transport::{Transport, TransportError};
use tst_srt::SrtListener;
use tst_srt::config::SrtListenerConfig;
use tst_test_helpers::timing::scaled;

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
    let result = SrtListener::accept_one_cancellable(&SrtListenerConfig::default(), &addr, &slot);
    let elapsed = start.elapsed();

    match result {
        Err(TransportError::ExplicitClose) => {}
        Ok(_) => panic!("returned a transport although the slot was cancelled before the call"),
        Err(e) => panic!("expected ExplicitClose; got {e:?}"),
    }
    assert!(
        elapsed < scaled(Duration::from_secs(1)),
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
    let result =
        SrtListener::accept_one_cancellable(&SrtListenerConfig::default(), "127.0.0.1:0", &slot);
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    match result {
        Err(TransportError::ExplicitClose) => {}
        Ok(_) => panic!("returned a transport although no peer ever connected"),
        Err(e) => panic!("expected ExplicitClose; got {e:?}"),
    }
    assert!(
        elapsed < scaled(Duration::from_secs(2)),
        "cancel did not wake the parked accept promptly: {elapsed:?}"
    );
}

/// The happy path still works: a peer that connects is handed back as a live
/// transport, and a successful accept leaves the slot un-latched.
#[test]
fn accepts_a_connecting_peer() {
    require_loopback!();
    let port = reserve_port();

    let slot = Arc::new(CancelSlot::new());
    let (release_tx, connector) = spawn_connector(port, Arc::clone(&slot));

    let result = SrtListener::accept_one_cancellable(
        &SrtListenerConfig::default(),
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
