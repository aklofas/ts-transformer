//! A hand-rolled blocking RTSP *server* peer for client tests that need a
//! wire outcome the tokio loopback fixture (`rtsp_loopback_server`) has no
//! knob for: answering a keepalive with `454`, or an abortive close
//! (`SO_LINGER=0`, a TCP RST) instead of a clean FIN. It answers DESCRIBE +
//! SETUP and leaves the rest of the exchange to the test.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Minimal SDP the hand-rolled server answers DESCRIBE with — PT=33
/// MP2T, same shape as `fixtures::rtsp_loopback_server`'s default.
pub const SDP_BODY: &[u8] = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst-rtp test\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

/// Block until a full request (through `CRLFCRLF`) has arrived and
/// return it as text. `leftover` is a persistent per-connection buffer
/// the caller must reuse across every call on the same socket: a single
/// `read()` can return more than one pipelined request back-to-back
/// (e.g. a keepalive ping racing this test's own TEARDOWN write landing
/// in the same read), and this function only consumes and returns the
/// FIRST complete request from `leftover`, keeping any bytes past its
/// terminating `CRLFCRLF` buffered for the next call. Discarding that
/// surplus silently drops the second pipelined request, wedging the
/// server in a blocking read that never completes since the client
/// already sent everything it's going to send — a deadlock that hangs
/// the test (and the gating runner) rather than failing it.
pub fn read_request(sock: &mut TcpStream, leftover: &mut Vec<u8>) -> String {
    loop {
        if let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = pos + 4;
            let req = String::from_utf8_lossy(&leftover[..end]).into_owned();
            leftover.drain(..end);
            return req;
        }
        let mut buf = [0u8; 8192];
        let n = sock.read(&mut buf).expect("read from client");
        assert!(n > 0, "client closed before a full request arrived");
        leftover.extend_from_slice(&buf[..n]);
    }
}

pub fn cseq_of(req: &str) -> String {
    req.to_ascii_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("cseq:").map(|v| v.trim().to_string()))
        .expect("request carries a CSeq header")
}

pub fn method_of(req: &str) -> &str {
    req.split_whitespace().next().unwrap_or("")
}

/// Answer DESCRIBE, then SETUP with `transport_line` (the `Transport:`
/// response value) under session id `DEADBEEF`. Returns once SETUP's
/// response has been written. `leftover` is the same persistent buffer
/// `read_request` needs — see its doc.
pub fn answer_describe_and_setup(
    sock: &mut TcpStream,
    leftover: &mut Vec<u8>,
    transport_line: &str,
) {
    let req = read_request(sock, leftover);
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

    let req = read_request(sock, leftover);
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
/// RST instead of a FIN, so the peer's next read/write sees a hard error
/// (`ConnectionReset`) rather than a clean EOF. A clean drop (or a tokio
/// task returning, as the shared fixture's TEARDOWN handler does) cannot
/// produce this — it is exactly what `CleanTeardown` must NOT be
/// confused with. `std::net::TcpStream::set_linger` is still unstable
/// (`tcp_linger`, rust-lang/rust#88494) on the pinned toolchain, so this
/// goes through `socket2` (already an unconditional tst-rtp dependency).
pub fn force_reset(sock: TcpStream) {
    socket2::SockRef::from(&sock)
        .set_linger(Some(Duration::ZERO))
        .ok();
    drop(sock);
}
