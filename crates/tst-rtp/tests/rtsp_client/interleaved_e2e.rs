//! TCP-interleaved producer thread wiring, client against our server. The
//! bounded teardown deadline in [`RtspClient::Drop`] keeps the post-PLAY
//! teardown from hanging on the server's lingering write-half references
//! after `RtspServer::stop`.
//!
//! Drives a real in-process `tst_rtp::RtspServer` as the client's peer —
//! requires the `rtsp-server` feature.

#![cfg(feature = "rtsp-server")]

use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::DemuxReceiver;
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
/// [`RtpRecvTransport`](tst_rtp::RtpRecvTransport), a frame pushed through
/// the mount comes back out of a [`DemuxReceiver`] on the client as the
/// same bytes (the `$`-framed RTP/MP2T path is byte-transparent), and
/// teardown completes.
#[test]
fn tcp_interleaved_end_to_end_round_trips_ts_bytes() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live?transport=tcp");

    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    // Bounded: a relay that delivers nothing fails here with a timeout
    // error instead of parking the test.
    transport.set_recv_timeout(Some(Duration::from_secs(5)));
    client.play().unwrap();

    // One IDR access unit with a payload no TS header or PSI section can
    // contain by accident: a 4-byte start code, an IDR NAL header, then
    // a counter-filled body long enough to span several TS packets.
    let mut nal = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    nal.extend((0u16..600).map(|i| (i % 251) as u8 + 1));
    // The fanout is registered at PLAY; push a few copies so the first
    // one landing before the peer is wired up cannot leave the client
    // with nothing to demux.
    for i in 0..3 {
        mount
            .push_video(&nal, Pts90khz::new(i * 3000), true)
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut demux = DemuxReceiver::new(transport);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let received = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no video sample reached the client within 10 s"
        );
        match demux.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => break raw.to_vec(),
            Ok(Some(_)) => continue,
            Ok(None) => panic!("stream ended before a video sample arrived"),
            Err(e) => panic!("demux receiver failed before a video sample arrived: {e:?}"),
        }
    };
    assert_eq!(
        received, nal,
        "the access unit must come back byte-identical through the interleaved path"
    );

    drop(demux);
    drop(client);
    server.stop().ok();
}
