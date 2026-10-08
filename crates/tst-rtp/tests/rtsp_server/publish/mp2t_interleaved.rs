//! MP2T publisher over TCP-interleaved: the application transport and a
//! PLAY reader both get the publisher's TS bundles byte-identical.

use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::{RtspClient, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

/// MP2T publisher over TCP-interleaved → the application `RtpRecvTransport`
/// gets the TS bundles byte-identical; a PLAY reader through our own client
/// gets them too.
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

    let bundles = ts_fixture_packets(20);
    for (i, b) in bundles.iter().enumerate() {
        p.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, 0xABCD, 33, b));
    }

    let mut buf = vec![0u8; 65536];
    for (i, b) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("app bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &b[..], "app bundle {i}");
    }
    for (i, b) in bundles.iter().enumerate() {
        let n = reader_t
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("reader bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &b[..], "reader bundle {i}");
    }
    assert_eq!(mount.stats().rtp_packets_received, 20);
    assert_eq!(mount.peer_count(), 1);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
