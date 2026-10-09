//! A TCP-interleaved PLAY reader that sends RTCP receiver reports as `$`
//! frames on its RTCP channel (RFC 7826 §14; live555, GStreamer rtspsrc and
//! VLC all do when forced to TCP) must keep its session: the next RTSP
//! request is answered 200, not 413. Before the fix only publisher sessions
//! drained `$` frames; a reader's RR bytes stayed at the buffer head and
//! were framed as non-UTF-8 RTSP headers.

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp::{header, make_muxer_cfg, request, session_id};

/// The RTCP channel of an `interleaved=A-B` Transport answer.
fn rtcp_channel_of(transport: &str) -> u8 {
    let pair = transport
        .split("interleaved=")
        .nth(1)
        .expect("interleaved= in Transport");
    let (_rtp, rest) = pair.split_once('-').expect("A-B pair");
    rest.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("channel number")
}

#[test]
fn a_tcp_reader_that_sends_rtcp_receiver_reports_keeps_its_session() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().expect("bound").port();

    let mut tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let sid = session_id(&setup);
    let rtcp_ch = rtcp_channel_of(header(&setup, "Transport"));
    let play = request(
        &mut tcp,
        &format!("PLAY rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");

    // RFC 3550 §6.4.2 receiver report with no report blocks: V=2 RC=0
    // PT=201 length=1 (8 bytes), SSRC 0xDEADBEEF — framed per RFC 7826 §14.
    let rr = [0x80u8, 0xC9, 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF];
    let mut frame = vec![b'$', rtcp_ch, 0, rr.len() as u8];
    frame.extend_from_slice(&rr);
    tcp.write_all(&frame).expect("write RR");

    // The reader's keepalive. Before the fix: `RTSP/1.0 413` + FIN.
    let ping = request(
        &mut tcp,
        &format!(
            "OPTIONS rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n"
        ),
    );
    assert!(
        ping.starts_with("RTSP/1.0 200"),
        "a reader that sent one RTCP receiver report was answered: {ping}"
    );
    let _ = server.stop();
}
