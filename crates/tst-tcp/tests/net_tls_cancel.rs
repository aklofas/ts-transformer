//! `tcps://` receive path: a cancel reaches a recv parked inside a TLS read,
//! and the one-socket-read-at-a-time TLS read still delivers whole plaintext
//! and a clean EOF on `close_notify`.
//!
//! Accepted-side send path: a cancel reaches a send parked in the lazy TLS
//! server handshake; a silent peer is `Backpressure`, a peer EOF
//! mid-handshake is `Broken`.
//!
//! Only compiled with the `tls` feature (the crate's default).

#![cfg(feature = "tls")]

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportError};
use tst_tcp::url::TcpUrl;
use tst_tcp::{TcpCancelHandle, TcpListener, TcpTransport};
use tst_test_helpers::timing::scaled;

/// Self-signed `localhost` certificate + key written to a temp directory; the
/// certificate doubles as the caller's CA bundle.
fn gen_cert() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert generation");
    let dir = tempfile::tempdir().expect("create tempdir for TLS fixture");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert.pem");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key.pem");
    (dir, cert_path, key_path)
}

fn tls_listener(cert: &std::path::Path, key: &std::path::Path) -> TcpListener {
    TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert.display(),
        key.display(),
    ))
    .expect("TLS listener bind")
}

fn tls_caller(port: u16, ca: &std::path::Path) -> TcpTransport {
    let url = format!("tcps://localhost:{port}?ca={}", ca.display());
    let parsed = TcpUrl::parse(&url).expect("URL parse");
    TcpTransport::connect_with_config(&parsed, &parsed.config).expect("tcps connect")
}

/// Spin until `cond` holds, failing the test after `limit`.
fn wait_for(what: &str, limit: Duration, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::yield_now();
        thread::park_timeout(Duration::from_millis(5));
    }
}

/// Run `serve` on the accepted connection in a worker; the worker hands its
/// cancel handle back first so a hung receive can be cancelled before the
/// test fails, instead of leaking a parked thread.
fn serve_one<T: Send + 'static>(
    listener: TcpListener,
    serve: impl FnOnce(&mut TcpTransport) -> T + Send + 'static,
) -> (mpsc::Receiver<TcpCancelHandle>, mpsc::Receiver<T>) {
    let (htx, hrx) = mpsc::channel();
    let (rtx, rrx) = mpsc::channel();
    thread::spawn(move || {
        let mut conn = listener.accept_blocking().expect("server accept");
        let _ = htx.send(conn.cancel_handle());
        let _ = rtx.send(serve(&mut conn));
    });
    (hrx, rrx)
}

fn await_result<T>(handles: &mpsc::Receiver<TcpCancelHandle>, results: &mpsc::Receiver<T>) -> T {
    match results.recv_timeout(scaled(Duration::from_secs(10))) {
        Ok(r) => r,
        Err(e) => {
            if let Ok(h) = handles.try_recv() {
                h.cancel();
            }
            panic!("server side did not finish: {e}");
        }
    }
}

