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

// --- `request_timeout` vs a longer `read_timeout` ---
//
// The request deadline and the cancel flag must be honored whatever the
// per-read socket timeout is configured to. These are deadline tests, so
// they assert on elapsed time, with a bound 20x the configured deadline
// and well under the 5 s read timeout, so the outcomes cannot be confused.
// None of them can hang: the 5 s socket read timeout ends the client call
// even without the behavior under test, and each server thread has its
// own cap.

const SHORT_REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const LONG_READ_TIMEOUT: Duration = Duration::from_secs(5);
const PROMPT: Duration = Duration::from_secs(2);

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
            let _ = done_rx.recv_timeout(Duration::from_secs(20));
            drop(sock);
        }
    });
    (port, done_tx, server)
}

/// A silent peer ends the call at the request deadline, not at the (much
/// longer) socket read timeout.
#[test]
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

/// A well-formed response that arrives 2 s after the request — twenty
/// request deadlines late — is a `Timeout`, not a success.
#[test]
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
            // Stay silent for 2 s (or until the client is already done).
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
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
        "a response 2 s after a request with a {SHORT_REQUEST_TIMEOUT:?} deadline \
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

/// `rtsps://`: the same bound holds when the blocking read sits beneath
/// rustls. The peer completes the TLS handshake and then never answers.
#[cfg(feature = "tls")]
#[test]
fn request_timeout_is_not_stretched_by_a_longer_read_timeout_over_tls() {
    let certs = crate::fixtures::tls_certs::SelfSignedCert::generate();
    let chain: Vec<_> = rustls_pemfile::certs(&mut certs.root_pem.as_bytes())
        .map(|c| c.unwrap())
        .collect();
    let key_pem = std::fs::read(&certs.key_path).unwrap();
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .unwrap()
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in &chain {
        roots.add(cert.clone()).unwrap();
    }
    let config = std::sync::Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
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
            let _ = done_rx.recv_timeout(Duration::from_secs(20));
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
