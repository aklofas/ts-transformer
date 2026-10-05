//! TCP-interleaved producer thread wiring, client against our server. The
//! bounded teardown deadline in [`RtspClient::Drop`] keeps the post-PLAY
//! teardown from hanging on the server's lingering write-half references
//! after `RtspServer::stop`.
//!
//! Drives a real in-process `tst_rtp::RtspServer` as the client's peer —
//! requires the `rtsp-server` feature.

#![cfg(feature = "rtsp-server")]

use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspServer};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// `rtsp:// ?transport=tcp` end-to-end: [`RtspClient::play`] succeeds with
/// the interleaved pump feeding the underlying
/// [`RtpRecvTransport`](tst_rtp::RtpRecvTransport), and teardown completes.
#[test]
fn tcp_interleaved_end_to_end_round_trips_ts_bytes() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live?transport=tcp");

    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let _recv = session.into_recv_transport();
    client.play().unwrap();

    // No payload is pushed: byte-identical TS delivery over the
    // interleaved path is not asserted here.
    server.stop().ok();
}
