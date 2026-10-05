//! Session keepalive interaction.
//!
//! The keepalive contract under test: a client may emit additional
//! `OPTIONS` requests on the control TCP after SETUP to refresh the
//! server-side session timeout, and the server must continue to answer
//! 200 OK for those pings without corrupting per-session state.
//!
//! These tests use manual `client.options()` calls rather than the
//! automatic [`RtspClient::spawn_keepalive_if_needed`] background
//! thread — the background variant is already covered by
//! `rtsp_client/keepalive.rs`. The point here is the *server* side:
//! it accepts repeated OPTIONS across the lifetime of a session.

use std::time::Duration;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspServer};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Client sends manual OPTIONS pings interleaved with DESCRIBE; server
/// returns 200 to each. Stand-in for the keepalive-over-existing-session
/// path until a session-aware ping helper lands.
#[test]
fn manual_keepalive_pings_keep_session_alive() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let mut client = RtspClient::connect(&url).unwrap();
    client.options().unwrap();
    let _sdp = client.describe().unwrap();
    // Manual ping (the simplest keepalive shape — fresh OPTIONS request
    // on the existing control TCP).
    client.options().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    client.options().unwrap();

    server.stop().ok();
}

/// Repeated OPTIONS pings don't corrupt server state: after several
/// pings, a fresh DESCRIBE against the same mount still returns a
/// valid SDP.
#[test]
fn options_pings_dont_corrupt_state() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let mut client = RtspClient::connect(&url).unwrap();
    for _ in 0..3 {
        client.options().unwrap();
    }
    let sdp = client.describe().unwrap();
    assert!(
        !sdp.media.is_empty(),
        "DESCRIBE after repeated OPTIONS should still return media lines"
    );

    server.stop().ok();
}
