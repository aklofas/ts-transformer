//! Tests for `Listener::accept_timeout`. Requires libsrt loopback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tst_srt::{AcceptError, SrtListenerBuilder};

#[test]
fn accept_timeout_returns_timed_out_when_no_connection() {
    require_loopback!();
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:0").expect("bind");

    let start = std::time::Instant::now();
    let result = listener.accept_timeout(Duration::from_millis(200));
    let elapsed = start.elapsed();

    match result {
        Err(AcceptError::TimedOut) => {}
        Ok(_) => panic!("expected TimedOut; got Ok"),
        Err(e) => panic!("expected TimedOut; got Err({e:?})"),
    }
    // Must have actually waited at least most of the timeout.
    assert!(elapsed >= Duration::from_millis(150), "elapsed={elapsed:?}");
    // But not absurdly long. The lower bound is the real assertion; this
    // one only guards against the timeout being ignored outright, so it
    // is deliberately loose: a loaded macOS runner took 1.17 s for this
    // 200 ms wait (2026-10-06), and the old `< 1 s` bound read that as
    // a failure.
    assert!(elapsed < Duration::from_secs(5), "elapsed={elapsed:?}");
}

#[test]
fn accept_timeout_succeeds_when_peer_connects() {
    require_loopback!();
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    // Spawn a connector in another thread with a short delay so the
    // accept_timeout call is already waiting when the connection arrives.
    //
    // The connector socket has to stay alive *past* accept_timeout's
    // return — if it closes mid-flight, libsrt's GC can reap the
    // listener-side accepted socket before srt_accept calls
    // locateSocket() on the dequeued ID, and srt_accept then throws
    // ECONNSETUP/MN_CLOSED ("socket closed during operation"). The
    // (release_tx, release_rx) channel pins the connector socket
    // until the main thread has finished accepting.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let ready = Arc::new(AtomicBool::new(false));
    let r = ready.clone();
    let connector = std::thread::spawn(move || {
        crate::common::wait_for_ready(&r);
        let socket = tst_srt::SrtSocketBuilder::new()
            .connect(format!("127.0.0.1:{port}"))
            .expect("connect");
        let _ = release_rx.recv();
        drop(socket);
    });

    ready.store(true, Ordering::SeqCst);
    let result = listener.accept_timeout(Duration::from_secs(2));
    let _ = release_tx.send(());
    let _ = connector.join();
    assert!(
        result.is_ok(),
        "accept should succeed; got {}",
        result.err().map(|e| format!("{e:?}")).unwrap_or_default()
    );
}

// Regression for the race that made `accept_timeout_succeeds_when_peer_connects`
// flake under workspace test load: when the connector's handshake completed
// *before* accept_timeout reached `srt_epoll_add_usock`, libsrt would not
// retroactively populate the new subscription's state, so srt_epoll_wait
// blocked until the timeout fired despite a queued connection. Fixed by
// the non-blocking accept probe between epoll_add and epoll_wait. This
// test forces the racy ordering deterministically by joining the connector
// thread before invoking accept_timeout.
#[test]
fn accept_timeout_drains_connection_queued_before_subscribe() {
    require_loopback!();
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    let connector = std::thread::spawn(move || {
        tst_srt::SrtSocketBuilder::new()
            .connect(format!("127.0.0.1:{port}"))
            .expect("connect")
    });
    // SRT connect() blocks until the handshake completes, so once join
    // returns the listener has the new connection queued.
    let _peer = connector.join().expect("connector thread panicked");

    let start = std::time::Instant::now();
    let result = listener.accept_timeout(Duration::from_millis(500));
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "accept_timeout missed connection queued before epoll subscribe (elapsed={elapsed:?}); err={:?}",
        result.err()
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "accept_timeout took {elapsed:?} for an already-queued connection — non-blocking probe is not draining the queue"
    );
}

/// A peer arriving after a stale event is still accepted
/// within the same call. A stale event cannot be forced from outside the
/// crate (the call's first probe clears any readiness left over from before
/// it, and libsrt's GC timing is not observable), so this pins the
/// surrounding shape: the peer arrives while the call is already waiting,
/// and it is that peer the call returns. The stale case itself is pinned by
/// the unit tests in `listener.rs`, which empty the queue between readiness
/// and accept through a test hook.
#[test]
fn accept_timeout_accepts_a_peer_that_arrives_after_stale_readiness() {
    require_loopback!();
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let watchdog = tst_test_helpers::timing::scaled(Duration::from_secs(20));

    // Held until accepted: a connector closing mid-accept lets libsrt's GC
    // reap the queued connection (see the test above).
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let ready = Arc::new(AtomicBool::new(false));
    let r = ready.clone();
    let connector = std::thread::spawn(move || {
        crate::common::wait_for_ready(&r);
        // Let the call get past its first probe into the wait. Either way
        // the assertions hold; the pause only makes the wait the usual path.
        std::thread::sleep(Duration::from_millis(200));
        let socket = tst_srt::SrtSocketBuilder::new()
            .connect(format!("127.0.0.1:{port}"))
            .ok()?;
        let local = socket.local_addr().ok();
        let _ = release_rx.recv_timeout(watchdog);
        local
    });

    ready.store(true, Ordering::SeqCst);
    let result = listener.accept_timeout(tst_test_helpers::timing::scaled(Duration::from_secs(5)));
    let accepted_port = result.as_ref().map(|(_, peer)| peer.port()).ok();
    let _ = release_tx.send(());
    let connector_local = connector.join().expect("connector thread panicked");
    assert!(
        result.is_ok(),
        "accept should succeed; got {:?}",
        result.err()
    );
    // Ports only: the connector's socket may be bound to the wildcard.
    assert_eq!(accepted_port, connector_local.map(|a| a.port()));
}