/// A peer dripping one byte of a TLS record every 50 ms must not hide a cancel
/// from a parked recv on the accepted transport.
#[test]
fn cancel_wakes_a_recv_parked_on_a_dripping_tls_peer() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let addr = listener.local_addr().expect("local_addr");

    // Raw client: a TLS handshake record header announcing 255 bytes, then
    // the body one byte at a time, so the server's TLS read always has a
    // byte arriving before its socket poll interval runs out.
    let stop = Arc::new(AtomicBool::new(false));
    let dripped = Arc::new(AtomicUsize::new(0));
    let drip = {
        let (stop, dripped) = (stop.clone(), dripped.clone());
        thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("raw connect");
            s.write_all(&[0x16, 0x03, 0x01, 0x00, 0xff])
                .expect("record header");
            let end = Instant::now() + Duration::from_secs(60);
            while !stop.load(Ordering::Acquire) && Instant::now() < end {
                if s.write_all(&[0x00]).is_err() {
                    break;
                }
                dripped.fetch_add(1, Ordering::AcqRel);
                thread::park_timeout(Duration::from_millis(50));
            }
        })
    };

    let mut conn = listener.accept_blocking().expect("server accept");
    let h = conn.cancel_handle();
    let about_to_recv = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let about_to_recv = about_to_recv.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 1316];
            about_to_recv.store(true, Ordering::Release);
            let _ = tx.send(conn.recv_bytes(&mut buf));
        })
    };

    wait_for(
        "the worker to reach recv",
        scaled(Duration::from_secs(5)),
        || about_to_recv.load(Ordering::Acquire),
    );
    let base = dripped.load(Ordering::Acquire);
    wait_for(
        "20 more dripped bytes",
        scaled(Duration::from_secs(10)),
        || dripped.load(Ordering::Acquire) >= base + 20,
    );
    h.cancel();

    let result = rx.recv_timeout(scaled(Duration::from_secs(10)));
    stop.store(true, Ordering::Release);
    let _ = drip.join();
    let result = result.expect("the cancelled recv must return");
    let _ = worker.join();
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "{result:?}"
    );
}

/// Plaintext split across several records survives the stepwise read.
#[test]
fn several_records_round_trip_byte_exact() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let port = listener.local_addr().expect("local_addr").port();

    let (handles, results) = serve_one(listener, |conn| {
        let mut got = Vec::new();
        let mut buf = [0u8; 1316];
        while got.len() < 564 {
            match conn.recv_bytes(&mut buf) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e),
            }
        }
        Ok(got)
    });

    let mut client = tls_caller(port, &cert);
    let input: Vec<u8> = (0..564u32).map(|i| (i % 251) as u8).collect();
    for chunk in input.chunks(188) {
        client.send_bytes(chunk).expect("client send");
    }

    let got = await_result(&handles, &results).expect("server recv");
    assert_eq!(got, input);
    Transport::close(&mut client);
}

/// The peer's `close_notify` reads as a clean EOF.
#[test]
fn close_notify_is_a_clean_eof() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let port = listener.local_addr().expect("local_addr").port();

    let (handles, results) = serve_one(listener, |conn| {
        let mut buf = [0u8; 1316];
        conn.recv_bytes(&mut buf)
    });

    let mut client = tls_caller(port, &cert);
    Transport::close(&mut client);

    let result = await_result(&handles, &results);
    assert!(
        matches!(
            result,
            Err(TransportError::Broken {
                cause: BrokenCause::CleanEof,
                ..
            })
        ),
        "{result:?}"
    );
}

