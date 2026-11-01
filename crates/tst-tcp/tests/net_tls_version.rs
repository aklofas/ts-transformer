//! TLS loopback test: a TLS 1.2-only peer can complete a `tcps://` handshake.
//!
//! The documented contract is TLS 1.2 and 1.3. A client restricted to
//! TLS 1.2 fails with a protocol-version alert unless rustls is built with
//! its `tls12` feature.

#![cfg(feature = "tls")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;

use rustls::pki_types::ServerName;
use tst_core::transport::{RecvTransport, Transport};
use tst_tcp::TcpListener;

/// Test name contains "loopback" so nextest assigns it to the `network` group.
#[test]
fn tcps_tls12_only_client_loopback_handshake_and_roundtrip() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert generation");
    let dir = tempfile::tempdir().expect("tempdir for TLS fixture");
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert.pem");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key.pem");

    let listener = TcpListener::from_url(&format!(
        "tcps://127.0.0.1:0?listen=1&cert={}&key={}",
        cert_path.display(),
        key_path.display(),
    ))
    .expect("TLS listener bind");
    let port = listener.local_addr().expect("local_addr").port();

    let srv = thread::spawn(move || {
        let mut conn = listener.accept_blocking().expect("server accept");
        let mut buf = [0u8; 4];
        let n = conn.recv_bytes(&mut buf).expect("server recv");
        conn.send_bytes(&buf[..n]).expect("server echo");
    });

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).expect("trust anchor");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS12])
    .expect("TLS 1.2 versions")
    .with_root_certificates(roots)
    .with_no_client_auth();

    let name = ServerName::try_from("localhost").expect("server name");
    let conn = rustls::ClientConnection::new(Arc::new(config), name).expect("client conn");
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("tcp connect");
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    tls.write_all(b"ping")
        .expect("write completes the TLS 1.2 handshake");
    let mut echo = [0u8; 4];
    tls.read_exact(&mut echo).expect("read echo");
    assert_eq!(&echo, b"ping");
    assert_eq!(
        tls.conn.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_2)
    );
    srv.join().expect("server thread");
}
