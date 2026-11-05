//! Lifecycle of a publish mount's application transport and publisher
//! slot: cancel, publisher TEARDOWN, server stop, idle reaping.

use std::net::UdpSocket;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, TransportError};
use tst_rtp::{RtpRecvTransport, RtspClient, RtspServer, RtspServerBuilder, RtspServerError};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::*;

/// Run `app.recv_bytes` on a thread; the result arrives on the returned
/// channel (the caller bounds its wait with `recv_timeout`). Nothing proves
/// the call is parked when the caller acts, so the tests claim only that
/// the action ends the call.
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

/// A cancel from another thread ends the application `recv_bytes` with
/// `ExplicitClose`, and leaves the publisher and the reader running.
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
    reader_t.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    reader.play().unwrap();

    let mut p = RawPublisher::connect(port);
    let ch = record_interleaved(&mut p, "/pub");

    let parked = park(app);
    std::thread::sleep(Duration::from_millis(200)); // usually parked by now; cancel-before-op ends the same way
    mount.cancel();
    let r = parked
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("cancel ended the recv");
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
/// connection, and ends the application transport: a `recv_bytes` on
/// another thread returns `Closed`.
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
    std::thread::sleep(Duration::from_millis(200)); // usually parked by now; not proven

    // stop() blocks through its graceful drain; run it off the test thread.
    let stopper = std::thread::spawn(move || {
        server.stop().ok();
        server
    });
    let (bytes, _eof) = p.read_until(
        b"Notice: 5402",
        Instant::now() + scaled(Duration::from_secs(5)),
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("ANNOUNCE ") && text.contains("Notice: 5402"),
        "publisher did not see the Notice 5402 ANNOUNCE: {text:?}"
    );
    let (rest, eof) = p.read_until(
        b"\0never\0",
        Instant::now() + scaled(Duration::from_secs(5)),
    );
    assert!(
        eof,
        "connection not closed after the notice (read {} more bytes)",
        rest.len()
    );

    let r = parked
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("stop() ended the recv");
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

    // Not scaled: the poll must end before a fixed 30 s reaper would fire
    // (≈20 s after RECORD here), and the 4 s bound is long past.
    let deadline = Instant::now() + Duration::from_secs(15);
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

/// A server whose publish idle timeout is `scaled(1 s)` and whose session
/// timeout is far longer, with one publish mount on `/pub`.
fn publish_idle_server() -> (RtspServer, tst_rtp::PublishMountHandle, u16) {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.session_timeout(Duration::from_secs(60));
    b.publish_idle_timeout(scaled(Duration::from_secs(1)));
    let server = b.build().unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    (server, mount, port)
}

/// With `publish_idle_timeout` set, a recording publisher that sends no
/// media loses its session at the timeout, not at the session timeout.
#[test]
fn a_silent_publisher_is_ended_at_the_publish_idle_timeout() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");
    assert!(mount.publisher().is_some());

    let (_bytes, eof) = p.read_until(
        b"\0never\0",
        Instant::now() + scaled(Duration::from_secs(2)),
    );
    assert!(
        eof,
        "the control connection stayed open past the publish idle timeout"
    );
    assert!(mount.publisher().is_none());
    server.stop().ok();
}

/// A publisher that keeps its session alive with RTSP requests but sends no
/// media is ended by the publish idle timeout, though the session timeout is
/// far longer.
#[test]
fn keepalives_without_media_do_not_outlast_the_publish_idle_timeout() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");

    // Keep the session alive with requests until the server ends it; if
    // keepalives postponed the idle timeout, no close would ever come.
    let deadline = Instant::now() + scaled(Duration::from_secs(3));
    let mut closed = false;
    while Instant::now() < deadline {
        std::thread::sleep(scaled(Duration::from_millis(300)));
        if p.try_get_parameter("/pub") != Some(200) {
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "keepalives kept a media-less publisher past the publish idle timeout"
    );
    assert!(mount.publisher().is_none());
    server.stop().ok();
}

/// Interleaved frames that fail RTP validation (wrong payload type, or too
/// short to be RTP) are not liveness: a publisher sending only those is ended
/// by the publish idle timeout, as a UDP publisher sending them would be.
#[test]
fn malformed_interleaved_frames_do_not_keep_a_publisher_alive() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    let ch = record_interleaved(&mut p, "/pub");
    let bundle = &ts_fixture_packets(1)[0];

    let deadline = Instant::now() + scaled(Duration::from_secs(3));
    let mut seq = 0u16;
    while Instant::now() < deadline && mount.publisher().is_some() {
        p.send_frame(ch, &[1, 2, 3]);
        p.send_frame(ch, &rtp_wrap(seq, 0, 0xA, 96, bundle));
        seq = seq.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        mount.publisher().is_none(),
        "malformed interleaved frames kept a media-less publisher alive"
    );
    drop(p);
    server.stop().ok();
}

