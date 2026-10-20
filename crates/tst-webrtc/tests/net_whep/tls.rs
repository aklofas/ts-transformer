//! `wheps://`: HTTPS signalling over rustls, with the bearer token.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tst_webrtc::{MountConfig, WhepServer, WhepServerError};

use crate::fixtures::tls_certs::SelfSignedCert;
use crate::fixtures::{STOP_REPORT, TestViewer, https, tls_client, whep_post_tls};

const CONNECT: Duration = Duration::from_secs(5);

#[test]
fn a_tls_post_is_201_and_the_viewer_connects() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    let (status, headers, answer) =
        whep_post_tls(srv.local_addr(), &certs.root_pem, "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    assert_eq!(headers["content-type"], "application/sdp");
    assert!(headers["location"].starts_with("/cam/sessions/"));
    v.accept_answer(&answer);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );
}

#[test]
fn a_plaintext_post_to_a_tls_port_fails_and_the_next_tls_post_works() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let offer = TestViewer::new().offer(true);

    // Plaintext HTTP is not a TLS ClientHello: the server drops the
    // connection without an HTTP answer.
    let mut plain = TcpStream::connect(srv.local_addr()).unwrap();
    plain
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _ = plain.write_all(
        format!(
            "POST /cam HTTP/1.1\r\nHost: x\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{offer}",
            offer.len()
        )
        .as_bytes(),
    );
    let mut got = Vec::new();
    let _ = plain.read_to_end(&mut got);
    assert!(
        !got.starts_with(b"HTTP/"),
        "no HTTP answer in plaintext: {got:?}"
    );

    let (status, _, body) = whep_post_tls(srv.local_addr(), &certs.root_pem, "/cam", &offer, None);
    assert_eq!(status, 201, "{body}");
}

#[test]
fn a_tls_client_that_does_not_trust_the_certificate_gets_nothing() {
    let certs = SelfSignedCert::generate();
    let other = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut s = tls_client(srv.local_addr(), &other.root_pem);
    let sent = s.write_all(b"GET / HTTP/1.1\r\n\r\n");
    let mut sink = Vec::new();
    assert!(sent.is_err() || s.read_to_end(&mut sink).is_err());
    // The server is still serving.
    let offer = TestViewer::new().offer(true);
    let (status, _, _) = whep_post_tls(srv.local_addr(), &certs.root_pem, "/cam", &offer, None);
    assert_eq!(status, 201);
}

#[test]
fn a_bad_certificate_path_is_refused_at_bind_and_the_port_is_free() {
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let missing = std::env::temp_dir().join("tst-webrtc-no-such-cert.pem");
    let url = format!(
        "wheps://127.0.0.1:{port}?cert={0}&key={0}",
        missing.display()
    );
    assert!(matches!(
        WhepServer::bind(&url),
        Err(WhepServerError::Tls(_))
    ));
    TcpListener::bind(("127.0.0.1", port)).expect("the refused bind holds no port");
}

#[test]
fn a_key_file_that_is_not_a_key_is_refused_at_bind() {
    // The certificate file given as the key.
    let certs = SelfSignedCert::generate();
    let url = format!(
        "wheps://127.0.0.1:0?cert={0}&key={0}",
        certs.cert_path.display()
    );
    assert!(matches!(
        WhepServer::bind(&url),
        Err(WhepServerError::Tls(_))
    ));
}

#[test]
fn the_bearer_token_is_enforced_over_tls() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("&token=abc")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let addr = srv.local_addr();
    let offer = TestViewer::new().offer(true);
    let (none, headers, _) = whep_post_tls(addr, &certs.root_pem, "/cam", &offer, None);
    assert_eq!(none, 401);
    assert_eq!(headers["www-authenticate"], "Bearer");
    let (wrong, _, _) = whep_post_tls(addr, &certs.root_pem, "/cam", &offer, Some("abd"));
    assert_eq!(wrong, 401);
    let (right, headers, _) = whep_post_tls(addr, &certs.root_pem, "/cam", &offer, Some("abc"));
    assert_eq!(right, 201);
    let location = &headers["location"];
    let (delete, _, _) = https(addr, &certs.root_pem, "DELETE", location, &[], b"");
    assert_eq!(delete, 401);
    let auth = [("Authorization", "Bearer abc")];
    let (delete, _, _) = https(addr, &certs.root_pem, "DELETE", location, &auth, b"");
    assert_eq!(delete, 200);
    assert_eq!(srv.stats().sessions_rejected_auth, 3);
}

/// A good TLS post is answered 201 and its viewer connects while `held`
/// is still open.
fn good_post_succeeds_while_open(srv: &WhepServer, certs: &SelfSignedCert, held: &TcpStream) {
    let _ = held;
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    let (status, _, answer) =
        whep_post_tls(srv.local_addr(), &certs.root_pem, "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );
}

#[test]
fn a_client_that_connects_and_never_writes_does_not_block_the_next_post() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let idle = TcpStream::connect(srv.local_addr()).unwrap();
    good_post_succeeds_while_open(&srv, &certs, &idle);
}

#[test]
fn a_client_that_stalls_inside_the_client_hello_does_not_block_the_next_post() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    // The first five bytes of a TLS record: handshake type, version, a
    // length that promises more than is sent.
    let mut stalled = TcpStream::connect(srv.local_addr()).unwrap();
    stalled.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00]).unwrap();
    good_post_succeeds_while_open(&srv, &certs, &stalled);
}

/// The longest a trickling client keeps sending, so a server that never
/// lets go of it fails the test instead of hanging it.
const TRICKLE_MAX: Duration = Duration::from_secs(30);

/// A client that sends the header of a TLS record promising 16 KiB, then
/// one byte every 50 ms until the returned sender is dropped (or
/// [`TRICKLE_MAX`]). `wrote` receives a message after each byte.
fn trickle(addr: SocketAddr, wrote: Sender<()>) -> (Sender<()>, JoinHandle<()>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]).unwrap();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let mut sent = Duration::ZERO;
        while sent < TRICKLE_MAX {
            // The receive doubles as the pacing wait and the stop signal.
            match stop_rx.recv_timeout(Duration::from_millis(50)) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            sent += Duration::from_millis(50);
            if s.write_all(&[0]).is_err() {
                return;
            }
            let _ = wrote.send(());
        }
    });
    (stop_tx, handle)
}

#[test]
fn a_client_that_trickles_a_record_does_not_hold_the_server_past_the_deadline() {
    let certs = SelfSignedCert::generate();
    let srv = WhepServer::bind(&certs.url("")).unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let addr = srv.local_addr();

    // Connected before the good POST, so the accept thread serves it first
    // and must give it up at the request deadline.
    let (wrote, _) = mpsc::channel();
    let (first, first_thread) = trickle(addr, wrote);
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    let (status, _, answer) = whep_post_tls(addr, &certs.root_pem, "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    assert!(
        v.pump(|v| v.connected && v.dc_open, CONNECT),
        "{}",
        v.trace()
    );
    drop(first);
    first_thread.join().unwrap();

    // A second trickler is being served when the server stops: stop still
    // joins the accept thread.
    let (wrote, wrote_rx) = mpsc::channel();
    let (second, second_thread) = trickle(addr, wrote);
    for _ in 0..4 {
        wrote_rx
            .recv_timeout(STOP_REPORT)
            .expect("the trickler writes");
    }
    srv.stop();
    assert!(srv.threads_joined_for_test());
    drop(second);
    second_thread.join().unwrap();
}
