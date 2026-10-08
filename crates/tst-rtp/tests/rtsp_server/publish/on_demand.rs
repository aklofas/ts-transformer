//! On-demand publish mounts: with `accept_unregistered_publishers(true)` an
//! ANNOUNCE on an unregistered path creates a publish mount and queues its
//! handle for `RtspServer::next_publisher`.

use std::time::{Duration, Instant};

use tst_core::transport::RecvTransport;
use tst_rtp::{RtspServer, RtspServerBuilder, RtspServerError};

use crate::fixtures::raw_rtsp_publisher::*;

fn on_demand_server() -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    let server = b.build().unwrap();
    server.start().unwrap();
    server
}

/// An ANNOUNCE on an unknown path surfaces through `next_publisher`, and the
/// queued handle's transport carries the publisher's TS.
#[test]
fn announce_on_an_unknown_path_queues_a_mount_for_next_publisher() {
    let server = on_demand_server();
    let port = server.local_addr().unwrap().port();

    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "nothing published yet"
    );

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/anything", SDP_MP2T), 200);
    let h = server
        .next_publisher(Duration::from_secs(5))
        .unwrap()
        .expect("the ANNOUNCE queued a mount");
    assert_eq!(h.mount_path(), "/anything");
    assert!(h.publisher().is_some(), "the announcing publisher holds it");
    let mut app = h.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));

    let (status, ch) = p.setup_interleaved("/anything", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.unwrap();
    assert_eq!(p.record("/anything"), 200);
    let bundles = ts_fixture_packets(4);
    for (i, b) in bundles.iter().enumerate() {
        p.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, 0xE, 33, b));
    }
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(server.stats().mounts, 1);
    assert_eq!(p.teardown("/anything"), 200);

    // A later publisher on the same path reuses the mount: nothing new queued.
    let mut p2 = RawPublisher::connect(port);
    assert_eq!(p2.announce("/anything", SDP_MP2T), 200);
    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "an existing on-demand mount is not queued twice"
    );
    assert_eq!(server.stats().mounts, 1);
    assert_eq!(p2.teardown("/anything"), 200);

    server.stop().unwrap();
    assert!(matches!(
        server.next_publisher(Duration::from_millis(200)),
        Err(RtspServerError::Shutdown)
    ));
}

/// Without the flag an unknown path is still 404 and `next_publisher` times
/// out empty.
#[test]
fn unknown_path_is_404_without_the_flag() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/anything", SDP_MP2T), 404);
    assert!(
        server
            .next_publisher(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    assert_eq!(server.stats().mounts, 0);
    server.stop().unwrap();
}

/// `stop()` wakes a `next_publisher` parked on another thread with
/// `Shutdown`, well before its own timeout.
#[test]
fn stop_wakes_a_parked_next_publisher() {
    let server = on_demand_server();
    std::thread::scope(|s| {
        let parked = s.spawn(|| {
            let t0 = Instant::now();
            (server.next_publisher(Duration::from_secs(60)), t0.elapsed())
        });
        std::thread::sleep(Duration::from_millis(200));
        server.stop().unwrap();
        let (r, waited) = parked.join().unwrap();
        assert!(
            matches!(r, Err(RtspServerError::Shutdown)),
            "got {:?}",
            r.as_ref().err()
        );
        assert!(waited < Duration::from_secs(30), "parked for {waited:?}");
    });
}
