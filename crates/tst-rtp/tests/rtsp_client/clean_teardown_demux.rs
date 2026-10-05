//! Clean RTSP teardown must end the `DemuxReceiver` path as a
//! clean `Ok(None)` end-of-stream, not a `TransportBroken` error.
//!
//! Companion regression pin to `stream_end_reason.rs`'s
//! `pump_read_error_records_transport_failed`: that test already checks
//! the raw `RtpRecvTransport::recv_timeout` shape after a wire reset;
//! this file checks the SAME two scenarios (clean TEARDOWN vs. abortive
//! RST) one layer up, through `DemuxReceiver::recv_event`, which is what
//! the `MPSC_PUMP_DISCONNECTED` remap in `transport.rs`'s
//! `recv_bytes_inner` is actually for — a demux consumer must be able to
//! tell "the peer politely hung up" apart from "the wire broke" without
//! reaching for `end_reason()`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use tst_pipeline::{DemuxReceiver, ShellErrorKind};
use tst_rtp::RtspClient;

use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle};

/// Minimal SDP the hand-rolled server answers DESCRIBE with — PT=33
/// MP2T, same shape as `stream_end_reason.rs`'s copy.
const SDP_BODY: &[u8] = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst-rtp test\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

/// Block until a full request (through `CRLFCRLF`) has arrived and
/// return it as text.
fn read_request(sock: &mut TcpStream) -> String {
    let mut buf = vec![0u8; 8192];
    let mut total = String::new();
    while !total.contains("\r\n\r\n") {
        let n = sock.read(&mut buf).expect("read from client");
        assert!(n > 0, "client closed before a full request arrived");
        total.push_str(std::str::from_utf8(&buf[..n]).unwrap_or(""));
    }
    total
}

fn cseq_of(req: &str) -> String {
    req.to_ascii_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("cseq:").map(|v| v.trim().to_string()))
        .expect("request carries a CSeq header")
}

fn method_of(req: &str) -> &str {
    req.split_whitespace().next().unwrap_or("")
}

/// Answer DESCRIBE, then SETUP with `transport_line` under session id
/// `DEADBEEF`. Returns once SETUP's response has been written.
fn answer_describe_and_setup(sock: &mut TcpStream, transport_line: &str) {
    let req = read_request(sock);
    assert_eq!(method_of(&req), "DESCRIBE", "unexpected request: {req}");
    let cseq = cseq_of(&req);
    sock.write_all(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            SDP_BODY.len()
        )
        .as_bytes(),
    )
    .unwrap();
    sock.write_all(SDP_BODY).unwrap();

    let req = read_request(sock);
    assert_eq!(method_of(&req), "SETUP", "unexpected request: {req}");
    let cseq = cseq_of(&req);
    sock.write_all(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF;timeout=60\r\nTransport: {transport_line}\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
}

/// Force an abortive close: `SO_LINGER=0` makes the kernel send a TCP
/// RST instead of a FIN, so the peer's next read sees a hard error
/// rather than a clean EOF — see `stream_end_reason.rs`'s copy of this
/// helper for the full rationale.
fn force_reset(sock: TcpStream) {
    socket2::SockRef::from(&sock)
        .set_linger(Some(Duration::ZERO))
        .ok();
    drop(sock);
}

/// Clean TEARDOWN: `DemuxReceiver::recv_event` must return `Ok(None)`
/// (clean end-of-stream), not surface the pump's `Broken` disconnect. A
/// bounded persistent deadline is a safety net only, so a regression
/// here fails fast (Backpressure/Broken) instead of hanging the test.
#[test]
fn clean_teardown_ends_demux_receiver_cleanly() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(Duration::from_secs(5)));
    let mut demux = DemuxReceiver::new(transport);

    client.teardown().unwrap();

    let result = demux.recv_event();
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None) after a clean TEARDOWN, got {result:?}"
    );

    drop(client);
    drop(h);
}

/// Regression pin: an abortive RST (not a clean TEARDOWN) must still
/// surface as `TransportBroken` — the CleanTeardown remap must not
/// blanket-convert every pump disconnect into a clean end-of-stream.
/// Mirrors `stream_end_reason.rs`'s `pump_read_error_records_transport_failed`
/// one layer up (through `DemuxReceiver` instead of the raw transport).
#[test]
fn wire_reset_ends_demux_receiver_as_transport_broken() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Ready-signal handshake — see the identical comment in
    // `stream_end_reason.rs`'s `pump_read_error_records_transport_failed`:
    // resetting before the client has fully consumed the SETUP response
    // races that read; only reset after `into_recv_transport()` returns.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        answer_describe_and_setup(&mut sock, "RTP/AVP/TCP;unicast;interleaved=0-1");
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(Duration::from_secs(5)));
    ready_tx.send(()).unwrap();
    let mut demux = DemuxReceiver::new(transport);

    let result = demux.recv_event();
    match result {
        Err(e) => assert_eq!(
            e.kind,
            ShellErrorKind::TransportBroken,
            "expected TransportBroken after a wire reset, got kind {:?}",
            e.kind
        ),
        other => panic!("expected Err(TransportBroken), got {other:?}"),
    }

    server.join().unwrap();
    drop(client);
}
