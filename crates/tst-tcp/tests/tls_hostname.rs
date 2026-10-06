//! TLS loopback test: `tcps://` hostname dial verifies against a dnsName-SAN cert.
//!
//! The client dials by *hostname*
//! (`localhost`) and rustls verifies the server certificate's `dnsName` SAN.
//!
//! The positive-path cert carries ONLY a `dnsName` SAN for `localhost` (no
//! `iPAddress` SAN). This means an IP-literal dial (`127.0.0.1`) against the
//! same cert MUST fail, which anchors both legs of the test:
//!
//! - `tcps_hostname_loopback_handshake_and_roundtrip` — dials `localhost`
//!   → cert has a matching `dnsName` → handshake succeeds.
//! - `tcps_ip_dial_against_dns_only_cert_loopback_fails` — dials `127.0.0.1`
//!   → cert has no `iPAddress` SAN → the connect itself fails (the client
//!   handshake runs inside `connect`, under `connect_timeout`).
//! - `tcps_stalled_handshake_loopback_is_connect_timeout` — the peer accepts
//!   TCP but never speaks TLS → `ConnectTimeout`, not a first-send
//!   `Backpressure`.
//!
//! The test binary is only compiled when the `tls` feature is active (the
//! crate's default). Without TLS there is nothing to exercise.

#![cfg(feature = "tls")]

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportError};
use tst_tcp::config::SocketConfig;
use tst_tcp::error::TcpError;
use tst_tcp::url::TcpUrl;
use tst_tcp::{TcpListener, TcpTransport};

// ---------------------------------------------------------------------------
// Cert fixture helpers
// ---------------------------------------------------------------------------

/// Self-signed cert that carries ONLY a `dnsName` SAN for `localhost`
/// (no `iPAddress` SAN). Written to files in a temp directory.
///
/// This is intentionally dnsName-only so that:
/// - A hostname dial (`localhost`) succeeds (the dnsName matches).
/// - An IP-literal dial (`127.0.0.1`) fails (no iPAddress SAN).
///
/// The temp directory (and the files inside it) live for the lifetime of the
/// returned `TempDir`. Drop it after the test completes.
fn gen_dns_only_cert() -> (
    tempfile::TempDir,
    std::path::PathBuf, // cert.pem / CA bundle
    std::path::PathBuf, // key.pem
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert generation");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    let dir = tempfile::tempdir().expect("create tempdir for TLS fixture");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, &cert_pem).expect("write cert.pem");
    std::fs::write(&key_path, &key_pem).expect("write key.pem");
    // The self-signed cert doubles as the trust anchor (CA bundle).
    (dir, cert_path, key_path)
}

// ---------------------------------------------------------------------------
// Helper: accept one connection, echo N bytes, close.
// ---------------------------------------------------------------------------

fn echo_server(listener: TcpListener) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut conn = listener.accept_blocking().expect("server accept");
        let mut buf = [0u8; 4];
        let n = conn.recv_bytes(&mut buf).expect("server recv");
        conn.send_bytes(&buf[..n]).expect("server send echo");
    })
}

// ---------------------------------------------------------------------------
// Positive test: hostname dial verifies against dnsName SAN
// ---------------------------------------------------------------------------

