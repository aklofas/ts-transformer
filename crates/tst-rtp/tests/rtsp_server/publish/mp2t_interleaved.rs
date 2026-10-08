//! MP2T publisher over TCP-interleaved: the application transport and a
//! PLAY reader both get the publisher's TS byte-identical.

use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::{RtspClient, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

/// Read from `t` until `want` bytes of payload have arrived; returns their
/// concatenation. The server re-chunks a publisher's packets into
/// 1316-byte bundles, so deliveries are compared as one byte stream, not
/// frame by frame.
fn read_stream(t: &mut impl RecvTransport, want: usize, who: &str) -> Vec<u8> {
    let mut got = Vec::with_capacity(want);
    let mut buf = vec![0u8; 65536];
    while got.len() < want {
        let n = t
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("{who} after {} bytes: {e:?}", got.len()));
        assert!(n <= 1316, "{who}: a {n}-byte bundle exceeds 7 TS packets");
        got.extend_from_slice(&buf[..n]);
    }
    got
}

/// MP2T publisher over TCP-interleaved → the application `RtpRecvTransport`
/// gets the TS byte-identical; a PLAY reader through our own client gets it
/// too. Each publisher packet carries eight fixture bundles (several 1316-byte
/// bundles' worth), so the server's re-chunking is on the path.
#[test]
fn mp2t_interleaved_publisher_reaches_app_and_reader() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));

    // Reader first, so it is subscribed to the fanout before the publisher sends.
    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/pub?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let sess = reader.setup_mp2t_auto(&sdp).unwrap();
    let mut reader_t = sess.into_recv_transport();
    reader_t.set_recv_timeout(Some(Duration::from_secs(5)));
    reader.play().unwrap();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, ch) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.expect("server allocated an interleaved channel");
    assert_eq!(p.record("/pub"), 200);
    assert!(mount.publisher().is_some());

    let bundles = ts_fixture_packets(160);
    let packets: Vec<Vec<u8>> = bundles.chunks(8).map(|group| group.concat()).collect();
    assert_eq!(packets.len(), 20);
    assert!(
        packets.iter().any(|p| p.len() > 1316),
        "re-chunking exercised"
    );
    for (i, payload) in packets.iter().enumerate() {
        p.send_frame(
            ch,
            &rtp_wrap(i as u16, i as u32 * 3003, 0xABCD, 33, payload),
        );
    }
    let sent: Vec<u8> = packets.concat();

    let got = read_stream(&mut app, sent.len(), "app");
    assert!(got == sent, "app stream differs from the sent TS");
    let got = read_stream(&mut reader_t, sent.len(), "reader");
    assert!(got == sent, "reader stream differs from the sent TS");
    assert_eq!(mount.stats().rtp_packets_received, 20);
    assert_eq!(mount.peer_count(), 1);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
