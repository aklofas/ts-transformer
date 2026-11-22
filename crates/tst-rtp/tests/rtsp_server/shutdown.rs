//! RtspServer graceful + hard shutdown integration tests. Exercises the
//! registry/cancel path; the RFC 7826 §13.5.1 Notice 5402 wire
//! transmission is covered by `notice_5402.rs`.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, session_id};
use crate::lagging_peer::stall_a_playing_peer;
use tst_core::mpegts::common::Pts90khz;
use tst_rtp::{MountError, RtspServer, RtspServerError};

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
            && t0.elapsed() < scaled(std::time::Duration::from_secs(5))
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
/// still open, ends under a hard cancel, and its RTP fanout stops with it;
/// the mount refuses every push after the cancel. Before the listener fired
/// the server token, a streaming session never ended under a hard cancel at
/// all. Latch-and-poll, no duration assertion.
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
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
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
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
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
    // Every push from here on is refused.
    server.cancel_handle().cancel();
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while server.stats().active_sessions != 0 || mount.peer_count() != 0 {
        let r = mount.push_video(&nal, Pts90khz::new(pts), true);
        assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
        pts += 3600;
        assert!(
            Instant::now() < deadline,
            "a streaming session outlived the hard cancel (sessions = {}, peers = {})",
            server.stats().active_sessions,
            mount.peer_count()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Drain what was in flight (bounded); the drain ends on the socket's
    // 500 ms read timeout, so nothing arrived for that long after it.
    let mut drained = 0usize;
    while rtp.recv(&mut buf).is_ok() {
        drained += 1;
        assert!(drained < 64, "RTP still arriving after the hard cancel");
    }
    drop(tcp);
}

const IDR: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];

fn push_once(mount: &tst_rtp::MountHandle) -> Result<(), MountError> {
    mount.push_video(&IDR, Pts90khz::new(0), true)
}

#[test]
fn mount_push_after_stop_is_closed() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
}

/// Refused at once: the push reads the cancel flag itself, not whatever
/// the listener does about it later.
#[test]
fn mount_push_right_after_a_hard_cancel_is_closed() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server.cancel_handle().cancel();
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
}

#[test]
fn mount_push_after_a_hard_cancel_on_a_never_started_server_is_closed() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.cancel_handle().cancel();
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
}

#[test]
fn mount_push_after_the_server_is_dropped_is_closed() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    drop(server);
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
}

/// A removed mount's handle is refused while the server lives, and stays
/// refused once the server ends.
#[test]
fn removed_mount_push_is_closed_before_and_after_the_server_ends() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server.remove_mount("/live").unwrap();
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
    server.stop().unwrap();
    let r = push_once(&mount);
    assert!(matches!(r, Err(MountError::Closed)), "{r:?}");
}

/// A push before the end is muxed and counted; the refused one after it
/// leaves the mount's stats as they were.
#[test]
fn a_refused_mount_push_leaves_the_stats_unchanged() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    push_once(&mount).unwrap();
    let before = mount.stats();
    assert!(before.bytes_pushed > 0, "{before:?}");
    server.stop().unwrap();
    assert!(matches!(push_once(&mount), Err(MountError::Closed)));
    assert_eq!(mount.stats(), before);
}

/// A started server with no PLAY reader yet accepts pushes: only the
/// server's end refuses them.
#[test]
fn mount_push_on_a_started_server_before_any_play_succeeds() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    push_once(&mount).expect("a push with no reader yet succeeds");
    assert!(mount.stats().bytes_pushed > 0);
    server.stop().unwrap();
}

/// A hard cancel is terminal: the server cannot be started afterwards.
#[test]
fn start_after_a_hard_cancel_is_refused() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.cancel_handle().cancel();
    assert!(matches!(server.start(), Err(RtspServerError::Shutdown)));
}