/// Full `tcps://` TLS handshake + ping/echo round-trip where the client dials
/// by *hostname* (`localhost`) and the server certificate carries that name
/// as a `dnsName` SAN (with NO `iPAddress` SAN).
///
/// The core assertion: hostname SNI works end-to-end.
/// If `tls.rs` reverted to the old IP-string server-name form (presenting the
/// resolved `127.0.0.1` for SNI), this test would fail with a certificate
/// mismatch error because the cert has no `iPAddress` SAN.
///
/// Test name contains "loopback" so nextest assigns it to the `network` group
/// (serialised, single-threaded; avoids port contention and timing flakes).
#[test]
fn tcps_hostname_loopback_handshake_and_roundtrip() {
    let (_dir, cert_path, key_path) = gen_dns_only_cert();
    // The self-signed cert itself is the CA bundle the client trusts.
    let ca_path = cert_path.clone();

    // Bind TLS listener on an ephemeral port (port 0 → OS assigns).
    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");

    let port = listener.local_addr().expect("local_addr after bind").port();
    assert_ne!(port, 0, "OS must have assigned a non-zero port");

    // Spawn echo server — accepts once, echoes 4 bytes, exits.
    let srv = echo_server(listener);

    // --- THE POINT OF THIS TEST ---
    // Dial by *hostname*. The cert has ONLY a dnsName SAN for "localhost"
    // (no iPAddress SAN). rustls must accept the handshake because the SNI
    // ("localhost") matches the dnsName SAN. An old IP-based SNI would present
    // "127.0.0.1" → no matching iPAddress SAN → certificate error.
    let dial_url = format!("tcps://localhost:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut client = TcpTransport::connect_with_config(&parsed, &SocketConfig::default())
        .expect("tcps connect (hostname dial must succeed with dnsName SAN)");

    // The handshake already completed inside `connect_with_config`, so the
    // first send is an ordinary application write: no zero-progress
    // `Backpressure` from a handshake read ticking over the 100 ms cancel-poll
    // socket timeout (that was the lazy-handshake shape this test used to
    // retry around).
    client.send_bytes(b"ping").expect("client send");

    let mut buf = [0u8; 4];
    let n = client.recv_bytes(&mut buf).expect("client recv");
    assert_eq!(n, 4);
    assert_eq!(&buf[..n], b"ping", "echoed payload must match");

    srv.join().expect("server thread panicked");
}

// ---------------------------------------------------------------------------
// Negative test: IP-literal dial against a dnsName-only cert fails
// ---------------------------------------------------------------------------

/// Confirm the inverse: if we dial by IP literal (`127.0.0.1`) but the cert
/// carries *only* a `dnsName` SAN (for `localhost`, no `iPAddress` SAN),
/// rustls MUST reject the handshake.
///
/// The client handshake runs inside `connect_with_config`, so the rejection
/// is a connect error (`TcpError::Tls`, the certificate failure's own kind),
/// not something that surfaces on a later send or recv. The connect used to
/// return `Ok` with a lazy handshake and the error appeared on the first
/// I/O; a reconnect wrapper then saw a "connected" transport fail on its
/// first message instead of a connect failure it could count and back off.
///
/// Test name contains "loopback" for nextest network group membership.
#[test]
fn tcps_ip_dial_against_dns_only_cert_loopback_fails() {
    let (_dir, cert_path, key_path) = gen_dns_only_cert();
    let ca_path = cert_path.clone();

    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");

    let port = listener.local_addr().expect("local_addr").port();

    // Spawn an accept thread that drives the server half of the handshake
    // with one read (the accepted transport's handshake runs on its first
    // I/O): that is what presents the certificate for the client to reject.
    // Dropping the accepted socket without any I/O would reset the unread
    // ClientHello instead, and the client would see `Io(ConnectionReset)`.
    // The read ends in the client's alert (an error we intentionally
    // ignore). Bind to a named variable so we can join on all paths.
    let srv = thread::spawn(move || {
        if let Ok(mut conn) = listener.accept_blocking() {
            let mut buf = [0u8; 4];
            let _ = conn.recv_bytes(&mut buf);
        }
    });

    // Dial by IP literal against a cert that has no iPAddress SAN: rustls
    // rejects the certificate during the handshake, which `connect` runs.
    let dial_url = format!("tcps://127.0.0.1:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let result = TcpTransport::connect_with_config(&parsed, &SocketConfig::default());

    // Join before asserting so the server thread is always reaped, even if
    // the assertion below fires (no parked accept_blocking leaks on test fail).
    let _ = srv.join();

    match result {
        Err(TcpError::Tls(msg)) => assert!(
            msg.to_lowercase().contains("certificate") || msg.to_lowercase().contains("handshake"),
            "the Tls error must name the handshake/certificate failure, got: {msg}"
        ),
        Err(other) => panic!("expected TcpError::Tls at connect, got {other:?}"),
        Ok(_) => panic!("IP-literal dial against a dnsName-only cert must fail at connect"),
    }
}

// ---------------------------------------------------------------------------
// Negative test: a peer that accepts TCP but never speaks TLS
// ---------------------------------------------------------------------------

/// A `tcps://` connect whose TLS handshake never completes — the peer
/// accepts the TCP connection and then stays silent (a plain-TCP service on
/// the port, a firewall that proxies the SYN, a wedged TLS terminator) — must
/// fail as `ConnectTimeout` after `connect_timeout`, the same outcome a
/// SYN that is never answered gets. Before the handshake moved into
/// `connect`, this connect returned `Ok` and every later send reported a
/// zero-progress `Backpressure` forever (the handshake read ticking over the
/// 100 ms socket timeout), so a managed sender believed it was connected and
/// never reconnected.
///
/// The handshake gets its own `connect_timeout` budget after the TCP connect
/// (`seconds: 1` here), and the whole connect must end well inside the 5 s
/// bound below.
///
/// Test name contains "loopback" for nextest network group membership.
#[test]
fn tcps_stalled_handshake_loopback_is_connect_timeout() {
    let (_dir, cert_path, _key_path) = gen_dns_only_cert();

    // A plain TCP listener: it completes the kernel handshake and holds the
    // socket open, but never answers the ClientHello.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("bind silent listener");
    let port = silent.local_addr().expect("local_addr").port();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let holder = thread::spawn(move || {
        let held = silent.accept().map(|(s, _)| s);
        // Keep the accepted socket open until the client has given up, so the
        // client cannot see an EOF or reset instead of a timeout.
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
        drop(held);
    });

    let url = format!(
        "tcps://localhost:{port}?connect_timeout=1&ca={}",
        cert_path.display()
    );
    let started = std::time::Instant::now();
    let result = TcpTransport::connect(&url);
    let elapsed = started.elapsed();
    let _ = done_tx.send(());
    holder.join().expect("holder thread panicked");

    match result {
        Err(TcpError::ConnectTimeout { seconds: 1 }) => {}
        Err(other) => panic!("expected ConnectTimeout {{ seconds: 1 }}, got {other:?}"),
        Ok(_) => panic!("a stalled TLS handshake must not report a connected transport"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "connect must give up at connect_timeout, took {elapsed:?}"
    );
}

/// A peer that keeps the handshake alive by dripping bytes — a plausible
/// TLS record header followed by one payload byte every 50 ms, so rustls
/// keeps waiting for the record to complete and every socket read
/// succeeds before the 100 ms socket timeout can fire — must still be cut
/// off at `connect_timeout`. The deadline has to be enforced per socket
/// read, not only when a read times out: `ClientConnection::complete_io`
/// loops internally until the handshake completes, so a deadline checked
/// only between its calls never runs against a slow-drip peer (a review
/// finding on the first version of the eager handshake).
///
/// The drip stops after 4 s (the server then closes), so a regression
/// shows up as an `Io` error after ~4 s instead of `ConnectTimeout` at
/// ~1 s — and the elapsed bound below is what catches it.
///
/// Test name contains "loopback" for nextest network group membership.
#[test]
fn tcps_slow_drip_handshake_loopback_is_connect_timeout() {
    let (_dir, cert_path, _key_path) = gen_dns_only_cert();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind drip listener");
    let port = listener.local_addr().expect("local_addr").port();
    let dripper = thread::spawn(move || {
        use std::io::Write;
        let (mut sock, _) = listener.accept().expect("accept");
        // Handshake record header: content type 22, TLS 1.2 framing,
        // length 4000 — rustls buffers the record until all 4000 bytes
        // have arrived, and this peer never sends them all.
        let _ = sock.write_all(&[0x16, 0x03, 0x03, 0x0F, 0xA0]);
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(4) {
            if sock.write_all(&[0x00]).is_err() {
                break; // the client gave up — the outcome under test
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    let url = format!(
        "tcps://localhost:{port}?connect_timeout=1&ca={}",
        cert_path.display()
    );
    let started = std::time::Instant::now();
    let result = TcpTransport::connect(&url);
    let elapsed = started.elapsed();
    dripper.join().expect("dripper thread panicked");

    match result {
        Err(TcpError::ConnectTimeout { seconds: 1 }) => {}
        Err(other) => panic!("expected ConnectTimeout {{ seconds: 1 }}, got {other:?}"),
        Ok(_) => panic!("a dripping handshake must not report a connected transport"),
    }
    assert!(
        elapsed < Duration::from_millis(2500),
        "connect must give up at connect_timeout even while bytes trickle in, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Explicit close on a TLS transport is visible to the peer
// ---------------------------------------------------------------------------

/// `Transport::close` on a `tcps://` transport must terminate the peer's read,
/// even while the transport itself is still alive in scope.
///
/// The TLS arm of `InnerStream::shutdown` used to be empty ("handled in the
/// StreamOwned/socket drop"), which made `close()` a no-op on the wire: a
/// caller that closes but retains the transport (a pipeline shell holding it
/// in a struct, a reconnect wrapper parking a dead leg) left the peer parked
/// on a read until the transport was eventually dropped. Closing must send
/// `close_notify` and shut the socket at the point of the call.
///
/// Every wait is bounded and the server thread is joined on all paths.
/// Test name contains "loopback" for nextest network group membership.
#[test]
fn tcps_explicit_close_loopback_ends_the_peer_read() {
    let (_dir, cert_path, key_path) = gen_dns_only_cert();
    let ca_path = cert_path.clone();

    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");
    let port = listener.local_addr().expect("local_addr after bind").port();

    // Server: accept, consume the client's first payload and report
    // readiness, then park on a
    // second read. That second read is the observation point — it must end
    // once the client calls close(), not run out the 2 s deadline below.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (result_tx, result_rx) = mpsc::channel::<Result<usize, TransportError>>();
    let srv = thread::spawn(move || {
        let mut conn = listener.accept_blocking().expect("server accept");
        let mut buf = [0u8; 4];
        let n = conn.recv_bytes(&mut buf).expect("server recv ping");
        assert_eq!(n, 4, "server must see the full 4-byte ping");
        let _ = ready_tx.send(());
        let mut after = [0u8; 16];
        let _ = result_tx.send(conn.recv_bytes(&mut after));
    });

    let dial_url = format!("tcps://localhost:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut client =
        TcpTransport::connect_with_config(&parsed, &SocketConfig::default()).expect("tcps connect");
    // The client's handshake completed inside connect; this send is plain
    // application data (the server's lazy half completed when its accept
    // answered the ClientHello).
    client.send_bytes(b"ping").expect("client send");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server did not complete the TLS handshake");

    // THE POINT: close explicitly, but keep `client` alive in scope so the
    // socket cannot be closed by Drop instead.
    Transport::close(&mut client);

    let observed = result_rx.recv_timeout(Duration::from_secs(2));

    // Release the server thread on the failing path too (it is still parked on
    // its second read there): dropping the client closes the socket for real.
    drop(client);
    srv.join().expect("server thread panicked");

    let observed =
        observed.expect("peer read did not end within 2 s of an explicit close() on the client");
    // Isolate `close_notify` from the socket shutdown that follows it: a clean
    // TLS close reaches the server as rustls's `Ok(0)`, which `recv_bytes` maps
    // to `Broken { cause: BrokenCause::CleanEof, .. }`. A bare socket shutdown
    // with no `close_notify` would still end the read, but through rustls's
    // `UnexpectedEof` error and the "read error: …" arm (`cause: Unspecified`)
    // — so a Broken of any shape is not enough to prove the alert was sent.
    match observed {
        Err(TransportError::Broken {
            cause, errno_code, ..
        }) => {
            assert_eq!(
                cause,
                BrokenCause::CleanEof,
                "peer read must end through the clean close_notify (Ok(0)) arm"
            );
            assert_eq!(
                errno_code, None,
                "a clean close_notify carries no errno_code"
            );
        }
        other => panic!("peer read must end with Broken (close_notify EOF), got {other:?}"),
    }
}

/// `close()` followed at once by `drop()` — the common shape in every binding
/// (`with`, `try`, `AutoCloseable`, `tst_*_close` + free) — must still reach
/// the peer as `close_notify`, even when the peer only gets round to reading
/// after both have happened.
///
/// A TLS 1.3 server sends `NewSessionTicket` records right after the
/// handshake (rustls: two of them by default). A write-only client never reads
/// them, so they sit unread in its receive buffer. A socket closed or shut
/// down with unread receive data is answered by the kernel with RST instead of
/// FIN (Windows at `shutdown`, Linux at `close`), and a RST can overtake and
/// purge the `close_notify` we just wrote, so the peer sees a reset
/// (`BrokenCause::Unspecified`) instead of a clean EOF; on windows-msvc
/// that fails `tcps_explicit_close_loopback_ends_the_peer_read`. The close
/// drains the already-received records before the alert; this test pins it on the
/// shape that trips the kernel on every platform (close + drop), with the
/// peer's read deliberately late so the drop has landed before it looks.
#[test]
fn tcps_close_then_drop_loopback_still_reaches_the_peer_as_close_notify() {
    let (_dir, cert_path, key_path) = gen_dns_only_cert();
    let ca_path = cert_path.clone();

    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");
    let port = listener.local_addr().expect("local_addr after bind").port();

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (closed_tx, closed_rx) = mpsc::channel::<()>();
    let (result_tx, result_rx) = mpsc::channel::<Result<usize, TransportError>>();
    let srv = thread::spawn(move || {
        let mut conn = listener.accept_blocking().expect("server accept");
        let mut buf = [0u8; 4];
        let n = conn.recv_bytes(&mut buf).expect("server recv ping");
        assert_eq!(n, 4, "server must see the full 4-byte ping");
        let _ = ready_tx.send(());
        // Read only once the client has closed AND dropped, so whatever the
        // kernel did at close time has already reached this side.
        let _ = closed_rx.recv_timeout(Duration::from_secs(5));
        thread::sleep(Duration::from_millis(200));
        let mut after = [0u8; 16];
        let _ = result_tx.send(conn.recv_bytes(&mut after));
    });

    let dial_url = format!("tcps://localhost:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut client =
        TcpTransport::connect_with_config(&parsed, &SocketConfig::default()).expect("tcps connect");
    client.send_bytes(b"ping").expect("client send");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server did not complete the TLS handshake");
    // Give the server's post-handshake records time to land in the client's
    // receive buffer: that unread data is the trigger under test.
    thread::sleep(Duration::from_millis(100));

    Transport::close(&mut client);
    drop(client);
    let _ = closed_tx.send(());

    let observed = result_rx.recv_timeout(Duration::from_secs(5));
    srv.join().expect("server thread panicked");
    let observed = observed.expect("peer read did not end after close + drop");
    match observed {
        Err(TransportError::Broken { cause, .. }) => assert_eq!(
            cause,
            BrokenCause::CleanEof,
            "peer must see close_notify (clean EOF), not a reset"
        ),
        other => panic!("peer read must end with Broken (close_notify EOF), got {other:?}"),
    }
}

/// The two edges of the same close path: closing a TLS transport *before* its
/// handshake has run (no keys yet, so `send_close_notify` has nothing to
/// encrypt) and closing twice (the socket is already shut down the second
/// time). Both must be quiet no-ops — `close()` is reached from the C ABI and
/// the bindings, where a panic would abort the caller's process.
///
/// The client side can no longer be in that state (its handshake completes
/// inside `connect`, or the connect fails), so the pre-handshake edge is
/// exercised on the server's accepted transport: `accept_blocking` returns
/// before the server half of the handshake has run (it runs on the first
/// I/O), and this test closes it without ever doing any.
#[test]
fn tcps_close_loopback_before_handshake_and_twice_is_quiet() {
    let (_dir, cert_path, key_path) = gen_dns_only_cert();
    let ca_path = cert_path.clone();

    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");
    let port = listener.local_addr().expect("local_addr after bind").port();

    // The client dials in the background. Its handshake needs the server to
    // answer, which this test never does: the connect ends in an error once
    // the server closes the socket (or at connect_timeout), and that result
    // is deliberately not asserted here.
    let dial_url = format!(
        "tcps://localhost:{port}?connect_timeout=2&ca={}",
        ca_path.display()
    );
    let client = thread::spawn(move || {
        let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
        let _ = TcpTransport::connect_with_config(&parsed, &SocketConfig::default());
    });

    let mut accepted = listener.accept_blocking().expect("server accept");
    Transport::close(&mut accepted);
    Transport::close(&mut accepted);
    assert!(!Transport::is_alive(&accepted), "close() must mark dead");

    drop(accepted);
    client.join().expect("client thread panicked");
}
