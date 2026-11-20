//! Verify that `RtspClientBuilder`'s `connect_timeout`, `request_timeout`
//! and `user_agent` reach the connect, the request wait and the request
//! headers, and that a long `read_timeout` (which bounds only the reads of
//! a TLS handshake) changes nothing after connect.

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
    // Not scaled: an ignored setting falls back to the builder's 10 s
    // default, so the bound must stay under 10 s at any scale. The connect
    // itself is bounded either way, so there is no hang to guard.
    assert!(
        elapsed < Duration::from_secs(8),
        "connect_timeout of 1 s was ignored — elapsed {elapsed:?} exceeds 8 s"
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

/// `request_timeout` bounds the wait for a response. The peer
/// accepts the TCP connection and never answers — before the fix
/// `describe()` parked forever (the client had no request deadline) and
/// `RtspError::Timeout` had no producer at all.
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

/// `request_timeout` deadline arithmetic must not
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

// --- `request_timeout` vs a longer `read_timeout` ---
//
// `read_timeout` bounds only the reads of a TLS handshake: these pin that a
// long one changes nothing after connect, where the request deadline and
// the cancel end the call. They assert on elapsed time. A call stretched to
// the 30 s read timeout would fail the 15 s bound, which is half of it and
// needs no scaling: a slow host cannot push the right behavior across it.
// No socket timeout applies after connect, so what ends a call a
// regression leaves waiting is the server thread's own cap (40 s at most),
// after which it closes the connection.

const SHORT_REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const LONG_READ_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT: Duration = Duration::from_secs(15);

/// Accept one connection and hold it open, silent, until `done` fires.
fn silent_peer() -> (
    u16,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        if let Ok((sock, _)) = listener.accept() {
            let _ = done_rx.recv_timeout(Duration::from_secs(40));
            drop(sock);
        }
    });
    (port, done_tx, server)
}

/// A silent peer ends the call at the request deadline, not at the (much
/// longer) socket read timeout.
#[test]
// Pins what the deprecated `read_timeout` does not bound.
#[allow(deprecated)]
fn request_timeout_is_not_stretched_by_a_longer_read_timeout() {
    let (port, done_tx, server) = silent_peer();

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(LONG_READ_TIMEOUT)
        .request_timeout(Some(SHORT_REQUEST_TIMEOUT))
        .connect()
        .unwrap();

    let start = Instant::now();
    let err = client.options().err();
    let elapsed = start.elapsed();
    let _ = done_tx.send(());

    assert!(
        matches!(err, Some(tst_rtp::RtspError::Timeout)),
        "expected RtspError::Timeout, got {err:?} after {elapsed:?}"
    );
    assert!(
        elapsed < PROMPT,
        "request_timeout = {SHORT_REQUEST_TIMEOUT:?} but the call took {elapsed:?} \
         (read_timeout = {LONG_READ_TIMEOUT:?})"
    );

    drop(client);
    server.join().unwrap();
}

/// A well-formed response that arrives 20 s after the request, long past the
/// request deadline, is a `Timeout`, not a success.
#[test]
// Pins what the deprecated `read_timeout` does not bound.
#[allow(deprecated)]
fn response_after_the_request_deadline_is_a_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
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
            // Stay silent for 20 s (or until the client is already done).
            let _ = done_rx.recv_timeout(Duration::from_secs(20));
            let _ = sock.write_all(
                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS\r\n\r\n").as_bytes(),
            );
        }
    });

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(LONG_READ_TIMEOUT)
        .request_timeout(Some(SHORT_REQUEST_TIMEOUT))
        .connect()
        .unwrap();

    let start = Instant::now();
    let result = client.options();
    let elapsed = start.elapsed();
    let _ = done_tx.send(());

    assert!(
        matches!(result, Err(tst_rtp::RtspError::Timeout)),
        "a response 20 s after a request with a {SHORT_REQUEST_TIMEOUT:?} deadline \
         must be a Timeout; got {:?} after {elapsed:?}",
        result.as_ref().map(|_| "Ok(<response>)")
    );
    assert!(
        elapsed < PROMPT,
        "request_timeout = {SHORT_REQUEST_TIMEOUT:?} but the call took {elapsed:?}"
    );

    drop(client);
    server.join().unwrap();
}

