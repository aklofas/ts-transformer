//! One publisher per mount: a second ANNOUNCE on a live mount is 403 and
//! does not disturb the first publisher.

use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp_publisher::*;

#[test]
fn second_announce_on_a_live_mount_is_403_and_first_keeps_publishing() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));

    let mut a = RawPublisher::connect(port);
    assert_eq!(a.announce("/pub", SDP_MP2T), 200);
    let (status, ch) = a.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.unwrap();
    assert_eq!(a.record("/pub"), 200);

    let mut b = RawPublisher::connect(port);
    assert_eq!(b.announce("/pub", SDP_MP2T), 403);
    assert_eq!(mount.generation(), 0, "a refused ANNOUNCE ends nothing");

    let bundles = ts_fixture_packets(5);
    for (i, bundle) in bundles.iter().enumerate() {
        a.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, 1, 33, bundle));
    }
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(a.teardown("/pub"), 200);
    server.stop().ok();
}
