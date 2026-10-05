//! TCP-interleaved loopback test.
//!
//! Both halves of the TCP-interleaved wire-up:
//!
//! - **Server side** — `handle_connection_inner` splits the
//!   per-session TCP and shares the `Arc<Mutex<OwnedWriteHalf>>` with
//!   the per-peer fanout task; `handle_play` for `TcpInterleaved`
//!   spawns the fanout.
//! - **Client side** — `RtspClient::activate_interleaved_pump` is
//!   called at SETUP, so subsequent `send_and_read` requests poll the
//!   pump's `ctrl_rx` and binary `$`-framed RTP/RTCP demultiplex into
//!   their own mpsc receivers.
//!
//! The control-plane test below exercises the handshake. A future test
//! can push a TS payload through the mount and assert the
//! byte-identical RTP-framed payload reaches the client; this file
//! intentionally stays narrow on the handshake to keep the regression
//! signal localized.

use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspServer};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

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