/// Sibling of the recv test: a peer dripping handshake bytes faster than
/// the socket tick must not hide a cancel from a parked SEND on the
/// accepted transport (the accepted side's handshake is lazy and runs
/// inside the first write).
#[test]
fn cancel_wakes_a_send_parked_on_a_dripping_tls_handshake() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let addr = listener.local_addr().expect("local_addr");

    // A raw dripping client: a handshake record header announcing 0x4000
    // bytes (the largest plaintext record), then one zero byte every 50 ms.
    // The record never completes within the test and the drip outlasts the
    // result wait at every timeout scale, so a send that ignores the cancel
    // until the handshake read finishes cannot return in time.
    let stop = Arc::new(AtomicBool::new(false));
    let dripped = Arc::new(AtomicUsize::new(0));
    let drip = {
        let (stop, dripped) = (stop.clone(), dripped.clone());
        thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("raw connect");
            s.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00])
                .expect("record header");
            let end = Instant::now() + scaled(Duration::from_secs(60));
            while !stop.load(Ordering::Acquire) && Instant::now() < end {
                if s.write_all(&[0x00]).is_err() {
                    break;
                }
                dripped.fetch_add(1, Ordering::AcqRel);
                thread::park_timeout(Duration::from_millis(50));
            }
        })
    };

    let mut conn = listener.accept_blocking().expect("server accept");
    let h = conn.cancel_handle();
    let about_to_send = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let about_to_send = about_to_send.clone();
        thread::spawn(move || {
            about_to_send.store(true, Ordering::Release);
            // A drip gap longer than the socket tick (a loaded runner) is a
            // zero-progress Backpressure; retry it so only the cancel ends
            // the send.
            let deadline = Instant::now() + scaled(Duration::from_secs(15));
            let r = loop {
                match conn.send_bytes(&[0x47; 188]) {
                    Err(TransportError::Backpressure { .. }) if Instant::now() < deadline => {}
                    other => break other,
                }
            };
            let _ = tx.send(r);
        })
    };

    wait_for(
        "the worker to reach send",
        scaled(Duration::from_secs(5)),
        || about_to_send.load(Ordering::Acquire),
    );
    let base = dripped.load(Ordering::Acquire);
    wait_for(
        "20 more dripped bytes",
        scaled(Duration::from_secs(10)),
        || dripped.load(Ordering::Acquire) >= base + 20,
    );
    h.cancel();

    let result = rx.recv_timeout(scaled(Duration::from_secs(10)));
    stop.store(true, Ordering::Release);
    let _ = drip.join();
    let result = result.expect("the cancelled send must return");
    let _ = worker.join();
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "{result:?}"
    );
}

/// A server certificate flight far larger than the socket buffers, read by
/// the peer a few KB per tick: every handshake write makes partial progress.
/// The cancel must reach the send within a tick or so, not after the whole
/// flight has drained.
#[test]
fn cancel_wakes_a_send_writing_a_slowly_read_tls_handshake_flight() {
    use std::io::Read;

    // The same certificate many times over: about 1.3 MB of Certificate
    // message. The server sends whatever chain it is given.
    const CHAIN_COPIES: usize = 3000;
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert generation");
    let dir = tempfile::tempdir().expect("create tempdir for TLS fixture");
    let cert = dir.path().join("chain.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, generated.cert.pem().repeat(CHAIN_COPIES)).expect("write chain.pem");
    std::fs::write(&key, generated.signing_key.serialize_pem()).expect("write key.pem");
    let flight_bytes = generated.cert.der().len() * CHAIN_COPIES;
    // A small send buffer on the accepted socket: loopback otherwise
    // autotunes it past the whole flight and the first write takes it all.
    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&sndbuf=8192&cert={}&key={}",
        cert.display(),
        key.display(),
    ))
    .expect("TLS listener bind");
    let addr = listener.local_addr().expect("local_addr");

    // A real ClientHello over a socket with a small receive buffer, so the
    // flight backs up into the server's send buffer at once.
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .expect("client socket");
    sock.set_recv_buffer_size(4096).expect("SO_RCVBUF");
    sock.connect(&addr.into()).expect("raw connect");
    let mut stream: TcpStream = sock.into();
    // The reader re-checks its stop flag at least this often.
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("SO_RCVTIMEO");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let mut hello =
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap())
            .expect("client connection");
    while hello.wants_write() {
        hello.write_tls(&mut stream).expect("ClientHello");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let drained = Arc::new(AtomicUsize::new(0));
    let reader = {
        let (stop, drained) = (stop.clone(), drained.clone());
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let end = Instant::now() + scaled(Duration::from_secs(60));
            while !stop.load(Ordering::Acquire) && Instant::now() < end {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        drained.fetch_add(n, Ordering::AcqRel);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
                thread::park_timeout(Duration::from_millis(50));
            }
        })
    };

    let mut conn = listener.accept_blocking().expect("server accept");
    let h = conn.cancel_handle();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + scaled(Duration::from_secs(30));
        let r = loop {
            match conn.send_bytes(&[0x47; 188]) {
                Err(TransportError::Backpressure { .. }) if Instant::now() < deadline => {}
                other => break other,
            }
        };
        let _ = tx.send((r, Instant::now()));
    });

    wait_for(
        "64 KB of the flight to drain",
        scaled(Duration::from_secs(10)),
        || drained.load(Ordering::Acquire) >= 64 * 1024,
    );
    let cancelled_at = Instant::now();
    h.cancel();
    let result = rx.recv_timeout(scaled(Duration::from_secs(40)));
    let drained_at_return = drained.load(Ordering::Acquire);
    stop.store(true, Ordering::Release);
    let _ = reader.join();
    let (result, returned_at) = result.expect("the cancelled send must return");
    let _ = worker.join();
    let latency = returned_at.saturating_duration_since(cancelled_at);
    assert!(
        drained_at_return < flight_bytes / 2,
        "the flight drained ({drained_at_return} of {flight_bytes} bytes) before the send returned"
    );
    assert!(
        latency < scaled(Duration::from_secs(1)),
        "the send returned {latency:?} after the cancel"
    );
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "{result:?}"
    );
}