/// A single byte of a request that never completes does not suspend the
/// publish idle timeout while the framing bound is still far away.
#[test]
fn a_partial_request_does_not_suspend_the_publish_idle_timeout() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");
    p.write_raw(b"G");

    let (_bytes, eof) = p.read_until(
        b"\0never\0",
        Instant::now() + scaled(Duration::from_secs(3)),
    );
    assert!(
        eof,
        "a partial request kept a media-less publisher past the publish idle timeout"
    );
    assert!(mount.publisher().is_none());
    server.stop().ok();
}

/// Media inside the window keeps the publisher past the timeout.
#[test]
fn a_publishing_publisher_outlives_the_publish_idle_timeout() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    let ch = record_interleaved(&mut p, "/pub");
    let bundle = &ts_fixture_packets(1)[0];

    let t0 = Instant::now();
    let mut seq = 0u16;
    while t0.elapsed() < scaled(Duration::from_secs(3)) {
        p.send_frame(ch, &rtp_wrap(seq, 0, 0xA, 33, bundle));
        seq = seq.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        mount.publisher().is_some(),
        "a publisher sending media was ended by the publish idle timeout"
    );
    drop(p);
    server.stop().ok();
}

/// A publisher that has not reached RECORD is governed by the session
/// timeout alone.
#[test]
fn the_publish_idle_timeout_ignores_a_publisher_that_never_records() {
    let (server, mount, port) = publish_idle_server();
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, _) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);

    let (_bytes, eof) = p.read_until(
        b"\0never\0",
        Instant::now() + scaled(Duration::from_secs(2)),
    );
    assert!(
        !eof,
        "a publisher that never recorded was closed by the publish idle timeout"
    );
    assert!(mount.publisher().is_some());
    drop(p);
    server.stop().ok();
}

/// `remove_mount` on a publish mount with a recording publisher, an
/// application `recv_bytes` on another thread and a PLAY reader: the publisher reads the
/// Notice 5402 ANNOUNCE then EOF, the application transport reads
/// `Closed`, the reader's stream ends, and the path is free again.
#[test]
fn remove_publish_mount_closes_app_transport_and_ends_sessions() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let app = mount.clone().into_recv_transport().unwrap();

    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/pub?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let mut reader_t = reader.setup_mp2t_auto(&sdp).unwrap().into_recv_transport();
    reader_t.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    reader.play().unwrap();

    let mut p = RawPublisher::connect(port);
    record_interleaved(&mut p, "/pub");
    let parked = park(app);
    std::thread::sleep(Duration::from_millis(200)); // usually parked by now; close-before-op ends the same way

    server.remove_mount("/pub").unwrap();

    let (bytes, _eof) = p.read_until(
        b"Notice: 5402",
        Instant::now() + scaled(Duration::from_secs(5)),
    );
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("ANNOUNCE ") && text.contains("Notice: 5402"),
        "publisher did not see the Notice 5402 ANNOUNCE: {text:?}"
    );
    let (rest, eof) = p.read_until(
        b"\0never\0",
        Instant::now() + scaled(Duration::from_secs(5)),
    );
    assert!(
        eof,
        "publisher connection not closed after the notice (read {} more bytes)",
        rest.len()
    );

    let r = parked
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("remove_mount ended the recv");
    assert!(matches!(r, Err(TransportError::Closed)), "got {r:?}");
    assert!(mount.publisher().is_none());

    // The reader's stream ends (no publisher frames were sent, so the
    // first non-data result is the end); a 5 s recv timeout would show
    // up as `Backpressure`.
    let mut buf = vec![0u8; 65536];
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    let end = loop {
        match reader_t.recv_bytes(&mut buf) {
            Ok(_) if Instant::now() < deadline => continue,
            other => break other,
        }
    };
    assert!(
        matches!(
            end,
            Err(TransportError::Closed) | Err(TransportError::Broken { .. })
        ),
        "reader stream did not end: {end:?}"
    );

    assert_eq!(server.stats().mounts, 0);
    server
        .add_publish_mount("/pub")
        .expect("the path is free again");
    server.stop().ok();
}

#[test]
fn remove_mount_unknown_is_mount_not_found() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let e = server.remove_mount("/nope").unwrap_err();
    assert!(
        matches!(&e, RtspServerError::MountNotFound { path } if path == "/nope"),
        "got {e:?}"
    );
}
