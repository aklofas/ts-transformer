//! Publish mount stats, `PublisherInfo`, and the server-wide publisher
//! counters on `ServerStats`.

use std::time::{Duration, Instant, SystemTime};

use tst_core::transport::RecvTransport;
use tst_rtp::{PublishShape, RtspClient, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

/// Read from `t` until `want` payload bytes have arrived.
fn drain(t: &mut impl RecvTransport, want: usize) {
    let mut got = 0;
    let mut buf = vec![0u8; 65536];
    while got < want {
        got += t
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("app after {got} bytes: {e:?}"));
    }
}

/// Poll `cond` until it holds or two seconds pass.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cond() {
        assert!(Instant::now() < deadline, "{what} never held");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Twenty interleaved MP2T packets: the mount's counters, its publisher
/// info, and the server's publisher totals all agree; TEARDOWN frees the
/// server's publisher count.
#[test]
fn publish_mount_and_server_stats_count_an_interleaved_publisher() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(Duration::from_secs(5)));
    assert_eq!(server.stats().active_publishers, 0);

    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/pub?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let _reader_t = reader.setup_mp2t_auto(&sdp).unwrap().into_recv_transport();
    reader.play().unwrap();

    let before_announce = SystemTime::now();
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, ch) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    let ch = ch.expect("server allocated an interleaved channel");
    assert_eq!(p.record("/pub"), 200);
    assert_eq!(server.stats().active_publishers, 1);

    let packets: Vec<Vec<u8>> = ts_fixture_packets(20)
        .iter()
        .enumerate()
        .map(|(i, ts)| rtp_wrap(i as u16, i as u32 * 3003, 0xABCD, 33, ts))
        .collect();
    let wire_bytes: usize = packets.iter().map(Vec::len).sum();
    for pkt in &packets {
        p.send_frame(ch, pkt);
    }
    drain(&mut app, packets.iter().map(|p| p.len() - 12).sum());

    let s = mount.stats();
    assert_eq!(s.rtp_packets_received, 20);
    assert_eq!(s.bytes_received, wire_bytes as u64);
    assert_eq!(s.frames_emitted, 20);
    assert_eq!(s.peer_count, 1);
    assert_eq!(s.generation, 0);
    let info = mount.publisher().expect("publisher holds the mount");
    assert!(info.peer.ip().is_loopback());
    assert_eq!(info.shape, PublishShape::Mp2t);
    assert!(info.since >= before_announce && info.since <= SystemTime::now());
    assert_eq!(info.generation, 0);

    let st = server.stats();
    assert_eq!(st.active_publishers, 1);
    assert_eq!(st.total_rtp_packets_received, 20);
    assert_eq!(st.total_rtp_bytes_received, wire_bytes as u64);

    assert_eq!(p.teardown("/pub"), 200);
    let st = server.stats();
    assert_eq!(st.active_publishers, 0, "TEARDOWN frees the publisher");
    assert_eq!(st.total_rtp_packets_received, 20, "totals are cumulative");
    assert_eq!(mount.stats().generation, 1);
    server.stop().ok();
}

/// Every other way a publisher slot is released keeps `active_publishers`
/// right: a dropped control connection, `remove_mount`, and `stop()`.
#[test]
fn active_publishers_follows_disconnect_remove_mount_and_stop() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _a = server.add_publish_mount("/a").unwrap();
    let _b = server.add_publish_mount("/b").unwrap();
    let _c = server.add_publish_mount("/c").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let publish = |path: &str| {
        let mut p = RawPublisher::connect(port);
        assert_eq!(p.announce(path, SDP_MP2T), 200);
        assert_eq!(p.setup_interleaved(path, "streamid=0").0, 200);
        assert_eq!(p.record(path), 200);
        p
    };
    let pa = publish("/a");
    let _pb = publish("/b");
    let _pc = publish("/c");
    assert_eq!(server.stats().active_publishers, 3);

    drop(pa);
    eventually("disconnect frees /a", || {
        server.stats().active_publishers == 2
    });

    server.remove_mount("/b").unwrap();
    assert_eq!(server.stats().active_publishers, 1);

    server.stop().unwrap();
    assert_eq!(server.stats().active_publishers, 0);
}
