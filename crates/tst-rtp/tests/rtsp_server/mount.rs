//! add_mount / add_multicast_mount / MountHandle surface integration
//! tests. No RTP/RTCP flow exercised here; the loopback, multicast and
//! client tests cover the actual streaming paths.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{MountKind, RtspServer, RtspServerError};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

#[test]
fn add_mount_returns_handle_with_path() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    assert_eq!(mount.mount_path(), "/live");
    assert!(matches!(mount.mount_kind(), MountKind::Unicast));
}

#[test]
fn add_mount_rejects_path_without_leading_slash() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let e = server.add_mount("live", make_muxer_cfg()).unwrap_err();
    assert!(matches!(e, RtspServerError::InvalidMountPath { .. }));
}

#[test]
fn add_mount_rejects_duplicate_path() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.add_mount("/live", make_muxer_cfg()).unwrap();
    let e = server.add_mount("/live", make_muxer_cfg()).unwrap_err();
    assert!(matches!(e, RtspServerError::DuplicateMount { .. }));
}

#[test]
fn add_multicast_mount_returns_handle_with_multicast_kind() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap();
    assert_eq!(mount.mount_path(), "/mc");
    assert!(matches!(mount.mount_kind(), MountKind::Multicast { .. }));
}

#[test]
fn add_multicast_mount_rejects_unicast_group_address() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let e = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://10.0.0.1:5004")
        .unwrap_err();
    assert!(matches!(e, RtspServerError::InvalidMulticastGroup { .. }));
}

#[test]
fn push_video_succeeds_with_no_subscribers() {
    // Pre-PLAY: broadcast has zero receivers. The muxer still accepts
    // the push; drain-and-broadcast silently absorbs the no-subscribers
    // error from broadcast::Sender::send.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount
        .push_video(&nal, Pts90khz::new(0), true)
        .expect("push succeeds even with no peers");
    assert_eq!(mount.peer_count(), 0);
}

#[test]
fn push_video_updates_stats() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    let initial = mount.stats();
    assert_eq!(initial.bytes_pushed, 0);
    assert_eq!(initial.packets_pushed, 0);
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let after = mount.stats();
    assert!(
        after.bytes_pushed > 0,
        "bytes_pushed should grow after push"
    );
    assert!(
        after.packets_pushed > 0,
        "packets_pushed should grow after push"
    );
}

#[test]
fn mount_handle_clone_shares_state() {
    // Clone semantics: pushing on one clone updates the stats observed
    // through the other clone. This proves the Arc<MountState> is shared
    // rather than deep-copied.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let h1 = server.add_mount("/live", make_muxer_cfg()).unwrap();
    let h2 = h1.clone();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    h1.push_video(&nal, Pts90khz::new(0), true).unwrap();
    assert!(
        h2.stats().bytes_pushed > 0,
        "clone must see writes through the shared state",
    );
    assert_eq!(h1.stats().bytes_pushed, h2.stats().bytes_pushed);
}

#[test]
fn two_add_mount_calls_grow_server_stats_mounts() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    assert_eq!(server.stats().mounts, 0);
    let _a = server.add_mount("/a", make_muxer_cfg()).unwrap();
    assert_eq!(server.stats().mounts, 1);
    let _b = server.add_mount("/b", make_muxer_cfg()).unwrap();
    assert_eq!(server.stats().mounts, 2);
}

#[test]
fn peer_count_zero_without_playing_clients() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    assert_eq!(mount.peer_count(), 0);
    // start() is enough to bind the listener but doesn't subscribe anyone.
    server.start().unwrap();
    assert_eq!(mount.peer_count(), 0);
}

/// `remove_mount` on a local mount ends its PLAY reader's stream and frees
/// the path; the caller's `MountHandle` keeps accepting pushes, which reach
/// nobody.
#[test]
fn remove_local_mount_ends_readers_and_handle_pushes_reach_nobody() {
    use std::time::{Duration, Instant};
    use tst_core::transport::{RecvTransport, TransportError};
    use tst_rtp::RtspClient;

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/live?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let mut reader_t = reader.setup_mp2t_auto(&sdp).unwrap().into_recv_transport();
    reader_t.set_recv_timeout(Some(Duration::from_secs(5)));
    reader.play().unwrap();

    // A second mount with its own reader: removing `/live` must leave it alone.
    let other = server.add_mount("/other", make_muxer_cfg()).unwrap();
    let mut other_reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/other?transport=tcp")).unwrap();
    let other_sdp = other_reader.describe().unwrap();
    let mut other_t = other_reader
        .setup_mp2t_auto(&other_sdp)
        .unwrap()
        .into_recv_transport();
    other_t.set_recv_timeout(Some(Duration::from_millis(500)));
    other_reader.play().unwrap();

    server.remove_mount("/live").unwrap();

    // Nothing was pushed, so the first non-data result is the end; a 5 s
    // recv timeout would show up as `Backpressure`.
    let mut buf = vec![0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(10);
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

    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount
        .push_video(&nal, Pts90khz::new(0), true)
        .expect("a removed mount's handle still accepts pushes");
    assert_eq!(mount.peer_count(), 0);
    assert_eq!(server.stats().mounts, 1);

    // `/other`'s reader still receives: push an IDR per attempt until bytes
    // arrive (its fanout subscription is in place once PLAY returned; the
    // retry only absorbs scheduling), bounded by a deadline.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pts = 0i64;
    let got = loop {
        other
            .push_video(&nal, Pts90khz::new(pts), true)
            .expect("push to /other");
        pts += 3003;
        match other_t.recv_bytes(&mut buf) {
            Ok(n) if n > 0 => break n,
            Err(TransportError::Backpressure { .. }) | Ok(_) if Instant::now() < deadline => {}
            other_end => panic!("/other's reader ended after removing /live: {other_end:?}"),
        }
    };
    assert!(got > 0);
    server
        .add_mount("/live", make_muxer_cfg())
        .expect("the path is free again");
    server.stop().ok();
    let e = server.remove_mount("/live").unwrap_err();
    assert!(matches!(e, RtspServerError::Shutdown), "got {e:?}");
}
