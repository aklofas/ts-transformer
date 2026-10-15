//! Publisher-side `$` frames on the RTSP control connection: frames
//! written in the same TCP stream as RTSP requests reach the publish
//! mount, the requests around them are still framed and answered, a frame
//! split across two writes reassembles, an unknown channel is counted
//! rather than fatal, and a frame's binary payload is never parsed as
//! RTSP.
//!
//! These drove `handle_connection` against a test-only `ServerState` as
//! inline `rtsp::server::session` tests; they now drive the same session
//! loop through `RtspServer` and observe it through `PublishMountHandle`.
//! Assertions are unchanged. The paused-time idle-bound tests stay inline.

use std::time::Duration;

use tst_core::transport::RecvTransport;
use tst_rtp::{PublishMountHandle, RtpRecvTransport, RtspServer};

use crate::fixtures::raw_rtsp::header;
use crate::fixtures::raw_rtsp_publisher::{RawPublisher, SDP_MP2T, frame};

/// A started server with one `/pub` publish mount: `(server, mount, port)`.
fn server_with_publish_mount() -> (RtspServer, PublishMountHandle, u16) {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    (server, mount, port)
}

/// One RTP packet (PT 33, the MP2T adapter's PT for `SDP_MP2T`) carrying
/// `n` TS packets (sync byte + 187 zero bytes each) as its payload.
fn rtp_mp2t_packet(n: usize) -> Vec<u8> {
    let mut v = vec![0x80u8, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
    for _ in 0..n {
        v.push(0x47);
        v.extend(std::iter::repeat_n(0u8, 187));
    }
    v
}

/// One payload from the application transport, bounded by a 5 s recv
/// timeout so a frame that never arrives fails instead of hanging.
fn app_recv(app: &mut RtpRecvTransport) -> Vec<u8> {
    app.set_recv_timeout(Some(Duration::from_secs(5)));
    let mut buf = vec![0u8; 4096];
    let n = app.recv_bytes(&mut buf).expect("a frame reached the app");
    buf.truncate(n);
    buf
}

/// ANNOUNCE → SETUP (`mode=record`, interleaved); returns the publisher and
/// the `(rtp, rtcp)` channel pair the server granted (read back, not assumed).
fn announce_setup(port: u16) -> (RawPublisher, (u8, u8)) {
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200, "ANNOUNCE");
    let (status, pair) = p.setup_interleaved_pair("/pub", "streamid=0");
    assert_eq!(status, 200, "SETUP");
    (p, pair.expect("interleaved pair in the SETUP answer"))
}

/// [`announce_setup`] then RECORD.
fn announce_setup_record(port: u16) -> (RawPublisher, (u8, u8)) {
    let (mut p, pair) = announce_setup(port);
    assert_eq!(p.record("/pub"), 200, "RECORD");
    (p, pair)
}

/// `method` on `/pub` with this publisher's session, as raw request text
/// for a pipelined write.
fn request_text(p: &RawPublisher, method: &str, cseq: u32) -> String {
    format!(
        "{method} {} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {}\r\n\r\n",
        p.uri("/pub"),
        p.session_id()
    )
}

#[test]
fn interleaved_frames_reach_the_publish_mount_and_a_split_frame_reassembles() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let (mut p, (rtp_ch, rtcp_ch)) = announce_setup_record(port);

    // Frame 1: RTCP SR on the session's RTCP channel (ignored by the MP2T
    // adapter), frame 2: RTP on the session's RTP channel — written in two
    // halves with a pipelined OPTIONS behind it, to exercise the
    // "incomplete frame waits for the next read" path.
    let rtp = rtp_mp2t_packet(2);
    let sr: [u8; 24] = [
        0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let mut wire = frame(rtcp_ch, &sr);
    wire.extend(frame(rtp_ch, &rtp));
    let split = wire.len() - 100;
    p.write_raw(&wire[..split]);
    std::thread::sleep(Duration::from_millis(50));
    let mut tail = wire[split..].to_vec();
    tail.extend_from_slice(request_text(&p, "OPTIONS", 5).as_bytes());
    p.write_raw(&tail);
    let r = p.read_responses(1);
    assert!(
        r.starts_with("RTSP/1.0 200") && header(&r, "CSeq") == "5",
        "{r}"
    );
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());
    assert_eq!(mount.stats().rtp_packets_received, 1);
}

#[test]
fn unknown_interleaved_channel_is_counted_not_fatal() {
    let (_server, mount, port) = server_with_publish_mount();
    let (mut p, (rtp_ch, rtcp_ch)) = announce_setup_record(port);
    // Guaranteed distinct from both allocated channels regardless of
    // their value: +3 and +2 (mod 256) are both nonzero offsets from the
    // rtcp channel (rtp_ch + 1), and +3 is a nonzero offset from rtp_ch.
    let unknown_ch = rtp_ch.wrapping_add(3);
    assert_ne!(unknown_ch, rtp_ch);
    assert_ne!(unknown_ch, rtcp_ch);

    let mut wire = frame(unknown_ch, &rtp_mp2t_packet(1));
    wire.extend_from_slice(request_text(&p, "OPTIONS", 5).as_bytes());
    p.write_raw(&wire);
    let r = p.read_responses(1);
    assert!(
        r.starts_with("RTSP/1.0 200") && header(&r, "CSeq") == "5",
        "{r}"
    );
    assert_eq!(mount.stats().malformed_packets, 1);
}

