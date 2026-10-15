//! RtspServer graceful + hard shutdown integration tests. Exercises the
//! registry/cancel path; the RFC 7826 §13.5.1 Notice 5402 wire
//! transmission is covered by `notice_5402.rs`.

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, session_id};
use tst_core::mpegts::common::Pts90khz;
use tst_rtp::{RtspServer, RtspServerError};

#[test]
fn start_then_stop_clean_exit() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().expect("clean stop after start");
}

#[test]
fn stop_is_idempotent() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    // A second stop() on a shut-down server is a no-op and must return Ok.
    server.stop().expect("second stop is idempotent");
}

#[test]
fn add_mount_after_stop_errors_shutdown() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let e = server.add_mount("/live", make_muxer_cfg()).unwrap_err();
    assert!(
        matches!(e, RtspServerError::Shutdown),
        "post-stop add_mount must return Shutdown; got {e:?}",
    );
}

#[test]
fn add_multicast_mount_after_stop_errors_shutdown() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let e = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap_err();
    assert!(
        matches!(e, RtspServerError::Shutdown),
        "post-stop add_multicast_mount must return Shutdown; got {e:?}",
    );
}

#[test]
fn add_publish_mount_after_stop_errors_shutdown() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let r = server.add_publish_mount("/late");
    assert!(
        matches!(r, Err(RtspServerError::Shutdown)),
        "post-stop add_publish_mount must return Shutdown",
    );
}

#[test]
fn cancel_handle_flip_observable() {
    // Hard-cancel path: cancel_handle().cancel() flips the public flag.
    // The internal runtime tear-down happens on Drop; this test verifies
    // only the visible cancel-handle observation contract.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let h = server.cancel_handle();
    assert!(!h.is_cancelled());
    h.cancel();
    assert!(h.is_cancelled());
    // Drop after a hard-cancel completes without hanging.
    drop(server);
}

#[test]
fn drop_started_server_with_mount_does_not_leak() {
    // Drop fires the hard-cancel path; shutdown_timeout(5s) bounds the
    // runtime teardown. Validates the mount-held + started path doesn't
    // hang or panic at drop.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount.push_video(&nal, Pts90khz::new(0), true).unwrap();
    // Mount handle outlives the server momentarily — Drop on RtspServer
    // must not panic even with an outstanding MountHandle reference.
    drop(server);
    drop(mount);
}

#[test]
fn stop_does_not_panic_with_mounts_registered() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _m1 = server.add_mount("/a", make_muxer_cfg()).unwrap();
    let _m2 = server.add_mount("/b", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server.stop().expect("stop with registered mounts");
}

/// A hard cancel ends a session parked in its read. Before the fix such a
/// session lived until the 30 s pre-SETUP idle bound; the 5 s bound here
/// only separates "woken" from "not woken".
#[test]
fn hard_cancel_ends_a_session_parked_in_read() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let _peer = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
    let wait_for = |want: usize| {
        let t0 = std::time::Instant::now();
        while server.stats().active_sessions != want
            && t0.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        server.stats().active_sessions
    };
    assert_eq!(wait_for(1), 1, "the connection never became a session");
    server.cancel_handle().cancel();
    assert_eq!(
        wait_for(0),
        0,
        "a session parked in read outlived the hard cancel"
    );
}

/// A session in streaming PLAY, media flowing and its control connection
/// still open, ends under a hard cancel, and its RTP fanout stops with it.
/// Before the listener fired the server token, a streaming session never
/// ended under a hard cancel at all. Latch-and-poll, no duration assertion.
#[test]
fn hard_cancel_ends_a_streaming_play_session_and_its_fanout() {
    use std::net::{TcpStream, UdpSocket};
    use std::time::{Duration, Instant};

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let rtp = UdpSocket::bind("127.0.0.1:0").unwrap();
    rtp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let client_port = rtp.local_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP;unicast;client_port={client_port}-{}\r\n\r\n",
            client_port + 1
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let sid = session_id(&setup);
    let play = request(
        &mut tcp,
        &format!("PLAY {url} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");

    // Media is flowing to the client.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut buf = [0u8; 2048];
    let mut pts: i64 = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        mount.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        pts += 3600;
        if rtp.recv(&mut buf).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "no RTP reached the client");
    }
    assert_eq!(mount.peer_count(), 1);
    assert_eq!(server.stats().active_sessions, 1);

    // The control connection stays open; only the hard cancel ends the session.
    server.cancel_handle().cancel();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.stats().active_sessions != 0 || mount.peer_count() != 0 {
        mount.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        pts += 3600;
        assert!(
            Instant::now() < deadline,
            "a streaming session outlived the hard cancel (sessions = {}, peers = {})",
            server.stats().active_sessions,
            mount.peer_count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Drain what was in flight (bounded), then keep pushing: nothing new may arrive.
    let mut drained = 0usize;
    while rtp.recv(&mut buf).is_ok() {
        drained += 1;
        assert!(drained < 64, "RTP still arriving after the fanout stopped");
    }
    for _ in 0..25 {
        mount.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        pts += 3600;
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        rtp.recv(&mut buf).is_err(),
        "RTP kept flowing after the hard cancel"
    );
    drop(tcp);
}