/// Whether a connection made now is served: connected and its OPTIONS
/// answered. A refused connect is not; neither is a connection to whatever
/// else may hold the port, which answers no RTSP.
fn a_new_connection_is_served(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut tcp) = std::net::TcpStream::connect_timeout(&addr, scaled(Duration::from_secs(2)))
    else {
        return false;
    };
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    let req = format!("OPTIONS rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    if tcp.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    tcp.read_exact(&mut head).is_ok() && head.starts_with(b"RTSP/1.0 200")
}

/// `stop()` admits nobody from the moment it begins. A reading reader and
/// then a reader that stopped reading: `stop()` writes the first one's
/// Notice and then waits up to a second on the second one's, and a
/// connection made in that second is not served.
#[test]
fn a_stop_in_progress_admits_nobody() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let base = format!("rtsp://127.0.0.1:{port}/live");

    // First in the registry, so its Notice is written first. Its media goes
    // over UDP, so its control connection carries the replies and the
    // Notice only.
    let rtp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let rtp_port = rtp.local_addr().unwrap().port();
    let mut reading = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    reading
        .set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let setup = request(
        &mut reading,
        &format!(
            "SETUP {base} RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP;unicast;client_port={rtp_port}-{}\r\n\r\n",
            rtp_port + 1
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let sid = session_id(&setup);
    let play = request(
        &mut reading,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");

    let _stalled = stall_a_playing_peer(&server, &mount);
    assert_eq!(mount.stats().peer_count, 2);
    assert!(a_new_connection_is_served(port), "control: before stop()");

    let server = std::sync::Arc::new(server);
    let stopping = {
        let server = std::sync::Arc::clone(&server);
        std::thread::spawn(move || server.stop())
    };

    // The listening socket is closed before the first Notice is written, so
    // the first reader's Notice is the sign that nobody can come in.
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"5402") {
        assert!(Instant::now() < deadline, "no Notice: {seen:?}");
        let mut chunk = [0u8; 1024];
        match reading.read(&mut chunk) {
            Ok(0) => panic!("closed before the Notice: {seen:?}"),
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
            Err(e) => panic!("read: {e}"),
        }
    }
    // Only the listener's scheduling stands between `stop()` beginning and
    // its socket closing; the stalled reader's Notice holds `stop()` for a
    // second, far longer.
    let closed_by = Instant::now() + Duration::from_millis(300);
    while a_new_connection_is_served(port) {
        assert!(
            Instant::now() < closed_by,
            "a connection made while stop() ran was served"
        );
    }
    assert!(!stopping.is_finished(), "stop() returned too early to tell");
    stopping.join().unwrap().unwrap();
}

/// `flush()` reads the server's end as the pushes do, and changes nothing:
/// after `stop()`, after a hard cancel on a server never started, and after
/// the server was dropped.
#[test]
fn flush_after_the_server_ended_is_closed() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    push_once(&mount).unwrap();
    let before = mount.stats();
    server.stop().unwrap();
    let r = mount.flush();
    assert!(matches!(r, Err(MountError::Closed)), "after stop(): {r:?}");
    assert_eq!(mount.stats(), before);

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.cancel_handle().cancel();
    let r = mount.flush();
    assert!(
        matches!(r, Err(MountError::Closed)),
        "after a hard cancel: {r:?}"
    );

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    drop(server);
    let r = mount.flush();
    assert!(matches!(r, Err(MountError::Closed)), "after Drop: {r:?}");
}

#[test]
fn flush_on_a_live_mount_is_ok() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    mount.flush().expect("before start()");
    server.start().unwrap();
    push_once(&mount).unwrap();
    mount.flush().expect("on a started server");
    server.stop().unwrap();
}

/// `reset_stats()` is a counter operation: like the getters it still answers
/// once the server has ended.
#[test]
fn reset_stats_still_answers_after_the_end() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    push_once(&mount).unwrap();
    server.stop().unwrap();
    assert!(mount.stats().bytes_pushed > 0);
    mount.reset_stats();
    assert_eq!(mount.stats(), tst_rtp::MountStats::default());
}
