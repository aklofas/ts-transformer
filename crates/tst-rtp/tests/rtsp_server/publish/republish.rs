//! The mount outlives its publishers: after a TEARDOWN (or a dropped
//! connection) the mount idles and a new ANNOUNCE takes it, feeding the
//! SAME application transport.

use std::time::Duration;

use tst_core::transport::{RecvTransport, TransportError};
use tst_rtp::{PublishMountOptions, RtspServer, RtspServerBuilder};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::*;
use crate::fixtures::wait_for;

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

#[test]
fn second_publisher_after_teardown_feeds_the_same_app_transport() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

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
    // The publisher slot must free once the connection drops; panics
    // "condition not met within 3s" otherwise.
    wait_for(scaled(Duration::from_secs(3)), || {
        mount.publisher().is_none().then_some(())
    });
    assert_eq!(mount.generation(), 1);
    // and the mount takes a new publisher
    let mut p2 = publish(port, &[], 0xD);
    assert_eq!(p2.teardown("/pub"), 200);
    server.stop().ok();
}

fn server_with_default(end_on_exit: bool) -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.end_publish_mount_on_publisher_exit(end_on_exit);
    b.build().unwrap()
}

/// Read the `n` bundles, then expect the transport to end with `Closed`.
fn drain_then_closed(app: &mut tst_rtp::RtpRecvTransport, bundles: &[Vec<u8>]) {
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    let r = app.recv_bytes(&mut buf);
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
}

#[test]
fn publisher_exit_ends_the_app_transport_when_the_mount_opts_in() {
    let server = server_with_default(false);
    let mount = server
        .add_publish_mount_with(
            "/pub",
            PublishMountOptions::new().end_on_publisher_exit(true),
        )
        .unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

    let bundles = ts_fixture_packets(3);
    let mut a = publish(port, &bundles, 0xA);
    assert_eq!(a.teardown("/pub"), 200);
    drain_then_closed(&mut app, &bundles);

    let mut p = RawPublisher::connect(port);
    assert_ne!(p.announce("/pub", SDP_MP2T), 200, "a closed mount refuses");
    server.remove_mount("/pub").unwrap();
    server.stop().ok();
}

#[test]
fn a_mount_option_overrides_the_server_default_both_ways() {
    // Default ON, mount opts out: the app idles and a second publisher feeds it.
    let server = server_with_default(true);
    let mount = server
        .add_publish_mount_with(
            "/pub",
            PublishMountOptions::new().end_on_publisher_exit(false),
        )
        .unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    let bundles = ts_fixture_packets(4);
    let mut a = publish(port, &bundles[..2], 0xA);
    assert_eq!(a.teardown("/pub"), 200);
    let mut b = publish(port, &bundles[2..], 0xB);
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("{i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(b.teardown("/pub"), 200);
    server.stop().ok();

    // Default ON, plain add_publish_mount: inherits and ends.
    let server = server_with_default(true);
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    let bundles = ts_fixture_packets(2);
    let mut a = publish(port, &bundles, 0xC);
    assert_eq!(a.teardown("/pub"), 200);
    drain_then_closed(&mut app, &bundles);
    server.stop().ok();
}

#[test]
fn publisher_exit_reaches_the_demux_shell_as_end_of_stream() {
    let server = server_with_default(true);
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut t = mount.into_recv_transport().unwrap();
    t.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    let mut rx = tst_pipeline::DemuxReceiver::new(t);
    let mut a = publish(port, &ts_fixture_packets(3), 0xA);
    assert_eq!(a.teardown("/pub"), 200);
    let deadline = std::time::Instant::now() + scaled(Duration::from_secs(10));
    loop {
        assert!(std::time::Instant::now() < deadline, "never ended");
        match rx.recv_event() {
            Ok(None) => break, // end of stream
            Ok(Some(_)) => {}
            Err(e) => panic!("ended with an error, not EndOfStream: {e:?}"),
        }
    }
    server.stop().ok();
}

/// A publisher that was admitted by ANNOUNCE but never reached RECORD is not
/// a publisher leaving: the mount stays open for the next one.
#[test]
fn a_publisher_that_never_recorded_does_not_end_the_mount() {
    let server = server_with_default(true);
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

    let mut early = RawPublisher::connect(port);
    assert_eq!(early.announce("/pub", SDP_MP2T), 200);
    drop(early);
    wait_for(scaled(Duration::from_secs(3)), || {
        mount.publisher().is_none().then_some(())
    });

    let bundles = ts_fixture_packets(2);
    let mut next = publish(port, &bundles, 0xE);
    let mut buf = vec![0u8; 65536];
    for (i, bundle) in bundles.iter().enumerate() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &bundle[..], "bundle {i}");
    }
    assert_eq!(next.teardown("/pub"), 200);
    drain_then_closed(&mut app, &[]);
    server.stop().ok();
}
