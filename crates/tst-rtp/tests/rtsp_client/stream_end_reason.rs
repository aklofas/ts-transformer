//! `StreamEndReason` recorded at every session-death site.
//!
//! `clean_teardown_records_clean_teardown` uses the shared tokio fixture
//! (`fixtures::rtsp_loopback_server`) — its TEARDOWN handler already
//! closes the socket right after replying, which is exactly the
//! orderly-EOF shape `CleanTeardown` covers.
//!
//! The other four scenarios (454-answering keepalive, a control-TCP
//! write failure, a hard read failure on the interleaved pump, and the
//! same read failure observed through `H264Receiver`) need a peer that
//! can force a specific wire outcome the shared fixture's fixed tokio
//! protocol surface has no knob for: answering a keepalive with `454`,
//! or an abortive (`SO_LINGER=0`) close that sends a TCP RST instead of
//! a clean FIN. Rather than grow the shared fixture with single-use
//! knobs, this file hand-rolls a minimal blocking `std::net` responder —
//! the same style `rtsp_client/keepalive.rs` already uses for its
//! wire-format tests, extended just far enough to complete DESCRIBE +
//! SETUP.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tst_core::transport::TransportError;
use tst_rtp::{H264DepayConfig, RtspClient, StreamEndReason};

use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle};

