//! TLS-side keepalive.
//!
//! [`RtspClient`](tst_rtp::RtspClient) shares its stream with the
//! auto-keepalive thread as an `Arc<Mutex<Stream>>`, so the thread (spawned
//! by
//! [`spawn_keepalive_if_needed`](tst_rtp::RtspClient::spawn_keepalive_if_needed)
//! from the builder) works uniformly across `Stream::Plain` and
//! `Stream::Tls`. A `TcpStream::try_clone`-based thread would be silently a
//! no-op for TLS because rustls `ClientConnection` isn't clonable.
//!
//! Drives a real in-process `rtsps://` server using the
//! [`SelfSignedCert`](crate::fixtures::tls_certs::SelfSignedCert) rcgen
//! fixture (cert + key + matching root for the client trust store), so
//! the keepalive thread is exercised over a live TLS session — verifying
//! both the client's TLS keepalive (the `Arc<Mutex<Stream>>` share) and
//! the server's TLS session loop handle periodic GET_PARAMETER pings.

// Drives a real in-process `rtsps://` server via `RtspServerBuilder::tls_cert`
// — requires the server's TLS acceptor, not just the client's `tls` feature.
#![cfg(feature = "rtsp-server-tls")]

use std::time::Duration;

use secrecy::SecretString;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClientBuilder, RtspServerBuilder};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Connect to an `rtsps://` server and verify the keepalive thread spawns
/// over TLS (a `try_clone`-based thread would silently no-op, `try_clone`
/// returning `Unsupported`). [`RtspClient::is_session_alive`](tst_rtp::RtspClient::is_session_alive)
/// returning `true` after the keepalive thread has been running for a
/// few hundred ms is the observable check — if the thread had failed to
/// spawn or had immediately errored on a control-TCP write, the
/// `session_dead` flag would have flipped.
///
/// Builds the client through [`RtspClientBuilder`] (rather than
/// [`RtspClient::connect`](tst_rtp::RtspClient::connect) +
/// `spawn_keepalive_if_needed`) because the builder is the public
/// keepalive-spawn path the public-API consumers will hit.
#[test]
fn rtsps_keepalive_thread_spawns_on_connect() {
    // Fresh self-signed cert (SANs: localhost + 127.0.0.1) from the rcgen
    // fixture; the tempdir lives as long as `certs`.
    let certs = crate::fixtures::tls_certs::SelfSignedCert::generate();
    let mut sb = RtspServerBuilder::new("rtsps://127.0.0.1:0").unwrap();
    sb.tls_cert(certs.cert_path.clone(), certs.key_path.clone());
    let server = sb.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsps://127.0.0.1:{port}/live");

    // Build a client trusting the fixture's self-signed cert as a root.
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut certs.root_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }

    let mut client = RtspClientBuilder::new(&url)
        .unwrap()
        .keepalive_interval(Duration::from_millis(50))
        .auth("unused", SecretString::new("unused".into()))
        .tls_root_certs(roots)
        .connect()
        .unwrap();

    // Drive an OPTIONS round trip so the connection is fully live, then
    // wait a few keepalive cycles. The keepalive thread shares
    // `Arc<Mutex<Stream>>` with the main client; a TLS
    // write failure would flip `session_dead` and make
    // `is_session_alive` return false.
    client.options().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        client.is_session_alive(),
        "TLS keepalive thread should hold the rtsps:// session alive; \
         pre-T21 this assertion would fail because the thread couldn't \
         clone the TLS stream"
    );

    drop(client);
    server.stop().ok();
}
