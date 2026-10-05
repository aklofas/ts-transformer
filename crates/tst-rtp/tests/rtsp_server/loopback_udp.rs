//! UDP loopback round-trip test.
//!
//! Our [`RtspClient`] against our [`RtspServer`] over plain UDP. The
//! tests exercise the full client-driven OPTIONS → DESCRIBE → SETUP →
//! PLAY handshake against the real server runtime and check that
//! `MountHandle::stats` + `ServerStats` reflect the activity.
//!
//! Full RTP byte-flow assertion (push_video → DemuxReceiver yields the
//! same frame) is NOT covered here — `spawn_peer_fanout` for the
//! Unicast UDP variant runs on the server runtime; this test verifies
//! the handshake + control-plane wire-up only.

use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspServer};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Sanity: server bind + add_mount + start + client.connect + describe
/// returns a valid SDP advertising the MP2T payload type.
#[test]
fn client_describes_server_mount_returns_mp2t_sdp() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let mut client = RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let sdp = client.describe().unwrap();
    // SDP should advertise the canonical MP2T media line.
    assert!(
        sdp.media.iter().any(|m| m.payload_types.contains(&33)),
        "expected PT=33 in SDP, got media: {:?}",
        sdp.media
    );

    server.stop().ok();
}

/// SETUP succeeds; PLAY succeeds — full handshake round-trip against
/// our own server runtime over plain UDP.
///
/// The client's `setup_mp2t_auto` appends the SDP `a=control:trackID=0`
/// attribute to the mount URL, producing a SETUP request URI of
/// `rtsp://host:port/live/trackID=0`. The server's `extract_mount_path`
/// (see `crates/tst-rtp/src/rtsp/server/handlers.rs`) strips the trailing
/// `trackID=N` segment before the mount lookup; without that the lookup
/// misses → 404 Not Found. The per-handler unit tests in
/// `handlers.rs::setup_with_udp_transport_returns_200_with_server_port`
/// hand-craft the URI as the bare `/live`, so only this test covers that
/// integration seam.
#[test]
fn client_setup_play_against_server_returns_200() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let mut client = RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    assert_eq!(session.transport_kind(), tst_rtp::RtspTransportKind::Udp);
    client.play().unwrap();

    drop(client); // best-effort TEARDOWN via Drop
    let _ = mount; // keep mount alive until after the client tears down
    server.stop().ok();
}

/// `MountHandle::stats` reflects push activity (bytes_pushed +
/// packets_pushed both increment after a push_video call).
#[test]
fn mount_stats_tick_after_push() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();

    let initial = mount.stats();
    assert_eq!(initial.bytes_pushed, 0);
    assert_eq!(initial.packets_pushed, 0);

    // Minimal NAL — a 4-byte start code followed by a NAL header +
    // RBSP payload. Muxer doesn't parse the payload; only the leading
    // start-code shape matters for `push_video`.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount.push_video(&nal, Pts90khz::new(0), true).unwrap();

    let after = mount.stats();
    assert!(after.bytes_pushed > 0, "bytes_pushed should tick");
    assert!(after.packets_pushed > 0, "packets_pushed should tick");

    server.stop().ok();
}

/// `ServerStats::mounts` reflects the registered mount count; the
/// `active_sessions` counter ticks up when a client connects.
#[test]
fn server_stats_tracks_mounts_and_sessions() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _m1 = server.add_mount("/a", make_muxer_cfg()).unwrap();
    let _m2 = server.add_mount("/b", make_muxer_cfg()).unwrap();
    server.start().unwrap();

    assert_eq!(server.stats().mounts, 2);
    assert_eq!(server.stats().active_sessions, 0);

    let port = server.local_addr().unwrap().port();
    let _client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/a")).unwrap();
    // Brief sleep to let the listener accept + spawn the session task.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        server.stats().active_sessions >= 1,
        "expected ≥1 active session after client connect, got {}",
        server.stats().active_sessions
    );

    server.stop().ok();
}