#[test]
fn frames_before_record_are_accepted() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let (mut p, (rtp_ch, _)) = announce_setup(port);

    // A frame lands BEFORE RECORD — the publish session already exists
    // from ANNOUNCE and SETUP has already allocated the channel, so the
    // `$` arm must route it even though RECORD has not been seen
    // (RFC 2326 §10.11 gates RECORD on SETUP, not the reverse).
    let rtp = rtp_mp2t_packet(1);
    let mut wire = frame(rtp_ch, &rtp);
    wire.extend_from_slice(request_text(&p, "RECORD", 4).as_bytes());
    p.write_raw(&wire);
    let r = p.read_responses(1);
    assert!(
        r.starts_with("RTSP/1.0 200") && header(&r, "CSeq") == "4",
        "{r}"
    );
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());
    assert_eq!(mount.stats().rtp_packets_received, 1);
}

/// `[OPTIONS][$frame][OPTIONS]` in one write: the frame between the two
/// requests is drained before the second request is framed, so both
/// requests are answered 200 and the frame is delivered.
#[test]
fn frame_pipelined_between_two_requests_is_delivered_and_both_answered() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let (mut p, (rtp_ch, _)) = announce_setup_record(port);

    let rtp = rtp_mp2t_packet(2);
    let mut wire = request_text(&p, "OPTIONS", 5).into_bytes();
    wire.extend(frame(rtp_ch, &rtp));
    wire.extend_from_slice(request_text(&p, "OPTIONS", 6).as_bytes());
    p.write_raw(&wire);
    let r = p.read_responses(2);
    let statuses: Vec<&str> = r.lines().filter(|l| l.starts_with("RTSP/1.0 ")).collect();
    assert_eq!(statuses, ["RTSP/1.0 200 OK", "RTSP/1.0 200 OK"], "{r}");
    assert!(r.to_ascii_lowercase().contains("cseq: 5"), "{r}");
    assert!(r.to_ascii_lowercase().contains("cseq: 6"), "{r}");
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());
    assert_eq!(mount.stats().rtp_packets_received, 1);
}

/// `[RECORD][$frame]` in one write, then silence: the frame behind the
/// request is routed in the same pass, not left in the buffer until a
/// next read that never comes.
#[test]
fn frame_behind_record_is_delivered_without_a_further_read() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let (mut p, (rtp_ch, _)) = announce_setup(port);

    let rtp = rtp_mp2t_packet(1);
    let mut wire = request_text(&p, "RECORD", 4).into_bytes();
    wire.extend(frame(rtp_ch, &rtp));
    p.write_raw(&wire);
    let r = p.read_responses(1);
    assert!(r.starts_with("RTSP/1.0 200"), "RECORD: {r}");
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());
}

/// A frame whose binary payload contains `\r\n\r\n` (and bytes that are
/// not UTF-8) positioned right after a request is routed as a frame: no
/// 400, no 413, and the connection stays framed for the next request.
#[test]
fn frame_payload_with_crlfcrlf_after_a_request_is_not_parsed_as_rtsp() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let (mut p, (rtp_ch, _)) = announce_setup_record(port);

    let mut rtp = rtp_mp2t_packet(1);
    // Inside the TS packet body (after the 0x47 sync byte): a non-UTF-8
    // byte run, then the RTSP header terminator.
    rtp[12 + 10..12 + 16].copy_from_slice(&[0xFF, 0xFE, b'\r', b'\n', b'\r', b'\n']);
    let mut wire = request_text(&p, "OPTIONS", 5).into_bytes();
    wire.extend(frame(rtp_ch, &rtp));
    p.write_raw(&wire);
    let r = p.read_responses(1);
    assert!(r.starts_with("RTSP/1.0 200"), "OPTIONS: {r}");
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());

    let req = request_text(&p, "OPTIONS", 6);
    p.write_raw(req.as_bytes());
    let r = p.read_responses(1);
    assert!(r.starts_with("RTSP/1.0 200"), "follow-up OPTIONS: {r}");
    assert_eq!(mount.stats().malformed_packets, 0);
}

/// A publisher that sends on the interleaved channels it requested,
/// without reading the SETUP answer, still has its frames routed: the
/// server honours a requested pair that is free in the session.
#[test]
fn publisher_requested_interleaved_channels_are_honoured() {
    let (_server, mount, port) = server_with_publish_mount();
    let mut app = mount.clone().into_recv_transport().unwrap();
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200, "ANNOUNCE");

    // SETUP asking for 6-7, RECORD and a frame on channel 6 in one write:
    // the frame is sent before either answer is read.
    let rtp = rtp_mp2t_packet(1);
    let mut wire = format!(
        "SETUP {} RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=6-7;mode=record\r\n\r\nRECORD {} RTSP/1.0\r\nCSeq: 4\r\n\r\n",
        p.uri("/pub/streamid=0"),
        p.uri("/pub"),
    )
    .into_bytes();
    wire.extend(frame(6, &rtp));
    p.write_raw(&wire);
    let r = p.read_responses(2);
    assert!(r.contains("interleaved=6-7"), "{r}");
    assert_eq!(app_recv(&mut app), rtp[12..].to_vec());
    assert_eq!(mount.stats().malformed_packets, 0);
}
