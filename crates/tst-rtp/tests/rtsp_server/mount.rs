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
