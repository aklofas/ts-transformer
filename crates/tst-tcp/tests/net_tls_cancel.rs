//! `tcps://` receive path: a cancel reaches a recv parked inside a TLS read,
//! and the one-socket-read-at-a-time TLS read still delivers whole plaintext
//! and a clean EOF on `close_notify`.
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
