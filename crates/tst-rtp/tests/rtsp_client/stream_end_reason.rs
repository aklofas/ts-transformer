//! `EndReason` recorded at every session-death site.
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
//! a clean FIN. Those scenarios use the hand-rolled blocking `std::net`
//! responder in `fixtures/raw_rtsp_server.rs`, which completes DESCRIBE +
//! SETUP and leaves the rest of the exchange to the test.

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use tst_core::transport::EndReason;
use tst_core::transport::TransportError;
use tst_rtp::{H264DepayConfig, RtspClient};

use crate::fixtures::raw_rtsp_server::{
    SDP_BODY, answer_describe_and_setup, cseq_of, force_reset, method_of, read_request,
};
use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle, extract_client_port};
use crate::fixtures::wait_for;

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
        matches!(reason, EndReason::CleanTeardown),
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
        let client_port = extract_client_port(&req).expect("SETUP request carries client_port=");
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
        matches!(transport.end_reason(), Some(EndReason::SessionExpired)),
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
        let client_port = extract_client_port(&req).expect("SETUP request carries client_port=");
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
        matches!(reason, EndReason::KeepaliveFailed { .. }),
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
        matches!(reason, EndReason::TransportFailed { .. }),
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
            Some(EndReason::TransportFailed { .. })
        ),
        "expected TransportFailed to discriminate the reset from a clean teardown, got {:?}",
        receiver.end_reason()
    );

    server.join().unwrap();
    drop(client);
}
