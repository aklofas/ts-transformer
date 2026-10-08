//! MP2T publisher over UDP: SETUP `mode=record` with `client_port`, RECORD,
//! then RTP datagrams to the server's `server_port`.

use std::net::UdpSocket;
use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp_publisher::*;

/// The server latches the full source address of the first RTP datagram
/// (not the announced `client_port`), so every datagram is sent from ONE
/// socket whose port differs from the announced one and all must be
/// admitted. (A second source being rejected is unit-tested in
/// `udp_ingest.rs`.) Loopback UDP may drop under load: ≥ 19 of 20 must
/// arrive, each one a byte-identical sent bundle.
#[test]
fn mp2t_udp_publisher_reaches_app() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_millis(1500)));

    // The announced client port pair (never sent from).
    let announced = UdpSocket::bind("127.0.0.1:0").unwrap();
    let announced_port = announced.local_addr().unwrap().port();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    assert_ne!(sender.local_addr().unwrap().port(), announced_port);

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, server_port) = p.setup_udp("/pub", "streamid=0", announced_port);
    assert_eq!(status, 200);
    let (server_rtp, server_rtcp) = server_port.expect("server_port in Transport");
    assert_eq!(server_rtcp, server_rtp + 1);
    assert_eq!(p.record("/pub"), 200);

    let bundles = ts_fixture_packets(20);
    for (i, b) in bundles.iter().enumerate() {
        sender
            .send_to(
                &rtp_wrap(i as u16, i as u32 * 3003, 0x5150, 33, b),
                ("127.0.0.1", server_rtp),
            )
            .unwrap();
    }

    let mut buf = vec![0u8; 65536];
    let mut got = 0;
    while let Ok(n) = app.recv_bytes(&mut buf) {
        assert!(
            bundles.iter().any(|b| b[..] == buf[..n]),
            "received bundle {got} is not one of the sent bundles"
        );
        got += 1;
        if got == bundles.len() {
            break;
        }
    }
    assert!(got >= 19, "only {got} of 20 datagrams reached the app");
    assert_eq!(mount.stats().malformed_packets, 0);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
