//! A sender report the server writes on interleaved channel 1 reaches
//! `RtpRecvTransport::rtcp_stats()`: the client's pump routes the frame to
//! the RTCP ingest thread, which counts it.

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tst_rtp::{RtspClient, SenderReport};

use crate::fixtures::raw_rtsp_server::{
    answer_describe_and_setup, cseq_of, method_of, read_request,
};

#[test]
fn a_sender_report_on_channel_1_reaches_rtcp_stats() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "PLAY", "unexpected request: {req}");
        let cseq = cseq_of(&req);
        sock.write_all(
            format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n").as_bytes(),
        )
        .unwrap();
        let sr = SenderReport {
            ssrc: 0x1234_5678,
            ntp_timestamp: 0xE000_0000_8000_0000,
            rtp_timestamp: 90_000,
            sender_packet_count: 10,
            sender_octet_count: 1_880,
            report_blocks: vec![],
        }
        .encode()
        .unwrap();
        let mut frame = vec![b'$', 1];
        frame.extend_from_slice(&(sr.len() as u16).to_be_bytes());
        frame.extend_from_slice(&sr);
        sock.write_all(&frame).unwrap();
        // Hold the connection open until the client has read the frame.
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let rx = session.into_recv_transport();
    client.play().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while rx.rtcp_stats().sr_packets_received == 0 {
        assert!(
            Instant::now() < deadline,
            "the channel-1 sender report never reached rtcp_stats()"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rx.rtcp_stats().sr_packets_received, 1);
    done_tx.send(()).unwrap();
    drop(client);
    server.join().unwrap();
}