/// A cancel is seen within a short tick even with no request deadline and
/// a long socket read timeout.
#[test]
// Pins what the deprecated `read_timeout` does not bound.
#[allow(deprecated)]
fn cancel_latency_does_not_depend_on_read_timeout() {
    let (port, done_tx, server) = silent_peer();

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(LONG_READ_TIMEOUT)
        .request_timeout(None)
        .connect()
        .unwrap();
    let cancel = client.cancel_handle();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });

    let start = Instant::now();
    let err = client.options().err();
    let elapsed = start.elapsed();
    let _ = done_tx.send(());

    assert!(
        matches!(err, Some(tst_rtp::RtspError::LocalCancel)),
        "expected RtspError::LocalCancel, got {err:?} after {elapsed:?}"
    );
    assert!(
        elapsed < PROMPT,
        "cancel took {elapsed:?} to be seen (read_timeout = {LONG_READ_TIMEOUT:?})"
    );

    canceller.join().unwrap();
    drop(client);
    server.join().unwrap();
}

/// `rtsps://`: the request deadline holds over TLS too, with the same long
/// `read_timeout`. The peer completes the TLS handshake and then never
/// answers.
#[cfg(feature = "tls")]
#[test]
// Pins what the deprecated `read_timeout` does not bound.
#[allow(deprecated)]
fn request_timeout_is_not_stretched_by_a_longer_read_timeout_over_tls() {
    let certs = crate::fixtures::tls_certs::SelfSignedCert::generate();
    let key_pem = std::fs::read(&certs.key_path).unwrap();
    let roots =
        tst_core::net::tls::load_root_store(certs.root_pem.as_bytes()).expect("trust bundle");
    let config = std::sync::Arc::new(
        tst_core::net::tls::server_config_from_pem(certs.root_pem.as_bytes(), &key_pem).unwrap(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            // Bound the handshake so a client that never completes it
            // cannot park this thread.
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut conn = rustls::ServerConnection::new(config).unwrap();
            while conn.is_handshaking() {
                if conn.complete_io(&mut sock).is_err() {
                    return;
                }
            }
            let _ = done_rx.recv_timeout(Duration::from_secs(40));
        }
    });

    let url = format!("rtsps://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(LONG_READ_TIMEOUT)
        .request_timeout(Some(SHORT_REQUEST_TIMEOUT))
        .tls_root_certs(roots)
        .connect()
        .unwrap();

    let start = Instant::now();
    let err = client.options().err();
    let elapsed = start.elapsed();
    let _ = done_tx.send(());

    assert!(
        matches!(err, Some(tst_rtp::RtspError::Timeout)),
        "expected RtspError::Timeout, got {err:?} after {elapsed:?}"
    );
    assert!(
        elapsed < PROMPT,
        "request_timeout = {SHORT_REQUEST_TIMEOUT:?} but the call took {elapsed:?} \
         (read_timeout = {LONG_READ_TIMEOUT:?})"
    );

    drop(client);
    server.join().unwrap();
}

/// A reply that arrives after its request timed out is not taken for the
/// reply to the next request: the peer answers request 1 only once
/// request 2 is in, then answers request 2, and request 2 returns its own
/// reply.
#[test]
fn a_late_reply_is_not_handed_to_the_next_request() {
    use crate::fixtures::raw_rtsp_server::{cseq_of, read_request};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let first = cseq_of(&read_request(&mut sock, &mut leftover));
        // Request 2 is written only after request 1 timed out.
        let second = cseq_of(&read_request(&mut sock, &mut leftover));
        for (cseq, n) in [(first, 1), (second, 2)] {
            sock.write_all(
                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nX-Reply: {n}\r\n\r\n").as_bytes(),
            )
            .unwrap();
        }
        // Hold the connection until the client hangs up.
        let _ = sock.read_to_end(&mut Vec::new());
    });

    let url = format!("rtsp://127.0.0.1:{port}/x");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .request_timeout(Some(Duration::from_millis(200)))
        .connect()
        .unwrap();

    let first = client.options();
    assert!(
        matches!(first, Err(tst_rtp::RtspError::Timeout)),
        "request 1 has no reply within its deadline; got {first:?}"
    );
    let second = client.options().expect("request 2 gets a reply");
    assert_eq!(
        second.headers.get("x-reply").map(String::as_str),
        Some("2"),
        "request 2 must get its own reply, not request 1's late one"
    );

    drop(client);
    server.join().unwrap();
}
