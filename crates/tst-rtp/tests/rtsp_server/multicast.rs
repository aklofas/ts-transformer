//! Multicast mount integration tests.

use std::time::{Duration, Instant};

use crate::fixtures::raw_rtsp::{make_muxer_cfg, scaled};
use tst_core::mpegts::common::Pts90khz;
use tst_rtp::{MountHandle, MountKind, RtspServer};

/// Waits until the mount's multicast sender task has built its socket and
/// subscribed to the fanout, so that `start()` is called on a warm server.
///
/// `add_multicast_mount` spawns that task on the server runtime at once.
/// Under TSan, the task's UDP socket registration trips the suppressed
/// tokio io-driver race (`race:tokio::runtime::io::scheduled_io`), and
/// every suppressed report still symbolises a stack, which stalls the
/// runtime's threads for seconds on a CI runner. With the mount added
/// right before `start()`, that stall sat inside `start()`'s 5 s
/// startup-report bound and the test died with `Io(TimedOut)` (ci-flakes
/// ledger, closed class "TSan `net_rtsp_server multicast::*` tests fail
/// at `server.start()`"). Warming the mount first keeps the bound
/// measuring only the listener's bind, which is what it exists for; a
/// sender task that never comes up still fails here, on its own margin.
/// The stall itself now lands in this wait: about 2.5 s per test under
/// TSan on a fast box, several seconds on a CI runner, so the margin is
/// ten seconds before scaling.
fn warm_mount(mount: &MountHandle) {
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    while mount.peer_count() == 0 {
        assert!(
            Instant::now() < deadline,
            "the multicast sender task never subscribed to the mount"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Multicast mount registered + visible in SDP via DESCRIBE.
#[test]
fn multicast_mount_describes_with_group_in_sdp() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap();
    assert!(matches!(mount.mount_kind(), MountKind::Multicast { .. }));
    warm_mount(&mount);
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/mc");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let sdp = client.describe().unwrap();
    // SDP should contain the multicast group address.
    let session_c = sdp.session_connection.as_deref().unwrap_or_default();
    assert!(
        session_c.contains("239.0.0.1"),
        "expected multicast group in c= line; got {sdp:?}"
    );
    server.stop().ok();
}

/// Multicast SETUP succeeds end-to-end against the real server: the
/// client's SETUP URI carries the SDP control suffix (`/trackID=0`),
/// which the server's `extract_mount_path` strips before the mount
/// lookup (see `crates/tst-rtp/src/rtsp/server/handlers.rs`).
///
/// The multicast Transport response shape is unit-tested in
/// `handlers.rs::setup_multicast_mount_returns_multicast_transport`;
/// this integration test proves the full client↔server SETUP path.
#[test]
fn multicast_setup_returns_multicast_transport() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap();
    warm_mount(&mount);
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/mc");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();
    // The session was created — that's the success signal for v1.
    server.stop().ok();
}

/// TCP-interleaved SETUP against a multicast mount → 461 Unsupported
/// Transport per RFC 7826 §13.3, asserted as the exact typed error
/// (`RtspError::Protocol { code: 461, .. }`) — a 404, auth failure, or
/// connection error must NOT satisfy this test. `?transport=tcp` forces
/// TCP so the client's UDP→TCP 461-fallback path cannot mask the
/// rejection. The handler-level twin is
/// `handlers.rs::setup_against_multicast_mount_rejects_tcp_with_461`.
#[test]
fn multicast_rejects_tcp_interleaved_with_461() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap();
    warm_mount(&mount);
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/mc?transport=tcp");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let sdp = client.describe().unwrap();
    let result = client.setup_mp2t_auto(&sdp);
    match result {
        Err(tst_rtp::RtspError::Protocol { code: 461, .. }) => {}
        Err(other) => panic!("expected RtspError::Protocol {{ code: 461 }}, got {other:?}"),
        Ok(_) => panic!("expected SETUP to fail with 461 Unsupported Transport, got Ok(_)"),
    }
    server.stop().ok();
}

/// MountHandle push_video on multicast mount drains successfully (the
/// per-mount sender task drives the multicast UDP socket).
#[test]
fn multicast_mount_push_video_succeeds() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap();
    warm_mount(&mount);
    server.start().unwrap();
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount.push_video(&nal, Pts90khz::new(0), true).unwrap();
    let stats = mount.stats();
    assert!(stats.bytes_pushed > 0);
    server.stop().ok();
}