/// Poll `f` until it returns `Some`, or panic after `budget`. All the
/// hand-rolled scenarios here race a background thread (the keepalive
/// pinger or the interleaved pump) against the test's assertion — this
/// is the shared wait-with-timeout shape so a regression fails fast
/// instead of hanging a CI runner.
fn wait_for<T>(budget: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= deadline {
            panic!("condition not met within {budget:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Minimal SDP the hand-rolled server answers DESCRIBE with — PT=33
/// MP2T, same shape as `fixtures::rtsp_loopback_server`'s default.
const SDP_BODY: &[u8] = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst-rtp test\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

/// Block until a full request (through `CRLFCRLF`) has arrived and
/// return it as text. `leftover` is a persistent per-connection buffer
/// the caller must reuse across every call on the same socket: a single
/// `read()` can return more than one pipelined request back-to-back
/// (e.g. a keepalive ping racing this test's own TEARDOWN write landing
/// in the same read), and this function only consumes and returns the
/// FIRST complete request from `leftover`, keeping any bytes past its
/// terminating `CRLFCRLF` buffered for the next call. Discarding that
/// surplus (the previous shape) silently drops the second pipelined
/// request, wedging the server in a blocking read that never completes
/// since the client already sent everything it's going to send — a
/// deadlock that hangs the test (and the gating runner) rather than
/// failing it.
fn read_request(sock: &mut TcpStream, leftover: &mut Vec<u8>) -> String {
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

fn cseq_of(req: &str) -> String {
    req.to_ascii_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("cseq:").map(|v| v.trim().to_string()))
        .expect("request carries a CSeq header")
}

fn method_of(req: &str) -> &str {
    req.split_whitespace().next().unwrap_or("")
}

/// Answer DESCRIBE, then SETUP with `transport_line` (the `Transport:`
/// response value) under session id `DEADBEEF`. Returns once SETUP's
/// response has been written. `leftover` is the same persistent buffer
/// `read_request` needs — see its doc.
fn answer_describe_and_setup(sock: &mut TcpStream, leftover: &mut Vec<u8>, transport_line: &str) {
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

/// The `client_port=` value from a `Transport:` request header — needed
/// to build a UDP SETUP response the client's negotiation accepts.
fn extract_client_port(transport: &str) -> u16 {
    transport
        .split(';')
        .find_map(|p| p.trim().strip_prefix("client_port="))
        .and_then(|v| v.split('-').next())
        .and_then(|s| s.parse().ok())
        .expect("SETUP request carries client_port=")
}

/// Force an abortive close: `SO_LINGER=0` makes the kernel send a TCP
/// RST instead of a FIN, so the peer's next read/write sees a hard error
/// (`ConnectionReset`) rather than a clean EOF. A clean drop (or a tokio
/// task returning, as the shared fixture's TEARDOWN handler does) cannot
/// produce this — it is exactly what `CleanTeardown` must NOT be
/// confused with. `std::net::TcpStream::set_linger` is still unstable
/// (`tcp_linger`, rust-lang/rust#88494) on the pinned toolchain, so this
/// goes through `socket2` (already an unconditional tst-rtp dependency).
fn force_reset(sock: TcpStream) {
    socket2::SockRef::from(&sock)
        .set_linger(Some(Duration::ZERO))
        .ok();
    drop(sock);
}

/// Clean TEARDOWN: the fixture's TEARDOWN handler writes the 200 OK then
/// returns, ending the tokio task and closing the socket — the still
/// running pump sees `Ok(0)` on its next read within one ~100 ms cycle.
#[test]
fn clean_teardown_records_clean_teardown() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let transport = session.into_recv_transport();

    assert!(
        transport.end_reason().is_none(),
        "must be None before anything has happened"
    );

    client.teardown().unwrap();

    let reason = wait_for(Duration::from_secs(3), || transport.end_reason());
    assert!(
        matches!(reason, StreamEndReason::CleanTeardown),
        "expected CleanTeardown after the peer's orderly close, got {reason:?}"
    );

    drop(client);
    drop(h);
}

/// A keepalive ping answered `454 Session Not Found` must record
/// `SessionExpired`. Non-pump (UDP) session: keepalive responses are only
/// consumed at the next explicit read (`send_and_read`), so the test
/// calls `teardown()` after the ping to drain it — see
/// `options_describe.rs`'s keepalive-consumption loop.
#[test]
fn keepalive_454_records_session_expired() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "DESCRIBE");
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

        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "SETUP");
        let cseq = cseq_of(&req);
        let client_port = extract_client_port(&req);
        sock.write_all(
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF;timeout=60\r\n\
                 Transport: RTP/AVP;unicast;client_port={client_port}-{};server_port=6970-6971\r\n\r\n",
                client_port + 1
            )
            .as_bytes(),
        )
        .unwrap();

        // Answer every keepalive ping (the pinger keeps going at its
        // configured interval regardless of 454s) with 454, until the
        // test's TEARDOWN arrives. A ping and the test's own TEARDOWN can
        // land in the SAME read() back-to-back — `leftover` (reused
        // across every call in this connection, not reset per
        // iteration) is what keeps the second request from being
        // silently dropped; see `read_request`'s doc.
        loop {
            let req = read_request(&mut sock, &mut leftover);
            let cseq = cseq_of(&req);
            match method_of(&req) {
                "OPTIONS" => {
                    sock.write_all(
                        format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {cseq}\r\n\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
                }
                "TEARDOWN" => {
                    sock.write_all(
                        format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
                    return;
                }
                other => panic!("unexpected request method: {other}"),
            }
        }
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=udp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let transport = session.into_recv_transport();
    client
        .spawn_keepalive_if_needed(Some(Duration::from_millis(50)))
        .unwrap();

    // Let the ping-then-454 round trip happen before draining it.
    std::thread::sleep(Duration::from_millis(300));
    client.teardown().unwrap();

    assert!(
        matches!(
            transport.end_reason(),
            Some(StreamEndReason::SessionExpired)
        ),
        "expected SessionExpired after a 454 keepalive answer, got {:?}",
        transport.end_reason()
    );

    server.join().unwrap();
    drop(client);
}

/// A control-TCP write failure inside the keepalive thread — the
/// formerly-silent site — must record `KeepaliveFailed`. UDP (non-pump)
/// session so the pump can't race the keepalive thread for the slot: the
/// server forces an abortive close right after SETUP and never reads
/// again, so ONLY the keepalive thread's own write can observe the
/// failure (nothing else touches the control TCP).
#[test]
fn keepalive_write_failure_records_keepalive_failed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // A TCP RST purges the peer's receive queue — resetting before the
    // client has finished reading the SETUP response races the client's
    // own read of that response (setup_mp2t_auto() can then see the
    // reset instead of the 200 OK). Signal readiness only after the
    // client-side conversion has returned, so the reset always lands
    // strictly after SETUP was fully consumed.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "DESCRIBE");
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

        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "SETUP");
        let cseq = cseq_of(&req);
        let client_port = extract_client_port(&req);
        sock.write_all(
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF;timeout=60\r\n\
                 Transport: RTP/AVP;unicast;client_port={client_port}-{};server_port=6970-6971\r\n\r\n",
                client_port + 1
            )
            .as_bytes(),
        )
        .unwrap();

        // Abortive close: the client's next keepalive write hits a reset
        // connection. Nothing reads again on this socket.
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=udp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let transport = session.into_recv_transport();
    ready_tx.send(()).unwrap();
    client
        .spawn_keepalive_if_needed(Some(Duration::from_millis(50)))
        .unwrap();

    let reason = wait_for(Duration::from_secs(5), || transport.end_reason());
    assert!(
        matches!(reason, StreamEndReason::KeepaliveFailed { .. }),
        "expected KeepaliveFailed after the control TCP reset, got {reason:?}"
    );

    server.join().unwrap();
    drop(client);
}

