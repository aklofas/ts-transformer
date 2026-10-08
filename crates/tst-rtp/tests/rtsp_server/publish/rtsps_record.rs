//! Interleaved RECORD over `rtsps://`: the publisher's control channel and
//! `$` frames both ride the server's TLS session loop.

#![cfg(feature = "rtsp-server-tls")]

use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::RtspServerBuilder;

use crate::fixtures::raw_rtsp_publisher::*;
use crate::fixtures::tls_certs::SelfSignedCert;

#[test]
fn rtsps_interleaved_record_reaches_app() {
    let certs = SelfSignedCert::generate();
    let mut b = RtspServerBuilder::new("rtsps://127.0.0.1:0").unwrap();
    b.tls_cert(certs.cert_path.clone(), certs.key_path.clone());
    let server = b.build().unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut certs.root_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut p = RawPublisher::connect_tls(port, roots);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, ch) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.unwrap();
    assert_eq!(p.record("/pub"), 200);

    let bundles = ts_fixture_packets(3);
    for (i, bundle) in bundles.iter().enumerate() {
        p.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, 7, 33, bundle));
    }
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
