//! Review 9 (int R9-02): a viewer that leaves WITHOUT TEARDOWN (crash, link
//! loss, kill) in the client-default UDP mode left the server streaming
//! RTP to the dead address and holding its UDP port pair until the server
//! was dropped — `ServerSessionState` had no `Drop`, dropping the fanout
//! `JoinHandle` detached the task, and an unconnected UDP `send_to` to a
//! closed port never errors.

use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};
use tst_core::mpegts::common::Pts90khz;
use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, session_id};

#[test]
fn a_peer_that_leaves_without_teardown_takes_its_fanout_with_it() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    // The client's RTP socket, bound BEFORE SETUP so client_port is real.
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

    // Precondition: RTP reaches the client while the control TCP is up.
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
        assert!(
            Instant::now() < deadline,
            "no RTP reached the client while the session was up"
        );
    }
    assert_eq!(mount.peer_count(), 1);

    // The viewer vanishes: no TEARDOWN, the control connection just closes.
    drop(tcp);

    // The session task sees EOF and drops its state; the fanout must go
    // with it. Latch-and-poll — no duration assertion.
    let deadline = Instant::now() + Duration::from_secs(5);
    while mount.peer_count() != 0 {
        assert!(
            Instant::now() < deadline,
            "fanout must not outlive the session (peer_count = {})",
            mount.peer_count()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Drain what was in flight (bounded, so a regression that keeps
    // streaming fails here instead of spinning), then keep pushing:
    // nothing new may arrive.
    let mut drained = 0usize;
    while rtp.recv(&mut buf).is_ok() {
        drained += 1;
        assert!(
            drained < 64,
            "RTP still arriving after the fanout should have stopped"
        );
    }
    for _ in 0..25 {
        mount.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        pts += 3600;
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        rtp.recv(&mut buf).is_err(),
        "RTP kept flowing to a departed peer"
    );
    server.stop().ok();
}