/// The normal shape: server sends first against a real rustls client. The
/// stepped handshake completes across its flights; a tick without the
/// client's next flight is a retryable Backpressure, never a dead latch.
#[test]
fn accepted_send_before_any_recv_completes_the_handshake() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let port = listener.local_addr().expect("local_addr").port();

    let (handles, results) = serve_one(listener, |conn| {
        let deadline = Instant::now() + scaled(Duration::from_secs(10));
        loop {
            match conn.send_bytes(&[0x47; 188]) {
                Ok(()) => return Ok(Transport::is_alive(conn)),
                Err(TransportError::Backpressure { .. }) if Instant::now() < deadline => continue,
                Err(e) => return Err(e),
            }
        }
    });

    let mut client = tls_caller(port, &cert);
    let mut buf = [0u8; 1316];
    let n = client.recv_bytes(&mut buf).expect("client recv");
    assert_eq!(n, 188);
    assert_eq!(&buf[..188], &[0x47; 188][..]);
    assert!(await_result(&handles, &results).expect("server send"));
    Transport::close(&mut client);
}

/// A peer that connects and says nothing: zero-progress Backpressure after
/// one tick, transport alive, exactly as before this change.
#[test]
fn accepted_send_against_a_silent_peer_is_backpressure_and_stays_alive() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let addr = listener.local_addr().expect("local_addr");

    let _silent = TcpStream::connect(addr).expect("raw connect");
    let mut conn = listener.accept_blocking().expect("server accept");
    let r = conn.send_bytes(&[0x47; 188]);
    assert!(
        matches!(r, Err(TransportError::Backpressure { .. })),
        "{r:?}"
    );
    assert!(Transport::is_alive(&conn));
}

/// A peer that closes mid-handshake: Broken, dead, never ExplicitClose.
#[test]
fn accepted_send_peer_eof_mid_handshake_is_broken() {
    let (_dir, cert, key) = gen_cert();
    let listener = tls_listener(&cert, &key);
    let addr = listener.local_addr().expect("local_addr");

    let dropped = Arc::new(AtomicBool::new(false));
    let client = {
        let dropped = dropped.clone();
        thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("raw connect");
            s.write_all(&[0x16, 0x03, 0x01, 0x00, 0xff])
                .expect("record header");
            drop(s);
            dropped.store(true, Ordering::Release);
        })
    };

    let mut conn = listener.accept_blocking().expect("server accept");
    wait_for(
        "the raw client to drop its stream",
        scaled(Duration::from_secs(5)),
        || dropped.load(Ordering::Acquire),
    );
    let _ = client.join();
    let r = conn.send_bytes(&[0x47; 188]);
    assert!(matches!(r, Err(TransportError::Broken { .. })), "{r:?}");
    assert!(!Transport::is_alive(&conn));
    assert!(matches!(
        conn.send_bytes(&[0x47; 188]),
        Err(TransportError::Closed)
    ));
}
