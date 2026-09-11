//! TLS loopback test: `tcps://` hostname dial verifies against a dnsName-SAN cert.
//!
//! This is the integration test for DA-NET-9: the client dials by *hostname*
//! (`localhost`) and rustls verifies the server certificate's `dnsName` SAN.
//!
//! The positive-path cert carries ONLY a `dnsName` SAN for `localhost` (no
//! `iPAddress` SAN). This means an IP-literal dial (`127.0.0.1`) against the
//! same cert MUST fail, which anchors both legs of the test:
//!
//! - `tcps_hostname_loopback_handshake_and_roundtrip` — dials `localhost`
//!   → cert has a matching `dnsName` → handshake succeeds.
//! - `tcps_ip_dial_against_dns_only_cert_loopback_fails` — dials `127.0.0.1`
//!   → cert has no `iPAddress` SAN → handshake fails on first I/O.
//!
//! The test binary is only compiled when the `tls` feature is active (the
//! crate's default). Without TLS there is nothing to exercise.

#![cfg(feature = "tls")]

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportError};
use tst_tcp::config::SocketConfig;
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
/// This is the core assertion of DA-NET-9: hostname SNI works end-to-end.
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

    // --- THE POINT OF DA-NET-9 ---
    // Dial by *hostname*. The cert has ONLY a dnsName SAN for "localhost"
    // (no iPAddress SAN). rustls must accept the handshake because the SNI
    // ("localhost") matches the dnsName SAN. An old IP-based SNI would present
    // "127.0.0.1" → no matching iPAddress SAN → certificate error.
    let dial_url = format!("tcps://localhost:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut client = TcpTransport::connect_with_config(&parsed, &SocketConfig::default())
        .expect("tcps connect (hostname dial must succeed with dnsName SAN)");

    // Trigger the TLS handshake and exercise the full round-trip.
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
/// The TLS handshake is lazy — it completes on the first I/O call, not at
/// connect time. So we trigger it via `send_bytes` and assert that either
/// the send or the subsequent `recv_bytes` returns an error.
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

    // Spawn an accept thread — the handshake failure closes the connection;
    // the server side may surface an error which we intentionally ignore.
    // Bind to a named variable so we can join on all paths (no thread leaks).
    let srv = thread::spawn(move || {
        let _ = listener.accept_blocking();
    });

    // Dial by IP literal against a cert that has no iPAddress SAN.
    // The TCP connect itself succeeds (lazy handshake), so we expect Ok here.
    let dial_url = format!("tcps://127.0.0.1:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut transport = TcpTransport::connect_with_config(&parsed, &SocketConfig::default())
        .expect("TCP connect returns Ok (handshake is lazy — not yet triggered)");

    // Trigger the handshake. The cert has no iPAddress SAN for 127.0.0.1
    // so rustls must reject it. The error surfaces on send or recv.
    let send_result = transport.send_bytes(b"ping");
    let error_observed = if send_result.is_err() {
        true
    } else {
        let mut buf = [0u8; 4];
        transport.recv_bytes(&mut buf).is_err()
    };

    // Join before asserting so the server thread is always reaped, even if
    // the assertion below fires (no parked accept_blocking leaks on test fail).
    let _ = srv.join();

    assert!(
        error_observed,
        "IP-literal dial against a dnsName-only cert must fail on first I/O"
    );
}

// ---------------------------------------------------------------------------
// CORR-10: explicit close on a TLS transport is visible to the peer
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

    // Server: accept, consume the client's first payload (which is what drives
    // the lazy handshake to completion) and report readiness, then park on a
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

/// The two edges of the same close path: closing a TLS transport *before* the
/// lazy handshake has run (no keys yet, so `send_close_notify` has nothing to
/// encrypt) and closing twice (the socket is already shut down the second
/// time). Both must be quiet no-ops — `close()` is reached from the C ABI and
/// the bindings, where a panic would abort the caller's process.
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

    // The server only has to complete the TCP accept; the handshake never runs.
    let srv = thread::spawn(move || {
        let _ = listener.accept_blocking();
    });

    let dial_url = format!("tcps://localhost:{port}?ca={}", ca_path.display());
    let parsed = TcpUrl::parse(&dial_url).expect("URL parse");
    let mut client = TcpTransport::connect_with_config(&parsed, &SocketConfig::default())
        .expect("tcps connect (handshake is lazy — not yet triggered)");

    Transport::close(&mut client);
    Transport::close(&mut client);
    assert!(!Transport::is_alive(&client), "close() must mark dead");

    drop(client);
    let _ = srv.join();
}
