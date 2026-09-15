//! CORR-25: a complete request the server cannot parse — an RTSP method we
//! don't implement, or a malformed request line — gets an answer (501 or
//! 400) and is drained, so the requests queued behind it are served.
//! Before the fix `RtspRequest::parse`'s error was treated as "need more
//! bytes": the bad request stayed at the head of the read buffer, every
//! later request piled up behind it, and the connection sat silent until
//! the 30 s idle timeout.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use tst_rtp::RtspServer;

/// Read one complete RTSP message head (through CRLFCRLF). Panics when
/// nothing arrives within the socket's 2 s read timeout — that silence is
/// the bug this file pins.
fn read_head(tcp: &mut TcpStream, what: &str) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tcp
            .read(&mut chunk)
            .unwrap_or_else(|e| panic!("no response to {what} within 2 s: {e}"));
        assert!(
            n > 0,
            "server closed the connection instead of answering {what}"
        );
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn connect(server: &RtspServer) -> TcpStream {
    let port = server.local_addr().unwrap().port();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_nodelay(true).unwrap();
    // Both timeouts before the first write (the oom_guard.rs macOS lesson).
    tcp.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    tcp
}

fn assert_status_and_cseq(head: &str, status: &str, cseq: &str) {
    assert!(
        head.starts_with(status),
        "expected a response starting with {status:?}, got:\n{head}"
    );
    let needle = format!("cseq: {cseq}\r\n");
    assert!(
        head.to_ascii_lowercase().contains(&needle),
        "response must echo CSeq {cseq}, got:\n{head}"
    );
}

/// `SET_PARAMETER` is a real RTSP method we do not implement → 501 with the
/// request's CSeq, and the OPTIONS behind it is served normally.
#[test]
fn unsupported_method_gets_501_and_the_next_request_is_served() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let mut tcp = connect(&server);

    tcp.write_all(b"SET_PARAMETER rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\n\r\n")
        .unwrap();
    let first = read_head(&mut tcp, "SET_PARAMETER");
    assert_status_and_cseq(&first, "RTSP/1.0 501 ", "1");

    tcp.write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 2\r\n\r\n")
        .unwrap();
    let second = read_head(&mut tcp, "OPTIONS after the rejected request");
    assert_status_and_cseq(&second, "RTSP/1.0 200 OK", "2");
    server.stop().ok();
}

/// A request line that is not `<METHOD> <uri> RTSP/x.y` → 400, CSeq still
/// echoed when it is a clean number, and the connection keeps serving.
#[test]
fn malformed_request_line_gets_400_and_the_next_request_is_served() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let mut tcp = connect(&server);

    tcp.write_all(b"NOT AN RTSP REQUEST LINE\r\nCSeq: 7\r\n\r\n")
        .unwrap();
    let first = read_head(&mut tcp, "the malformed request");
    assert_status_and_cseq(&first, "RTSP/1.0 400 ", "7");

    tcp.write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 8\r\n\r\n")
        .unwrap();
    let second = read_head(&mut tcp, "OPTIONS after the malformed request");
    assert_status_and_cseq(&second, "RTSP/1.0 200 OK", "8");
    server.stop().ok();
}
