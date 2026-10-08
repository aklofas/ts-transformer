//! Lifecycle of a publish mount's application transport and publisher
//! slot: cancel, publisher TEARDOWN, server stop, idle reaping.

use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, TransportError};
use tst_rtp::{RtpRecvTransport, RtspClient, RtspServer, RtspServerBuilder};

use crate::fixtures::raw_rtsp_publisher::*;

/// Park `app.recv_bytes` on a thread; the result arrives on the returned
/// channel (the caller bounds its wait with `recv_timeout`).
fn park(mut app: RtpRecvTransport) -> mpsc::Receiver<Result<usize, TransportError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let _ = tx.send(app.recv_bytes(&mut buf));
    });
    rx
}

/// ANNOUNCE + interleaved SETUP + RECORD on `mount`; returns the channel.
fn record_interleaved(p: &mut RawPublisher, mount: &str) -> u8 {
    assert_eq!(p.announce(mount, SDP_MP2T), 200);
    let (status, ch) = p.setup_interleaved(mount, "streamid=0");
    assert_eq!(status, 200);
    assert_eq!(p.record(mount), 200);
    ch.unwrap()
}

/// ANNOUNCE + UDP SETUP + RECORD on `mount`; returns the server RTP port.
fn record_udp(p: &mut RawPublisher, mount: &str, client_port: u16) -> u16 {
    assert_eq!(p.announce(mount, SDP_MP2T), 200);
    let (status, ports) = p.setup_udp(mount, "streamid=0", client_port);
    assert_eq!(status, 200);
    assert_eq!(p.record(mount), 200);
    ports.unwrap().0
}

#[test]
fn cancel_wakes_parked_app_recv_with_explicit_close() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let app = mount.clone().into_recv_transport().unwrap();

    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/pub?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let mut reader_t = reader.setup_mp2t_auto(&sdp).unwrap().into_recv_transport();
    reader_t.set_recv_timeout(Some(Duration::from_secs(5)));
    reader.play().unwrap();

    let mut p = RawPublisher::connect(port);
    let ch = record_interleaved(&mut p, "/pub");

    let parked = park(app);
    std::thread::sleep(Duration::from_millis(200)); // let it park (cancel-before-op ends the same way)
    mount.cancel();
    let r = parked
        .recv_timeout(Duration::from_secs(5))
        .expect("cancel woke the parked recv");
    assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");

    // The app side ended; the publisher and the reader did not.
    assert!(mount.publisher().is_some());
    let bundles = ts_fixture_packets(3);
    for (i, b) in bundles.iter().enumerate() {
        p.send_frame(ch, &rtp_wrap(i as u16, i as u32 * 3003, 9, 33, b));
    }
    let mut buf = vec![0u8; 65536];
    for (i, b) in bundles.iter().enumerate() {
        let n = reader_t
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("reader bundle {i}: {e:?}"));
        assert_eq!(&buf[..n], &b[..], "reader bundle {i}");
    }
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}

#[test]
fn publisher_teardown_leaves_app_transport_open() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_millis(300)));

    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");
    assert_eq!(p.teardown("/pub"), 200);
    assert!(mount.publisher().is_none());

    let mut buf = vec![0u8; 65536];
    let r = app.recv_bytes(&mut buf);
    assert!(
        matches!(r, Err(TransportError::Backpressure { .. })),
        "idle but open after the publisher left, got {r:?}"
    );
    assert!(app.is_alive());
    server.stop().ok();
}

/// `stop()` sends the publisher the Notice 5402 ANNOUNCE before closing its
/// connection, and ends the application transport: a parked `recv_bytes`
/// returns `Closed`.
#[test]
fn server_stop_closes_app_transport() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let app = mount.clone().into_recv_transport().unwrap();

    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");
    let parked = park(app);
    std::thread::sleep(Duration::from_millis(200)); // let it park

    // stop() blocks through its graceful drain; run it off the test thread.
    let stopper = std::thread::spawn(move || {
        server.stop().ok();
        server
    });
    let (bytes, _eof) = p.read_until(b"Notice: 5402", Instant::now() + Duration::from_secs(5));
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("ANNOUNCE ") && text.contains("Notice: 5402"),
        "publisher did not see the Notice 5402 ANNOUNCE: {text:?}"
    );
    let (rest, eof) = p.read_until(b"\0never\0", Instant::now() + Duration::from_secs(5));
    assert!(
        eof,
        "connection not closed after the notice (read {} more bytes)",
        rest.len()
    );

    let r = parked
        .recv_timeout(Duration::from_secs(5))
        .expect("stop() woke the parked recv");
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(mount.publisher().is_none());
    drop(stopper.join().unwrap());
}

/// RTP media is liveness for a UDP publisher (no RTSP keepalive needed); a
/// publisher that sends neither is reaped at `post_setup_idle_bound`
/// (2 s timeout → 4 s).
#[test]
fn idle_publisher_is_reaped_but_media_only_publisher_is_not() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.session_timeout(Duration::from_secs(2));
    let server = b.build().unwrap();
    let live = server.add_publish_mount("/a").unwrap();
    let idle = server.add_publish_mount("/b").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    // Two announced client ports (never sent from), each from a bound
    // socket so neither is in use by anything else.
    let announced_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let announced_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut pa = RawPublisher::connect(port);
    let a_rtp = record_udp(&mut pa, "/a", announced_a.local_addr().unwrap().port());
    let mut pb = RawPublisher::connect(port);
    record_udp(&mut pb, "/b", announced_b.local_addr().unwrap().port());

    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let bundle = &ts_fixture_packets(1)[0];
    let t0 = Instant::now();
    let mut seq = 0u16;
    while t0.elapsed() < Duration::from_secs(5) {
        sender
            .send_to(&rtp_wrap(seq, 0, 0xA, 33, bundle), ("127.0.0.1", a_rtp))
            .unwrap();
        seq = seq.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        live.publisher().is_some(),
        "a publisher sending only RTP media was reaped"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while idle.publisher().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        idle.publisher().is_none(),
        "a publisher sending nothing was never reaped"
    );
    drop((pa, pb));
    server.stop().ok();
}
