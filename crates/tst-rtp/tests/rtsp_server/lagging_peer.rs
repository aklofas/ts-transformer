//! Lagging-peer behavior.
//!
//! A slow/stalled peer must not block the muxer (or, by tokio broadcast's
//! per-receiver cursor semantics, other peers). The producer pushes through a
//! `broadcast::Sender`, which never blocks on a slow subscriber — it drops the
//! oldest frames for that subscriber and bumps `MountStats::frames_dropped_total`
//! (the per-peer→mount aggregation is unit-tested deterministically in
//! `fanout.rs::mount_total_aggregates_peer_drops`).
//!
//! This integration test reproduces the real wire scenario: a raw
//! TCP-interleaved peer that completes SETUP+PLAY and then STOPS reading its
//! socket. Its server-side fanout task blocks on the TCP write once the kernel
//! send buffer fills — yet the muxer push path stays responsive. If the push
//! path were ever coupled to peer drain (e.g. a bounded blocking channel), this
//! loop would stall and nextest's per-test timeout would fail it.

use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tst_test_helpers::timing::scaled;

use tst_core::mpegts::common::Pts90khz;
use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, session_id};
use crate::fixtures::{wait_for, wait_for_parked_fanout};

/// A stalled TCP-interleaved peer does not block the muxer push path.
#[test]
fn slow_peer_does_not_block_muxer() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let base = format!("rtsp://127.0.0.1:{port}/live");

    // Raw TCP-interleaved handshake. SETUP uses the bare mount path (the
    // server's extract_mount_path does not strip a trailing trackID segment).
    let peer = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    // Before connect, so the window the peer advertises stays small.
    peer.set_recv_buffer_size(64 * 1024).unwrap();
    peer.connect(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())
        .unwrap();
    let mut peer: TcpStream = peer.into();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let setup_resp = request(
        &mut peer,
        &format!(
            "SETUP {base} RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(
        setup_resp.starts_with("RTSP/1.0 200"),
        "SETUP failed:\n{setup_resp}"
    );
    let sid = session_id(&setup_resp);

    let play_resp = request(
        &mut peer,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(
        play_resp.starts_with("RTSP/1.0 200"),
        "PLAY failed:\n{play_resp}"
    );

    // From here the peer NEVER reads again — its server-side fanout task will
    // block on the TCP write once the kernel send buffer fills. `peer` stays in
    // scope so the connection (and the blocked fanout task) stays alive.

    // Minimal IDR NAL; the muxer doesn't parse the payload.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    // Push far more than the 256-frame default fanout capacity so the stalled
    // peer is genuinely overwhelmed. Each push is a non-blocking broadcast.
    let start = Instant::now();
    for i in 0..3000i64 {
        mount
            .push_video(&nal, Pts90khz::new(i * 3000), true)
            .expect("push must not fail or block on a stalled peer");
    }
    let elapsed = start.elapsed();

    // The 3000 non-blocking pushes complete in well under a second locally; a
    // generous bound still fails loudly (and well within nextest's per-test
    // timeout) if the push path ever became coupled to peer drain.
    assert!(
        elapsed < scaled(Duration::from_secs(10)),
        "muxer push loop stalled behind the slow peer: took {elapsed:?}"
    );

    drop(peer);
    server.stop().ok();
}

/// Bytes pushed to a peer that does not read, far beyond what the loopback
/// socket buffers on either side hold, so the fanout's write is parked.
const STALL_BYTES: u64 = 32 * 1024 * 1024;

/// A TCP-interleaved peer that played `/live` and then stopped reading, with
/// its server-side fanout parked on the full socket (holding the session's
/// writer lock). Returns once the server's write probe shows that parked
/// write; panics if it never does.
///
/// Pushed bytes alone do not prove the stall: the broadcast drops frames a
/// lagging fanout has not read, so a mount can push far more than ever
/// reaches the socket. The peer's small receive buffer only makes the
/// socket fill sooner.
fn stalled_playing_peer() -> (RtspServer, TcpStream, u16) {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let base = format!("rtsp://127.0.0.1:{port}/live");

    let peer = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    // Before connect, so the window the peer advertises stays small.
    peer.set_recv_buffer_size(64 * 1024).unwrap();
    peer.connect(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())
        .unwrap();
    let mut peer: TcpStream = peer.into();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    // The server reads this side, so a write cannot park; bound it anyway.
    peer.set_write_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let setup_resp = request(
        &mut peer,
        &format!(
            "SETUP {base} RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup_resp.starts_with("RTSP/1.0 200"), "{setup_resp}");
    let sid = session_id(&setup_resp);
    let play_resp = request(
        &mut peer,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play_resp.starts_with("RTSP/1.0 200"), "{play_resp}");

    // Large access units fill the peer's socket buffers quickly; the muxer
    // does not parse the payload.
    let mut nal = vec![0xBB; 32 * 1024];
    nal[..5].copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
    let deadline = Instant::now() + scaled(Duration::from_secs(4));
    let mut i = 0i64;
    while mount.stats().bytes_pushed < STALL_BYTES && Instant::now() < deadline {
        mount
            .push_video(&nal, Pts90khz::new(i * 3000), true)
            .expect("push must not fail on a stalled peer");
        i += 1;
    }
    assert_eq!(mount.stats().peer_count, 1, "the peer is not subscribed");
    wait_for_parked_fanout(&server, scaled(Duration::from_secs(10)));
    (server, peer, port)
}

/// A session whose peer stopped reading must still end on stop(), whatever
/// reply it was writing: the unparseable-request reply shares the fanout's
/// writer lock and used to wait for it without a bound.
#[test]
fn stop_ends_a_stalled_session_that_was_answering_an_unparseable_request() {
    let (server, mut peer, _port) = stalled_playing_peer();
    peer.write_all(b"NOT AN RTSP LINE\r\n\r\n").unwrap();
    std::thread::sleep(scaled(Duration::from_millis(200)));
    server.stop().unwrap();
    wait_for(scaled(Duration::from_secs(6)), || {
        (server.stats().active_sessions == 0).then_some(())
    });
}

/// Same, for the 413 reply to an over-cap request.
#[test]
fn stop_ends_a_stalled_session_that_was_answering_413() {
    let (server, mut peer, _port) = stalled_playing_peer();
    // Past the 64 KiB header cap, with no CRLFCRLF.
    peer.write_all(&vec![b'A'; 70 * 1024]).unwrap();
    std::thread::sleep(scaled(Duration::from_millis(200)));
    server.stop().unwrap();
    wait_for(scaled(Duration::from_secs(6)), || {
        (server.stats().active_sessions == 0).then_some(())
    });
}
