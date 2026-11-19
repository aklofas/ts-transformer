//! TCP-interleaved media wiring, client against our server.
//! Dropping the client writes TEARDOWN (500 ms bounds getting it to the
//! wire), does not wait for the reply and does not wait for the client's
//! thread, so the
//! post-PLAY teardown cannot hang on the server's lingering write-half
//! references after `RtspServer::stop`.
//!
//! Drives a real in-process `tst_rtp::RtspServer` as the client's peer —
//! requires the `rtsp-server` feature.

#![cfg(feature = "rtsp-server")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use crate::fixtures::raw_rtsp::{make_muxer_cfg, scaled};
use crate::fixtures::raw_rtsp_server::{SDP_BODY, cseq_of, method_of, read_request};
use crate::fixtures::rtp_packets::build_rtp_packet;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::transport::RecvTransport;
use tst_pipeline::DemuxReceiver;
use tst_rtp::{RtspClient, RtspServer};

/// `rtsp:// ?transport=tcp` end-to-end: [`RtspClient::play`] succeeds with
/// the client's thread feeding the underlying
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

/// A server may send media right behind its SETUP reply, in the same TCP
/// segment: those frames reach the receiver. The scripted server writes
/// the reply and two RTP packets in one write and never sends another.
#[test]
fn media_sent_with_the_setup_reply_is_delivered() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "DESCRIBE", "unexpected request: {req}");
        let mut describe = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            cseq_of(&req),
            SDP_BODY.len()
        )
        .into_bytes();
        describe.extend_from_slice(SDP_BODY);
        sock.write_all(&describe).unwrap();

        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "SETUP", "unexpected request: {req}");
        let mut bytes = format!(
            "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: DEADBEEF;timeout=60\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n",
            cseq_of(&req)
        )
        .into_bytes();
        for seq in [1u16, 2] {
            // One TS packet whose payload names the RTP packet it rode in.
            let mut ts = vec![seq as u8; 188];
            ts[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
            let rtp = build_rtp_packet(seq, 90_000, 0x1234_5678, 33, false, &ts);
            bytes.extend_from_slice(&[b'$', 2]);
            bytes.extend_from_slice(&u16::try_from(rtp.len()).unwrap().to_be_bytes());
            bytes.extend_from_slice(&rtp);
        }
        sock.write_all(&bytes).unwrap();
        // Hold the connection until the client hangs up.
        let _ = sock.read_to_end(&mut Vec::new());
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    // A safety net only: a lost frame fails with a timeout error.
    transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    let mut buf = vec![0u8; transport.max_payload()];
    for seq in [1u8, 2] {
        let n = transport
            .recv_bytes(&mut buf)
            .unwrap_or_else(|e| panic!("packet {seq} was not delivered: {e:?}"));
        assert_eq!(n, 188, "packet {seq}");
        assert_eq!(buf[4], seq, "packet {seq}");
    }
    drop(transport);
    drop(client);
    server.join().unwrap();
}
