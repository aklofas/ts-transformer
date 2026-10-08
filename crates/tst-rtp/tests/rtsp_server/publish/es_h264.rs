//! Elementary H.264 publisher in ffmpeg's shape (spec appendix A): one
//! RFC 6184 track with `sprop-parameter-sets` in the SDP, `streamid=0`,
//! RECORD with `Range: npt=0.000-`, an RTCP sender report first. The
//! server re-muxes it into TS (video PID 0x100) for the application's
//! `DemuxReceiver` and for PLAY readers.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use base64::Engine as _;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_pipeline::DemuxReceiver;
use tst_rtp::{ClockAlignment, RtspClient, RtspServer};

use crate::fixtures::raw_rtsp_publisher::*;

const VIDEO_PID: u16 = 0x100;
const PT: u8 = 96;
const SSRC: u32 = 0x0F0F_1234;
const TS0: u32 = 1_000_000;
/// 28 AUs: a 3-fragment IDR plus 27 single-packet P slices = 30 packets.
const AUS: usize = 28;

/// A demuxed video sample: `(pts ticks, Annex-B bytes, random access)`.
type VideoSample = (i64, Vec<u8>, bool);

fn video_samples(events: &[DemuxEvent]) -> Vec<VideoSample> {
    events
        .iter()
        .filter_map(|ev| match ev {
            DemuxEvent::Sample {
                stream,
                pts,
                payload:
                    SamplePayload::Video {
                        raw,
                        random_access_indicator,
                        ..
                    },
                ..
            } if stream.pid == VIDEO_PID => {
                Some((pts.as_ticks(), raw.to_vec(), *random_access_indicator))
            }
            _ => None,
        })
        .collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The checks every delivery of the 28-AU fixture must pass: at least 10
/// samples, the first a random-access IDR carrying the SDP's SPS and PPS
/// (injected by the server from `sprop-parameter-sets`, never sent
/// in-band), and PTS stepping by exactly one AU interval.
fn assert_video_delivery(samples: &[VideoSample], who: &str) {
    assert!(
        samples.len() >= 10,
        "{who}: only {} video samples",
        samples.len()
    );
    let (_, first, rai) = &samples[0];
    assert!(*rai, "{who}: the first sample is not a random-access point");
    let b64 = base64::engine::general_purpose::STANDARD;
    let sps = b64.decode(SPROP_SPS_B64).unwrap();
    let pps = b64.decode(SPROP_PPS_B64).unwrap();
    assert!(contains(first, &sps), "{who}: first AU lacks the sprop SPS");
    assert!(contains(first, &pps), "{who}: first AU lacks the sprop PPS");
    assert!(
        contains(first, &[0, 0, 1, 0x65]),
        "{who}: first AU lacks the IDR slice"
    );
    for (i, w) in samples.windows(2).enumerate() {
        assert_eq!(
            w[1].0 - w[0].0,
            i64::from(H264_AU_STEP),
            "{who}: PTS step after sample {i}"
        );
        assert!(!w[1].2, "{who}: P slice {} flagged random access", i + 1);
    }
}

/// ffmpeg over TCP-interleaved: the application's `DemuxReceiver` and a
/// PLAY reader through our own client both demux the re-muxed video, and
/// they demux the same samples.
#[test]
fn ffmpeg_shaped_h264_publisher_reaches_app_and_reader() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app_t = mount.clone().into_recv_transport().unwrap();
    app_t.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut app = DemuxReceiver::new(app_t);

    // Reader first, so it is subscribed to the fanout before the publisher sends.
    let mut reader =
        RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/pub?transport=tcp")).unwrap();
    let sdp = reader.describe().unwrap();
    let sess = reader.setup_mp2t_auto(&sdp).unwrap();
    let mut reader_t = sess.into_recv_transport();
    reader_t.set_recv_timeout(Some(Duration::from_secs(2)));
    reader.play().unwrap();
    let mut reader_rx = DemuxReceiver::new(reader_t);

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_H264), 200);
    let (status, pair) = p.setup_interleaved_pair("/pub", "streamid=0");
    assert_eq!(status, 200);
    let (rtp_ch, rtcp_ch) = pair.expect("server allocated an interleaved pair");
    assert_eq!(p.record_from_start("/pub"), 200);

    // ffmpeg's first interleaved frame is an RTCP SR on the RTCP channel.
    p.send_frame(rtcp_ch, &sr_packet(SSRC, 3_900_000_000, 0, TS0));
    let packets = h264_rtp_packets(AUS, PT, 100, SSRC, TS0);
    assert_eq!(packets.len(), 30);
    for pkt in &packets {
        p.send_frame(rtp_ch, pkt);
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let app_samples = video_samples(&demux_until_quiet(&mut app, deadline, "app"));
    assert_video_delivery(&app_samples, "app");
    let reader_samples = video_samples(&demux_until_quiet(&mut reader_rx, deadline, "reader"));
    assert_video_delivery(&reader_samples, "reader");
    assert!(
        reader_samples == app_samples,
        "reader demuxed {} samples, app {}; or their bytes differ",
        reader_samples.len(),
        app_samples.len()
    );

    let stats = mount.stats();
    assert_eq!(stats.aus_emitted, AUS as u64);
    assert_eq!(stats.aus_dropped, 0);
    assert_eq!(stats.malformed_packets, 0);
    assert_eq!(stats.rtp_packets_received, 30);
    assert_eq!(stats.alignment, ClockAlignment::NotApplicable);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}

/// ffmpeg over UDP: the SR goes to the server's RTCP port and the video
/// to its RTP port, both from ONE socket whose port differs from the
/// announced `client_port` (the server latches the source from the first
/// valid RTP datagram). Loopback UDP may drop under load, so only the
/// delivery shape is asserted, not an exact count.
#[test]
fn ffmpeg_shaped_h264_publisher_over_udp_reaches_app() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app_t = mount.clone().into_recv_transport().unwrap();
    app_t.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut app = DemuxReceiver::new(app_t);

    let announced = UdpSocket::bind("127.0.0.1:0").unwrap();
    let announced_port = announced.local_addr().unwrap().port();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    assert_ne!(sender.local_addr().unwrap().port(), announced_port);

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_H264), 200);
    let (status, server_port) = p.setup_udp("/pub", "streamid=0", announced_port);
    assert_eq!(status, 200);
    let (server_rtp, server_rtcp) = server_port.expect("server_port in Transport");
    assert_eq!(p.record_from_start("/pub"), 200);

    sender
        .send_to(
            &sr_packet(SSRC, 3_900_000_000, 0, TS0),
            ("127.0.0.1", server_rtcp),
        )
        .unwrap();
    for pkt in &h264_rtp_packets(AUS, PT, 100, SSRC, TS0) {
        sender.send_to(pkt, ("127.0.0.1", server_rtp)).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let samples = video_samples(&demux_until_quiet(&mut app, deadline, "app"));
    assert!(
        samples.len() >= 10,
        "only {} video samples over UDP",
        samples.len()
    );
    assert!(
        samples[0].2,
        "the first UDP sample is not a random-access point"
    );
    let stats = mount.stats();
    assert_eq!(stats.malformed_packets, 0);
    assert_eq!(stats.source_rejected, 0);
    assert_eq!(p.teardown("/pub"), 200);
    server.stop().ok();
}
