//! The mount outlives its publishers: after a TEARDOWN (or a dropped
//! connection) the mount idles and a new ANNOUNCE takes it, feeding the
//! SAME application transport.

use std::time::{Duration, Instant};

use tst_core::transport::RecvTransport;
use tst_rtp::{PublishMountHandle, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

/// ANNOUNCE + interleaved SETUP + RECORD, then `frames` bundles.
fn publish(port: u16, bundles: &[Vec<u8>], ssrc: u32) -> RawPublisher {
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, ch) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.unwrap();
    assert_eq!(p.record("/pub"), 200);
    for (i, b) in bundles.iter().enumerate() {
        p.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, ssrc, 33, b));
    }
    p
}

/// Poll `cond` every 100 ms until it holds or `ceiling` passes.
fn wait_for(ceiling: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + ceiling;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

fn publisher_gone(mount: &PublishMountHandle) -> bool {
    mount.publisher().is_none()
}

#[test]
fn second_publisher_after_teardown_feeds_the_same_app_transport() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));

    let bundles = ts_fixture_packets(6);
    let mut a = publish(port, &bundles[..3], 0xA);
    assert_eq!(a.teardown("/pub"), 200);
    assert_eq!(mount.generation(), 1);
    assert!(mount.publisher().is_none());

    let mut b = publish(port, &bundles[3..], 0xB);
    assert!(mount.publisher().is_some());
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(mount.generation(), 1, "B is still publishing");
    assert_eq!(b.teardown("/pub"), 200);
    assert_eq!(mount.generation(), 2);
    server.stop().ok();
}

#[test]
fn publisher_dropping_its_connection_frees_the_mount() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let p = publish(port, &ts_fixture_packets(2), 0xC);
    assert!(mount.publisher().is_some());
    drop(p); // no TEARDOWN
    assert!(
        wait_for(Duration::from_secs(3), || publisher_gone(&mount)),
        "publisher slot still held after the connection dropped"
    );
    assert_eq!(mount.generation(), 1);
    // and the mount takes a new publisher
    let mut p2 = publish(port, &[], 0xD);
    assert_eq!(p2.teardown("/pub"), 200);
    server.stop().ok();
}
