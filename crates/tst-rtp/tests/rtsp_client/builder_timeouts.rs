//! Verify that `RtspClientBuilder`'s `connect_timeout`, `read_timeout`,
//! and `user_agent` fields are actually wired through to the socket and
//! request headers — not silently ignored.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// `connect_timeout` must be honored: a 1-second limit on a blackhole
/// address must return an error in well under the old hardcoded 10 s.
///
/// `10.255.255.1` is a non-routable address guaranteed never to RST —
/// the SYN hangs until the TCP connect timeout fires.
#[test]
fn connect_timeout_is_honored() {
    let url = "rtsp://10.255.255.1:554/x";
    let start = Instant::now();
    let result = tst_rtp::RtspClientBuilder::new(url)
        .unwrap()
        .connect_timeout(Duration::from_secs(1))
        .connect();
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "expected connection to blackhole to fail, but it succeeded"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "connect_timeout of 1 s was ignored — elapsed {elapsed:?} exceeds 4 s"
    );
}

/// `user_agent` must appear in the OPTIONS wire request.
///
/// Uses a hand-rolled TCP accept loop so we can inspect raw bytes.
#[test]
fn user_agent_is_sent_in_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Background: accept one connection, read bytes, reply a 200 OPTIONS
    // so the client can complete options(), then close.
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = vec![0u8; 4096];
        let mut acc = String::new();
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            acc.push_str(std::str::from_utf8(&buf[..n]).unwrap_or(""));
            if acc.contains("\r\n\r\n") {
                break;
            }
        }
        // Reply 200 OK so the client's options() call returns.
        let _ = sock.write_all(
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n",
        );
        acc
    });

    let url = format!("rtsp://127.0.0.1:{port}/test");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .user_agent("my-custom-agent/9.9")
        .connect()
        .unwrap();
    let _ = client.options(); // drive the wire exchange

    let received = server.join().unwrap();
    let lower = received.to_ascii_lowercase();
    assert!(
        lower.contains("my-custom-agent/9.9"),
        "custom User-Agent not found in OPTIONS request; got:\n{received}"
    );
}

/// CORR-26: `request_timeout` bounds the wait for a response. The peer
/// accepts the TCP connection and never answers — before the fix
/// `describe()` parked forever (the only deadline in the client was the
/// 500 ms TEARDOWN bound inside `Drop`) and `RtspError::Timeout` had no
/// producer at all.
///
/// Bounded by a watchdog on a pre-obtained cancel handle so the RED cannot
/// hang CI: without the fix the watchdog fires at 5 s, the call ends
/// `LocalCancel`, and the assertion below fails. With the fix the call ends
/// `Timeout` at ~300 ms and the watchdog is released before it fires.
#[test]
fn request_timeout_bounds_a_silent_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        if let Ok((sock, _)) = listener.accept() {
            // Accept and hold: no read, no reply, no FIN until the client
            // side has finished asserting.
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
            drop(sock);
        }
    });

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .request_timeout(Some(Duration::from_millis(300)))
        .connect()
        .unwrap();
    let cancel = client.cancel_handle();
    let (wd_tx, wd_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if wd_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            cancel.cancel();
        }
    });

    let err = client.describe().err();
    let _ = wd_tx.send(());
    assert!(
        matches!(err, Some(tst_rtp::RtspError::Timeout)),
        "expected RtspError::Timeout after the 300 ms request deadline, got {err:?}"
    );

    let _ = done_tx.send(());
    drop(client);
    watchdog.join().unwrap();
    server.join().unwrap();
}

/// Post-Arc-1 review finding: `request_timeout` deadline arithmetic must not
/// panic when `t` is too large to add to `Instant::now()`. Before the fix,
/// both `send_and_read` (the producer behind every request method, incl.
/// `options()`) and `teardown()` computed the deadline as
/// `Instant::now() + t` — an unchecked add that panics on overflow for
/// `Duration::MAX`. `teardown()` computed that deadline BEFORE its
/// no-session early return, so even a no-op teardown call would have
/// panicked.
#[test]
fn request_timeout_duration_max_does_not_panic() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Same hand-rolled accept-and-reply shape as `user_agent_is_sent_in_requests`
    // above, but replies with the CSeq the client actually sent (rather than
    // a hardcoded "1") so the response is a well-formed match for any request.
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = vec![0u8; 4096];
        let mut acc = String::new();
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            acc.push_str(std::str::from_utf8(&buf[..n]).unwrap_or(""));
            if acc.contains("\r\n\r\n") {
                break;
            }
        }
        let cseq = acc
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("cseq:").map(|v| v.trim().to_string()))
            .unwrap_or_else(|| "1".to_string());
        let _ = sock.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes());
    });

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .request_timeout(Some(Duration::MAX))
        .connect()
        .unwrap();

    let result = client.options();
    assert!(
        result.is_ok(),
        "options() with request_timeout(Some(Duration::MAX)) must not panic \
         and must succeed, got {result:?}"
    );

    // No SETUP ever ran, so there is no session — teardown() must be the
    // documented no-op `Ok(())`, not a panic from the same deadline
    // arithmetic computed before that early return.
    let result = client.teardown();
    assert!(
        matches!(result, Ok(())),
        "teardown() with no session must be a no-op Ok(()), got {result:?}"
    );

    server.join().unwrap();
}