/// TCP-interleaved SETUP activates the pump before PLAY — an abortive
/// close right after SETUP's response is a hard read failure on the
/// SAME thread that also owns the pump's own cancel/close accounting,
/// so this exercises `:235` (`TCP read failed`) without any PLAY data.
#[test]
fn pump_read_error_records_transport_failed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // See the ready-signal comment in `keepalive_write_failure_records_keepalive_failed`
    // — a TCP RST purges the peer's receive queue, so resetting before
    // the client's own read of the SETUP response completes races that
    // read. Only reset after `into_recv_transport()` has returned.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    ready_tx.send(()).unwrap();

    let reason = wait_for(Duration::from_secs(3), || transport.end_reason());
    assert!(
        matches!(reason, StreamEndReason::TransportFailed { .. }),
        "expected TransportFailed after the reset, got {reason:?}"
    );

    // The mpsc bridge to the (now-dead) pump surfaces the disconnect as
    // Broken on the next recv — the recorded reason above is what lets a
    // caller tell this apart from a clean teardown.
    let mut buf = vec![0u8; 2048];
    let result = transport.recv_timeout(&mut buf, Duration::from_secs(2));
    assert!(
        matches!(result, Err(TransportError::Broken { .. })),
        "expected Broken once the pump's Sender drops, got {result:?}"
    );

    server.join().unwrap();
    drop(client);
}

/// The H.264 path variant of the previous test: after the pump dies,
/// `recv_au()` surfaces the usual `Ok(None)` clean-EOS shape — but
/// `end_reason()` lets the caller tell a genuine wire failure apart from
/// an orderly teardown, which `Ok(None)` alone cannot.
#[test]
fn h264_pump_death_records_transport_failed_and_recv_au_returns_none() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // See the ready-signal comment in `keepalive_write_failure_records_keepalive_failed`
    // — only reset after `into_h264_receiver()` has returned, so the RST
    // can't race the client's own read of the SETUP response.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    // H264DepayConfig is non-exhaustive — default-and-assign.
    let mut h264_config = H264DepayConfig::default();
    h264_config.payload_type = 96;
    let mut receiver = session.into_h264_receiver(h264_config);
    ready_tx.send(()).unwrap();

    // recv_au() blocks until the pump's Sender drops (post-reset), then
    // resolves via the normal flush-then-EOS path — Ok(None), the SAME
    // shape a clean teardown produces.
    let result = receiver.recv_au();
    assert!(
        matches!(result, Ok(None)),
        "expected clean-EOS shape Ok(None), got {result:?}"
    );
    assert!(
        matches!(
            receiver.end_reason(),
            Some(StreamEndReason::TransportFailed { .. })
        ),
        "expected TransportFailed to discriminate the reset from a clean teardown, got {:?}",
        receiver.end_reason()
    );

    server.join().unwrap();
    drop(client);
}
