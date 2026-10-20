//! `wheps://`: HTTPS signalling over rustls, with the bearer token.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use tst_webrtc::{MountConfig, WhepServer, WhepServerError};

use crate::fixtures::tls_certs::SelfSignedCert;
use crate::fixtures::{TestViewer, https, tls_client, whep_post_tls};

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
