//! `RtspPush` against this library's own server: an MPEG-TS push reaches a
//! publish mount's application transport byte for byte, an elementary
//! push delivers its access units and KLV units, and closing a push frees
//! the mount.

use std::time::{Duration, Instant};

use base64::Engine as _;
use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::RecvTransport;
use tst_rtp::{PushDescription, RtspClientBuilder, RtspPush, RtspServer, VideoTrack};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::{SPROP_PPS_B64, SPROP_SPS_B64, ts_fixture_packets};

fn push_to(port: u16, desc: &PushDescription) -> RtspPush {
    let builder = RtspClientBuilder::new(&format!("rtsp://127.0.0.1:{port}/pub")).unwrap();
    RtspPush::connect(builder, desc).unwrap()
}

#[test]
fn an_mp2t_push_reaches_the_publish_mount_and_close_frees_it() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app = mount.clone().into_recv_transport().unwrap();
    app.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

    let mut push = push_to(port, &PushDescription::Mp2t);
    assert!(mount.publisher().is_some());
    let ts: Vec<u8> = ts_fixture_packets(210).concat();
    for bundle in ts.chunks(7 * 188) {
        push.send_ts(bundle).unwrap();
    }

    let mut got = Vec::with_capacity(ts.len());
    let mut buf = vec![0u8; 65536];
    while got.len() < ts.len() {
        let n = app
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("after {} bytes: {e:?}", got.len()));
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got == ts, "the mount's stream differs from the pushed TS");
    assert_eq!(mount.stats().malformed_packets, 0);

    push.close().unwrap();
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    while mount.publisher().is_some() {
        assert!(Instant::now() < give_up, "the closed push kept the mount");
        std::thread::sleep(Duration::from_millis(5));
    }
    server.stop().ok();
}

#[test]
fn an_h264_and_klv_push_delivers_its_access_units_and_klv_units() {
    const UNITS: u64 = 30;
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let b64 = base64::engine::general_purpose::STANDARD;
    let sps = b64.decode(SPROP_SPS_B64).unwrap();
    let pps = b64.decode(SPROP_PPS_B64).unwrap();
    let video = VideoTrack::H264 {
        parameter_sets: vec![sps.clone(), pps.clone()],
    };
    let mut push = push_to(port, &PushDescription::Elementary { video, klv: true });
    for i in 0..UNITS {
        let pts = Pts90khz::new(i as i64 * 3003);
        // A key frame with its parameter sets first, then P slices, each
        // large enough to be fragmented.
        let mut au = Vec::new();
        let slice_type = if i == 0 { 0x65 } else { 0x41 };
        let slice: Vec<u8> = std::iter::once(slice_type)
            .chain((0..3_000).map(|k| (k % 250) as u8 + 1))
            .collect();
        let nals: &[&[u8]] = if i == 0 {
            &[&sps, &pps, &slice]
        } else {
            &[&slice]
        };
        for nal in nals {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(nal);
        }
        push.send_video(&au, pts).unwrap();
        push.send_klv(&[0x06, 0x0E, 0x2B, 0x34, i as u8], pts)
            .unwrap();
    }
    // The last access unit is held until the next one starts.
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    loop {
        let stats = mount.stats();
        if stats.aus_emitted >= UNITS - 1 && stats.klv_units_emitted >= UNITS - 1 {
            break;
        }
        assert!(Instant::now() < give_up, "the mount stopped at {stats:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(mount.stats().malformed_packets, 0);
    push.close().unwrap();
    server.stop().ok();
}
