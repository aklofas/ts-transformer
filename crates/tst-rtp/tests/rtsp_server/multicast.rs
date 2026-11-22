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

/// A hard cancel stops a multicast mount's sender on a server that was
/// never started: the sender task leaves the mount's fanout, and nothing
/// more reaches the group. Linux only: it needs loopback multicast, and
/// skips (a failure where `TST_REQUIRE_MULTICAST=1`) when the host has
/// none. The group and port are this test's own; no other test uses them.
#[cfg(target_os = "linux")]
#[test]
fn hard_cancel_stops_the_multicast_sender_of_a_never_started_server() {
    use std::net::{Ipv4Addr, UdpSocket};
    use tst_test_helpers::capability::skip_unless_required;
    const GROUP: Ipv4Addr = Ipv4Addr::new(239, 55, 55, 7);
    const PORT: u16 = 55040;
    let sock = match UdpSocket::bind((GROUP, PORT)).and_then(|s| {
        s.join_multicast_v4(&GROUP, &Ipv4Addr::LOCALHOST)
            .map(|()| s)
    }) {
        Ok(s) => s,
        Err(e) => {
            skip_unless_required(
                "MULTICAST",
                format_args!("bind/join failed: {e:?}. Likely no multicast routing on lo."),
            );
            return;
        }
    };
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount(
            "/mc",
            make_muxer_cfg(),
            "rtp://239.55.55.7:55040?ttl=1&iface=127.0.0.1",
        )
        .unwrap();
    warm_mount(&mount);
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut pts = 0i64;
    let mut push = || {
        pts += 3_000;
        mount.push_video(&nal, Pts90khz::new(pts), true)
    };
    let mut buf = [0u8; 2048];

    // Latch: the sender task delivers to the group.
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    loop {
        push().unwrap();
        if sock.recv(&mut buf).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            skip_unless_required(
                "MULTICAST",
                "no datagram reached the group — kernel filter on lo",
            );
            return;
        }
    }

    server.cancel_handle().cancel();
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while push().is_ok() {
        assert!(Instant::now() < deadline, "a push was never refused");
    }
    // The sender task ends and drops its subscription.
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while mount.peer_count() != 0 {
        assert!(
            Instant::now() < deadline,
            "the multicast sender task is still subscribed after the cancel"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Drain what was sent before the cancel.
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    while sock.recv(&mut buf).is_ok() {}
    // Nothing more reaches the group while pushes are attempted.
    let quiet = Instant::now() + Duration::from_millis(300);
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    while Instant::now() < quiet {
        assert!(
            matches!(push(), Err(tst_rtp::MountError::Closed)),
            "a push after the cancel was accepted"
        );
        assert!(
            sock.recv(&mut buf).is_err(),
            "a datagram reached the group after the cancel"
        );
    }
}

/// A socket joined to `group:port` on loopback, or `None` after
/// `skip_unless_required` when the host has no multicast there.
#[cfg(target_os = "linux")]
fn joined(group: std::net::Ipv4Addr, port: u16) -> Option<std::net::UdpSocket> {
    use std::net::{Ipv4Addr, UdpSocket};
    match UdpSocket::bind((group, port)).and_then(|s| {
        s.join_multicast_v4(&group, &Ipv4Addr::LOCALHOST)
            .map(|()| s)
    }) {
        Ok(s) => Some(s),
        Err(e) => {
            tst_test_helpers::capability::skip_unless_required(
                "MULTICAST",
                format_args!("bind/join failed: {e:?}. Likely no multicast routing on lo."),
            );
            None
        }
    }
}

/// Pushes through `mount` until a datagram reaches `sock`, or `false` after
/// `skip_unless_required` when none does (a kernel filter on lo).
#[cfg(target_os = "linux")]
fn latch_delivery(mount: &MountHandle, sock: &std::net::UdpSocket, pts: &mut i64) -> bool {
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut buf = [0u8; 2048];
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    loop {
        *pts += 3_000;
        mount.push_video(&nal, Pts90khz::new(*pts), true).unwrap();
        if sock.recv(&mut buf).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            tst_test_helpers::capability::skip_unless_required(
                "MULTICAST",
                "no datagram reached the group — kernel filter on lo",
            );
            return false;
        }
    }
}

/// Reads what is still in flight to `sock` until it has been quiet for
/// 200 ms.
#[cfg(target_os = "linux")]
fn drain(sock: &std::net::UdpSocket) {
    let mut buf = [0u8; 2048];
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    while sock.recv(&mut buf).is_ok() {}
}

/// A removed multicast mount stops sending to its group: its sender task
/// ends (the mount's subscription goes), its pushes are refused, and
/// nothing more reaches the group.
#[cfg(target_os = "linux")]
#[test]
fn a_removed_multicast_mount_stops_sending() {
    const GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 55, 55, 8);
    let Some(sock) = joined(GROUP, 55042) else {
        return;
    };
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server
        .add_multicast_mount(
            "/mc",
            make_muxer_cfg(),
            "rtp://239.55.55.8:55042?ttl=1&iface=127.0.0.1",
        )
        .unwrap();
    warm_mount(&mount);
    server.start().unwrap();
    let mut pts = 0i64;
    if !latch_delivery(&mount, &sock, &mut pts) {
        return;
    }

    server.remove_mount("/mc").unwrap();
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while mount.peer_count() != 0 {
        assert!(
            Instant::now() < deadline,
            "the multicast sender task is still subscribed after the removal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drain(&sock);
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut buf = [0u8; 2048];
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let quiet = Instant::now() + Duration::from_millis(300);
    while Instant::now() < quiet {
        pts += 3_000;
        let r = mount.push_video(&nal, Pts90khz::new(pts), true);
        assert!(matches!(r, Err(tst_rtp::MountError::Closed)), "{r:?}");
        assert!(
            sock.recv(&mut buf).is_err(),
            "a datagram reached the group after the removal"
        );
    }
    server.stop().ok();
}

/// A multicast mount added again on a removed one's group is the group's
/// only sender: one SSRC reaches the group, whichever handle is pushed.
#[cfg(target_os = "linux")]
#[test]
fn a_multicast_mount_added_again_has_one_sender() {
    const GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 55, 55, 9);
    const URL: &str = "rtp://239.55.55.9:55044?ttl=1&iface=127.0.0.1";
    let Some(sock) = joined(GROUP, 55044) else {
        return;
    };
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let old = server
        .add_multicast_mount("/mc", make_muxer_cfg(), URL)
        .unwrap();
    warm_mount(&old);
    server.start().unwrap();
    let mut pts = 0i64;
    if !latch_delivery(&old, &sock, &mut pts) {
        return;
    }
    server.remove_mount("/mc").unwrap();
    drain(&sock);
    let new = server
        .add_multicast_mount("/mc", make_muxer_cfg(), URL)
        .unwrap();
    warm_mount(&new);

    // Both handles pushed alternately: the old one's pushes are refused
    // (or, if it still sent, its SSRC would be the second one).
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    let mut buf = [0u8; 2048];
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut ssrcs = std::collections::BTreeSet::new();
    let mut received = 0;
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    while received < 40 {
        assert!(Instant::now() < deadline, "only {received} datagrams");
        pts += 3_000;
        let _ = old.push_video(&nal, Pts90khz::new(pts), true);
        new.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        while let Ok(n) = sock.recv(&mut buf) {
            assert!(n >= 12, "a datagram shorter than an RTP header");
            ssrcs.insert(u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]));
            received += 1;
        }
    }
    assert_eq!(ssrcs.len(), 1, "senders on the group: {ssrcs:x?}");
    server.stop().ok();
}
