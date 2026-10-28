//! The `rtsps://` client handshake ends: a peer FIN is an error, and the
//! connect timeout bounds the whole handshake, while a socket read timeout
//! inside it is only a poll tick.

#![cfg(feature = "tls")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::fixtures::raw_rtsp::scaled;
use tst_rtp::{RtspClient, RtspClientBuilder, RtspError};

/// Accepts one connection and reads exactly one TLS record (5-byte header +
/// its length) — the ClientHello — then runs `then` on the socket.
///
/// Reading the whole record matters: a peer that closed with unread bytes
/// would send RST, which already takes the error path.
fn tls_peer(then: impl FnOnce(TcpStream) + Send + 'static) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut header = [0u8; 5];
        s.read_exact(&mut header).unwrap();
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).unwrap();
        then(s);
    });
    (port, peer)
}

/// Run `connect` on a worker; `None` if it has not returned after `limit`.
fn connect_with_watchdog(
    b: RtspClientBuilder,
    limit: Duration,
) -> Option<Result<RtspClient, RtspError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(b.connect());
    });
    rx.recv_timeout(limit).ok()
}

/// A FIN after the ClientHello ends the handshake: rustls reports it only as
/// `read_tls` returning 0 (rustls 0.23 `ConnectionCommon::read_tls`), and the
/// loop must turn that into an error instead of polling again.
#[test]
fn rtsps_peer_closing_after_client_hello_is_a_tls_error() {
    let (port, peer) = tls_peer(drop); // drop = FIN
    let b = RtspClientBuilder::new(&format!("rtsps://127.0.0.1:{port}/x"))
        .unwrap()
        .no_auto_keepalive(true);
    let got = connect_with_watchdog(b, scaled(Duration::from_secs(10)))
        .expect("connect did not return: the handshake loop is spinning on EOF");
    match got {
        Err(RtspError::Tls(msg)) => assert!(msg.contains("closed"), "{msg}"),
        other => panic!("expected RtspError::Tls, got {other:?}"),
    }
    peer.join().unwrap();
}

/// A peer that keeps every read productive (one byte of a 16 KiB handshake
/// record every 50 ms) is stopped by the connect budget.
#[test]
fn rtsps_trickled_handshake_hits_the_connect_timeout() {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (port, peer) = tls_peer(move |mut s| {
        let _ = s.write_all(&[0x16, 0x03, 0x03, 0x40, 0x00]);
        while done_rx.recv_timeout(Duration::from_millis(50)).is_err() {
            if s.write_all(&[0]).is_err() {
                return;
            }
        }
    });
    let b = RtspClientBuilder::new(&format!("rtsps://127.0.0.1:{port}/x"))
        .unwrap()
        .no_auto_keepalive(true)
        .connect_timeout(Duration::from_millis(500));
    // Not scaled: a handshake that ignored the 500 ms budget and fell back
    // to the builder's 10 s default must miss this watchdog at any scale.
    let got = connect_with_watchdog(b, Duration::from_secs(8))
        .expect("connect did not return: no overall handshake deadline");
    let _ = done_tx.send(());
    assert!(
        matches!(got, Err(RtspError::Io(std::io::ErrorKind::TimedOut))),
        "{got:?}"
    );
    peer.join().unwrap();
}

/// A server that sends ServerHello later than one `read_timeout` (100 ms) is
/// still a successful handshake: a socket-read timeout inside the handshake
/// is a poll tick, not a failure.
#[test]
fn rtsps_handshake_survives_a_server_slower_than_read_timeout() {
    let certs = crate::fixtures::tls_certs::SelfSignedCert::generate();
    let key_pem = std::fs::read(&certs.key_path).unwrap();
    let roots =
        tst_core::net::tls::load_root_store(certs.root_pem.as_bytes()).expect("trust bundle");
    let config = std::sync::Arc::new(
        tst_core::net::tls::server_config_from_pem(certs.root_pem.as_bytes(), &key_pem).unwrap(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            // Three client read timeouts pass before the ServerHello leaves.
            std::thread::sleep(Duration::from_millis(300));
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut conn = rustls::ServerConnection::new(config).unwrap();
            while conn.is_handshaking() {
                if conn.complete_io(&mut sock).is_err() {
                    return;
                }
            }
            let _ = done_rx.recv_timeout(scaled(Duration::from_secs(20)));
        }
    });

    let client = RtspClientBuilder::new(&format!("rtsps://127.0.0.1:{port}/x"))
        .unwrap()
        .no_auto_keepalive(true)
        .tls_root_certs(roots);
    let got = connect_with_watchdog(client, scaled(Duration::from_secs(10))).expect("returned");
    assert!(got.is_ok(), "{got:?}");
    let _ = done_tx.send(());
    drop(got);
    server.join().unwrap();
}
