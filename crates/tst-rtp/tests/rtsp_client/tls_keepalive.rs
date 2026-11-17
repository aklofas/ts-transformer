//! TLS-side keepalive.
//!
//! [`RtspClient`](tst_rtp::RtspClient)'s keepalive pings (enabled by
//! [`spawn_keepalive_if_needed`](tst_rtp::RtspClient::spawn_keepalive_if_needed)
//! from the builder) are written by the client's own thread, which owns
//! the control connection whether it is plain TCP or TLS.
//!
//! Drives a real in-process `rtsps://` server using the
//! [`SelfSignedCert`](crate::fixtures::tls_certs::SelfSignedCert) rcgen
//! fixture (cert + key + matching root for the client trust store), so
//! the keepalive pings are exercised over a live TLS session — verifying
//! both the client's TLS keepalive and that the server's TLS session loop
//! handles periodic OPTIONS pings.

// Drives a real in-process `rtsps://` server via `RtspServerBuilder::tls_cert`
// — requires the server's TLS acceptor, not just the client's `tls` feature.
#![cfg(feature = "rtsp-server-tls")]

use std::time::Duration;

use crate::fixtures::raw_rtsp::make_muxer_cfg;
use secrecy::SecretString;
use tst_rtp::{RtspClientBuilder, RtspServerBuilder};

/// Connect to an `rtsps://` server and verify the keepalive pings run
/// over TLS. [`RtspClient::is_session_alive`](tst_rtp::RtspClient::is_session_alive)
/// returning `true` after the pings have run for a few hundred ms is the
/// observable check — had a ping failed on the control connection, the
/// `session_dead` flag would have flipped.
///
/// Builds the client through [`RtspClientBuilder`] (rather than
/// [`RtspClient::connect`](tst_rtp::RtspClient::connect) +
/// `spawn_keepalive_if_needed`) because the builder is the public
/// keepalive-spawn path the public-API consumers will hit.
#[test]
fn rtsps_keepalive_pings_keep_the_session_alive() {
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
    let roots =
        tst_core::net::tls::load_root_store(certs.root_pem.as_bytes()).expect("trust bundle");

    let mut client = RtspClientBuilder::new(&url)
        .unwrap()
        .keepalive_interval(Duration::from_millis(50))
        .auth("unused", SecretString::new("unused".into()))
        .tls_root_certs(roots)
        .connect()
        .unwrap();

    // Drive an OPTIONS round trip so the connection is fully live, then
    // wait a few keepalive cycles. A TLS write failure would flip
    // `session_dead` and make `is_session_alive` return false.
    client.options().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        client.is_session_alive(),
        "TLS keepalive pings should hold the rtsps:// session alive"
    );

    drop(client);
    server.stop().ok();
}
