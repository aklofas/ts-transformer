//! CORR-06: the `rtsps://` TLS handshake has a deadline. A connection that
//! is accepted but never sends a ClientHello used to hold its
//! `active_sessions` slot until the peer went away — `max_sessions` silent
//! connects (no auth needed) wedged the server at its cap for good.

#![cfg(feature = "rtsp-server-tls")]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::RtspServerBuilder;

use crate::fixtures::tls_certs::SelfSignedCert;

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().expect("MuxerConfig builds")
}

/// Two raw TCP connects that never speak TLS fill `max_sessions(2)`. The
/// handshake deadline (1 s here) must release both slots; a real
/// `rtsps://` client then gets a slot and completes OPTIONS. Before the fix
/// the two handshake tasks parked in `cfg.accept(tcp).await` forever and
/// the count stayed pinned at 2.
#[test]
fn silent_connects_release_their_slots_after_the_handshake_deadline() {
    let certs = SelfSignedCert::generate();
    let mut b = RtspServerBuilder::new("rtsps://127.0.0.1:0").expect("URL parse");
    b.tls_cert(certs.cert_path.clone(), certs.key_path.clone());
    b.max_sessions(2);
    b.tls_handshake_timeout(Duration::from_secs(1));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    let addr = server.local_addr().expect("local_addr");

    // Two silent connects: accepted, slot reserved, no ClientHello ever
    // sent. Held in `_silent` so the kernel keeps them open all test long.
    let _silent: Vec<TcpStream> = (0..2)
        .map(|_| TcpStream::connect(addr).expect("raw connect"))
        .collect();

    // The accept loop is async — wait for both slots to be reserved.
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.stats().active_sessions < 2 {
        assert!(
            Instant::now() < deadline,
            "the two silent connects were never accepted"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The 1 s handshake deadline must release both slots (generous slack).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut count = usize::MAX;
    while Instant::now() < deadline {
        count = server.stats().active_sessions;
        if count == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        count, 0,
        "silent TLS connects still hold {count} session slot(s) after the 1 s handshake deadline"
    );

    // A real client now gets a slot and completes the encrypted OPTIONS
    // (same trust-store wiring as `tls.rs::rtsps_handshake_succeeds_with_trusted_root`).
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut certs.root_pem.as_bytes()) {
        roots
            .add(cert.expect("PEM cert parse"))
            .expect("add to root store");
    }
    let url = format!("rtsps://127.0.0.1:{}/live", addr.port());
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .expect("client builder")
        .tls_root_certs(roots)
        .connect()
        .expect("rtsps connect once the silent connects' slots were released");
    client.options().expect("OPTIONS over TLS");
    server.stop().ok();
}
