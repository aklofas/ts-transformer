//! TCP-interleaved loopback test.
//!
//! Both halves of the TCP-interleaved wire-up:
//!
//! - **Server side** — `handle_connection_inner` splits the
//!   per-session TCP and shares the `Arc<Mutex<OwnedWriteHalf>>` with
//!   the per-peer fanout task; `handle_play` for `TcpInterleaved`
//!   spawns the fanout.
//! - **Client side** — from SETUP on, the client's I/O thread routes
//!   binary `$`-framed RTP to the session's media queue and RTCP to its
//!   statistics, and hands each later request its own reply by CSeq.
//!
//! The control-plane test below exercises the handshake. A future test
//! can push a TS payload through the mount and assert the
//! byte-identical RTP-framed payload reaches the client; this file
//! intentionally stays narrow on the handshake to keep the regression
//! signal localized.

use crate::fixtures::raw_rtsp::{make_muxer_cfg, scaled};
use std::time::Duration;
use tst_rtp::{RtspClient, RtspServer};

/// Server accepts a `?transport=tcp` client through PLAY without
/// erroring. The fanout task is spawned on the interleaved branch
/// — it'll sit on `rx.recv()` waiting for frames that the test
/// doesn't push, which is the success path here.
#[test]
fn client_setup_with_transport_tcp_round_trips_ts() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live?transport=tcp");

    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    assert_eq!(
        session.transport_kind(),
        tst_rtp::RtspTransportKind::TcpInterleaved
    );
    let _recv = session.into_recv_transport();
    client.play().unwrap();
    // `rtsp_client/interleaved_e2e.rs` drives the same path through
    // teardown; neither test asserts byte-identical delivery.
}

/// A raw TCP-interleaved reader of `/live` that is playing; returns its
/// connection and the RTP channel the server granted (the pair comes from
/// a counter shared by every reader in the process).
fn playing_interleaved_reader(port: u16) -> (std::net::TcpStream, u8) {
    use crate::fixtures::raw_rtsp::{request, session_id, try_header};

    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(r.starts_with("RTSP/1.0 200"), "{r}");
    let channel: u8 = try_header(&r, "Transport")
        .and_then(|t| {
            t.split(';')
                .find_map(|p| p.trim().strip_prefix("interleaved="))
        })
        .and_then(|pair| pair.split('-').next())
        .and_then(|ch| ch.parse().ok())
        .unwrap_or_else(|| panic!("no interleaved pair granted: {r}"));
    let sid = session_id(&r);
    let r = request(
        &mut tcp,
        &format!("PLAY rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(r.starts_with("RTSP/1.0 200"), "{r}");
    (tcp, channel)
}

/// Reads `$` frames off `tcp` until `count` of them were on `channel`;
/// returns the sum of those frames' RTP packet lengths (without the 4-byte
/// interleaved prefix). Frames on other channels are skipped.
fn read_rtp_frames(tcp: &mut std::net::TcpStream, channel: u8, count: u64) -> u64 {
    use std::io::Read as _;
    let mut seen = 0u64;
    let mut bytes = 0u64;
    let mut head = [0u8; 4];
    let mut body = vec![0u8; 65_536];
    while seen < count {
        tcp.read_exact(&mut head)
            .expect("a frame within the timeout");
        assert_eq!(head[0], b'$', "only media after PLAY's reply");
        let len = u16::from_be_bytes([head[2], head[3]]) as usize;
        tcp.read_exact(&mut body[..len]).expect("the frame's body");
        if head[1] == channel {
            seen += 1;
            bytes += len as u64;
        }
    }
    bytes
}

/// Pushes five access units into `mount`; returns its stats after.
fn push_five(mount: &tst_rtp::MountHandle) -> tst_rtp::MountStats {
    use tst_core::mpegts::common::Pts90khz;
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    for i in 0..5 {
        mount
            .push_video(&nal, Pts90khz::new(i * 3000), true)
            .unwrap();
    }
    let pushed = mount.stats();
    assert!(pushed.packets_pushed > 0);
    pushed
}

/// Every RTP packet written to a TCP-interleaved reader is counted in
/// `ServerStats`: the packet total equals the frames the reader read and
/// the byte total the RTP packets' lengths, without the 4-byte prefix.
#[test]
fn the_server_counts_the_rtp_it_sent_interleaved() {
    use crate::fixtures::wait_for;

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let budget = scaled(Duration::from_secs(5));

    let (mut tcp, channel) = playing_interleaved_reader(port);
    let pushed = push_five(&mount);
    let read = read_rtp_frames(&mut tcp, channel, pushed.packets_pushed);
    wait_for(budget, || {
        (server.interleaved_write_probe().1 == pushed.packets_pushed).then_some(())
    });
    // The fanout adds after its write returned, so the reader can be ahead
    // of the counters for a moment: wait for each total. The bytes are
    // added before the packet, so no snapshot counts a packet whose bytes
    // it lacks.
    wait_for(budget, || {
        (server.stats().total_rtp_packets_sent == pushed.packets_pushed).then_some(())
    });
    wait_for(budget, || {
        (server.stats().total_rtp_bytes_sent == read).then_some(())
    });
    assert_eq!(read, pushed.bytes_pushed + 12 * pushed.packets_pushed);
    server.stop().ok();
}

/// Two readers of one mount are each sent every packet, and each send is
/// counted.
#[test]
fn two_readers_are_counted_twice() {
    use crate::fixtures::wait_for;

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let budget = scaled(Duration::from_secs(5));

    let (mut a, ch_a) = playing_interleaved_reader(port);
    let (mut b, ch_b) = playing_interleaved_reader(port);
    let pushed = push_five(&mount);
    let read = read_rtp_frames(&mut a, ch_a, pushed.packets_pushed)
        + read_rtp_frames(&mut b, ch_b, pushed.packets_pushed);
    wait_for(budget, || {
        (server.stats().total_rtp_packets_sent == 2 * pushed.packets_pushed).then_some(())
    });
    wait_for(budget, || {
        (server.stats().total_rtp_bytes_sent == read).then_some(())
    });
    assert_eq!(read, 2 * (pushed.bytes_pushed + 12 * pushed.packets_pushed));
    server.stop().ok();
}
